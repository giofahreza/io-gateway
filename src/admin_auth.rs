use axum::http::{header, HeaderMap, HeaderValue};
use data_encoding::BASE32_NOPAD;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha1::Sha1;
use sha2::Sha256;
use std::{
    collections::{HashMap, VecDeque},
    fs::{self, File, OpenOptions},
    io::{self, Write},
    net::{IpAddr, SocketAddr},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};
use uuid::Uuid;

const ADMIN_SESSION_COOKIE: &str = "io_gateway_admin_session";
const TOTP_STEP_SECONDS: u64 = 30;
const TOTP_WINDOW_STEPS: i64 = 1;
const TOTP_DIGITS: u32 = 6;
const DEFAULT_SESSION_TTL_SECONDS: u64 = 12 * 60 * 60;
const MIN_SESSION_TTL_SECONDS: u64 = 300;
const MAX_SESSION_TTL_SECONDS: u64 = 7 * 24 * 60 * 60;
const LOGIN_FAILURE_WINDOW_SECONDS: u64 = 5 * 60;
const LOGIN_FAILURE_THRESHOLD: usize = 3;
const LOGIN_BASE_LOCKOUT_SECONDS: u64 = 5 * 60;
const LOGIN_MAX_BACKOFF_SHIFT: u32 = 10;
// Changing this invalidates every persisted admin session.  Keep it separate
// from the cookie name so an authentication-policy upgrade cannot silently
// inherit sessions minted under an older policy.
const ADMIN_SESSION_AUTH_CONTEXT_VERSION: u8 = 1;
const ADMIN_SESSION_AUTH_CONTEXT_DOMAIN: &[u8] = b"io-gateway/admin-session-auth-context/v1";
// This is a domain separator, not a secret.  `verify_slice` performs the
// fixed-size MAC comparison in constant time, which is what we need when
// comparing a supplied administrator key with the configured one.
const ADMIN_API_KEY_COMPARISON_DOMAIN: &[u8] = b"io-gateway/admin-api-key-compare/v1";

type HmacSha1 = Hmac<Sha1>;
type HmacSha256 = Hmac<Sha256>;

#[derive(Debug, Deserialize, Default, Clone)]
pub(crate) struct AdminAuthConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default)]
    pub totp_secret: Option<String>,
    #[serde(default)]
    pub session_ttl_seconds: Option<u64>,
    #[serde(default)]
    pub secure_cookies: bool,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct AdminSession {
    pub expires_at_unix: u64,
    /// An authenticated-session context bound to the configured credentials
    /// and authentication policy.  Older persisted sessions deserialize with
    /// the defaults below and are intentionally rejected on their next use.
    #[serde(default)]
    pub auth_context_version: u8,
    #[serde(default)]
    pub auth_context: String,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct LoginAttemptState {
    recent_failures_unix: VecDeque<u64>,
    lockout_level: u32,
    locked_until_unix: Option<u64>,
}

#[derive(Deserialize)]
pub(crate) struct LoginForm {
    pub otp: String,
    #[serde(default)]
    pub api_key: Option<String>,
}

pub(crate) fn apply_env_overrides(cfg: &mut AdminAuthConfig) {
    if let Some(value) = env_value(&["ADMIN_AUTH_API_KEY", "ADMIN_API_KEY"]) {
        cfg.api_key = Some(value);
    }
    if let Some(value) = env_value(&["ADMIN_AUTH_TOTP_SECRET", "ADMIN_TOTP_SECRET"]) {
        cfg.totp_secret = Some(value);
    }
    if let Some(value) = env_value(&["ADMIN_AUTH_ENABLED"]) {
        cfg.enabled = parse_bool(&value).unwrap_or(cfg.enabled);
    }
    if let Some(value) = env_value(&["ADMIN_AUTH_SESSION_TTL_SECONDS"]) {
        if let Ok(parsed) = value.parse::<u64>() {
            cfg.session_ttl_seconds = Some(parsed);
        }
    }
    if let Some(value) = env_value(&["ADMIN_AUTH_SECURE_COOKIES"]) {
        cfg.secure_cookies = parse_bool(&value).unwrap_or(cfg.secure_cookies);
    }
}

pub(crate) fn is_enabled(cfg: &AdminAuthConfig) -> bool {
    cfg.enabled || configured_totp_secret(cfg).is_some() || configured_admin_api_key(cfg).is_some()
}

pub(crate) fn is_configured(cfg: &AdminAuthConfig) -> bool {
    configured_totp_secret(cfg).is_some()
}

/// Returns whether a successful admin login must also prove knowledge of the
/// configured administrator API key.  TOTP remains required in all modes.
pub(crate) fn requires_api_key(cfg: &AdminAuthConfig) -> bool {
    configured_admin_api_key(cfg).is_some()
}

pub(crate) fn session_ttl_seconds(cfg: &AdminAuthConfig) -> u64 {
    cfg.session_ttl_seconds
        .unwrap_or(DEFAULT_SESSION_TTL_SECONDS)
        .clamp(MIN_SESSION_TTL_SECONDS, MAX_SESSION_TTL_SECONDS)
}

pub(crate) fn verify_login(
    cfg: &AdminAuthConfig,
    otp: &str,
    api_key: Option<&str>,
    now: SystemTime,
) -> Result<(), String> {
    if !is_enabled(cfg) {
        return Err("admin login is not enabled".to_string());
    }
    if !is_configured(cfg) {
        return Err(
            "admin login is not configured: set admin_auth.totp_secret or ADMIN_AUTH_TOTP_SECRET"
                .to_string(),
        );
    }
    let secret = configured_totp_secret(cfg).ok_or_else(|| {
        "admin login is not configured: set admin_auth.totp_secret or ADMIN_AUTH_TOTP_SECRET"
            .to_string()
    })?;

    // Check both factors before deciding.  Besides making failures
    // indistinguishable to callers, this avoids turning OTP success into an
    // oracle for the optional administrator key.
    let otp_valid = verify_totp(secret, otp, now);
    let api_key_valid = configured_admin_api_key(cfg)
        .map(|expected| verify_admin_api_key(expected, api_key))
        .unwrap_or(true);
    if !otp_valid || !api_key_valid {
        return Err("invalid administrator credentials".to_string());
    }
    Ok(())
}

fn configured_admin_api_key(cfg: &AdminAuthConfig) -> Option<&str> {
    cfg.api_key
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn configured_totp_secret(cfg: &AdminAuthConfig) -> Option<&str> {
    cfg.totp_secret
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn verify_admin_api_key(expected: &str, supplied: Option<&str>) -> bool {
    let supplied = supplied.map(str::trim).unwrap_or_default();

    // HMAC tags are a fixed size, and `Mac::verify_slice` uses a constant-time
    // tag comparison.  Comparing tags instead of the raw strings avoids a
    // length-dependent comparison of the configured administrator key.
    let mut expected_mac = HmacSha256::new_from_slice(ADMIN_API_KEY_COMPARISON_DOMAIN)
        .expect("fixed HMAC domain is valid");
    expected_mac.update(expected.as_bytes());
    let expected_tag = expected_mac.finalize().into_bytes();

    let mut supplied_mac = HmacSha256::new_from_slice(ADMIN_API_KEY_COMPARISON_DOMAIN)
        .expect("fixed HMAC domain is valid");
    supplied_mac.update(supplied.as_bytes());
    supplied_mac.verify_slice(expected_tag.as_slice()).is_ok()
}

fn session_auth_context_fingerprint(cfg: &AdminAuthConfig) -> String {
    // Do not store credential material itself in admin-sessions.json. The
    // session file is mode 0600 on Unix and contains bearer session IDs, so it
    // must remain protected even though this fingerprint is opaque.
    let mut key_material = Vec::new();
    append_auth_context_component(&mut key_material, configured_admin_api_key(cfg));
    append_auth_context_component(&mut key_material, configured_totp_secret(cfg));

    let mut mac = HmacSha256::new_from_slice(&key_material)
        .expect("HMAC accepts administrator credential material of any length");
    mac.update(ADMIN_SESSION_AUTH_CONTEXT_DOMAIN);
    mac.update(&[ADMIN_SESSION_AUTH_CONTEXT_VERSION]);
    let tag = mac.finalize().into_bytes();
    tag.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn append_auth_context_component(buffer: &mut Vec<u8>, value: Option<&str>) {
    match value {
        Some(value) => {
            buffer.push(1);
            let length = u64::try_from(value.len()).unwrap_or(u64::MAX);
            buffer.extend_from_slice(&length.to_be_bytes());
            buffer.extend_from_slice(value.as_bytes());
        }
        None => buffer.push(0),
    }
}

/// Returns a stable identity for rate limiting administrator login failures.
///
/// A User-Agent is not an identity: clients can vary it at no cost, which
/// would make a per-User-Agent login lockout ineffective. Prefer a sanitized
/// forwarding address only when the operator explicitly trusts the proxy;
/// otherwise use the TCP peer address installed by the server. If neither is
/// available (for example an embedded router), use one shared bucket rather
/// than trusting any request-controlled header.
pub(crate) fn login_client_key(
    headers: &HeaderMap,
    trust_forwarded_headers: bool,
    peer_addr: Option<SocketAddr>,
) -> String {
    if trust_forwarded_headers {
        if let Some(forwarded_ip) = forwarded_client_ip(headers) {
            return format!("forwarded-ip:{forwarded_ip}");
        }
    }

    match peer_addr {
        Some(peer_addr) => format!("peer-ip:{}", peer_addr.ip()),
        None => "global".to_string(),
    }
}

fn forwarded_client_ip(headers: &HeaderMap) -> Option<IpAddr> {
    [
        "cf-connecting-ip",
        "true-client-ip",
        "x-real-ip",
        "x-forwarded-for",
    ]
    .iter()
    .find_map(|header_name| {
        headers.get(*header_name).and_then(|value| {
            value.to_str().ok().and_then(|raw| {
                raw.split(',')
                    .next()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .and_then(|value| value.parse::<IpAddr>().ok())
            })
        })
    })
}

pub(crate) fn current_lockout_message(
    attempts: &mut HashMap<String, LoginAttemptState>,
    client_key: &str,
    now: SystemTime,
) -> Option<String> {
    let now_unix = unix_seconds(now);
    let Some(state) = attempts.get_mut(client_key) else {
        return None;
    };
    prune_attempt_state(state, now_unix);
    let locked_until = state.locked_until_unix?;
    if locked_until <= now_unix {
        state.locked_until_unix = None;
        prune_attempt_state(state, now_unix);
        return None;
    }
    Some(lockout_message(locked_until.saturating_sub(now_unix)))
}

pub(crate) fn record_failed_login(
    attempts: &mut HashMap<String, LoginAttemptState>,
    client_key: &str,
    now: SystemTime,
) -> Option<String> {
    let now_unix = unix_seconds(now);
    let state = attempts.entry(client_key.to_string()).or_default();
    prune_attempt_state(state, now_unix);
    if let Some(locked_until) = state.locked_until_unix {
        if locked_until > now_unix {
            return Some(lockout_message(locked_until.saturating_sub(now_unix)));
        }
        state.locked_until_unix = None;
    }
    state.recent_failures_unix.push_back(now_unix);
    prune_attempt_state(state, now_unix);
    if state.recent_failures_unix.len() < LOGIN_FAILURE_THRESHOLD {
        return None;
    }
    state.recent_failures_unix.clear();
    let shift = state.lockout_level.min(LOGIN_MAX_BACKOFF_SHIFT);
    let lockout_seconds = LOGIN_BASE_LOCKOUT_SECONDS.saturating_mul(1u64 << shift);
    state.lockout_level = state.lockout_level.saturating_add(1);
    state.locked_until_unix = Some(now_unix.saturating_add(lockout_seconds));
    Some(lockout_message(lockout_seconds))
}

pub(crate) fn clear_login_attempts(
    attempts: &mut HashMap<String, LoginAttemptState>,
    client_key: &str,
) {
    attempts.remove(client_key);
}

pub(crate) fn create_session(
    sessions: &mut HashMap<String, AdminSession>,
    ttl_seconds: u64,
    cfg: &AdminAuthConfig,
) -> String {
    prune_expired_sessions(sessions);
    let session_id = Uuid::new_v4().simple().to_string();
    let expires_at_unix = now_unix_seconds().saturating_add(ttl_seconds);
    sessions.insert(
        session_id.clone(),
        AdminSession {
            expires_at_unix,
            auth_context_version: ADMIN_SESSION_AUTH_CONTEXT_VERSION,
            auth_context: session_auth_context_fingerprint(cfg),
        },
    );
    session_id
}

pub(crate) fn validate_session(
    headers: &HeaderMap,
    sessions: &mut HashMap<String, AdminSession>,
    cfg: &AdminAuthConfig,
) -> bool {
    prune_expired_sessions(sessions);
    let Some(session_id) = read_cookie_value(headers, ADMIN_SESSION_COOKIE) else {
        return false;
    };
    let expected_auth_context = session_auth_context_fingerprint(cfg);
    match sessions.get(&session_id) {
        Some(session)
            if session.expires_at_unix > now_unix_seconds()
                && session.auth_context_version == ADMIN_SESSION_AUTH_CONTEXT_VERSION
                && !session.auth_context.is_empty()
                && timing_safe_eq(&session.auth_context, &expected_auth_context) =>
        {
            true
        }
        _ => {
            sessions.remove(&session_id);
            false
        }
    }
}

pub(crate) fn remove_session(headers: &HeaderMap, sessions: &mut HashMap<String, AdminSession>) {
    if let Some(session_id) = read_cookie_value(headers, ADMIN_SESSION_COOKIE) {
        sessions.remove(&session_id);
    }
    prune_expired_sessions(sessions);
}

pub(crate) fn build_session_cookie(session_id: &str, ttl_seconds: u64, secure: bool) -> String {
    let secure_suffix = if secure { "; Secure" } else { "" };
    format!(
        "{}={}; Path=/; Max-Age={}; HttpOnly; SameSite=Lax{}",
        ADMIN_SESSION_COOKIE, session_id, ttl_seconds, secure_suffix
    )
}

pub(crate) fn clear_session_cookie(secure: bool) -> String {
    let secure_suffix = if secure { "; Secure" } else { "" };
    format!(
        "{}=; Path=/; Max-Age=0; HttpOnly; SameSite=Lax{}",
        ADMIN_SESSION_COOKIE, secure_suffix
    )
}

pub(crate) fn append_set_cookie(headers: &mut axum::http::HeaderMap, cookie: &str) {
    if let Ok(value) = HeaderValue::from_str(cookie) {
        headers.append(header::SET_COOKIE, value);
    }
}

pub(crate) fn load_sessions(path: &Path) -> HashMap<String, AdminSession> {
    let Ok(data) = fs::read_to_string(path) else {
        return HashMap::new();
    };
    let Ok(mut sessions) = serde_json::from_str::<HashMap<String, AdminSession>>(&data) else {
        return HashMap::new();
    };
    prune_expired_sessions(&mut sessions);
    sessions
}

/// Durably save bearer admin sessions without ever exposing a partially
/// written JSON file.  The temporary file lives in the target directory so
/// the rename is atomic on the same filesystem.  On Unix it is created 0600
/// before any session material is written, then both it and the containing
/// directory are fsynced around the rename.
pub(crate) fn save_sessions(
    path: &Path,
    sessions: &HashMap<String, AdminSession>,
) -> Result<(), String> {
    let parent = session_parent_dir(path);
    fs::create_dir_all(parent).map_err(|err| {
        format!(
            "failed to create admin session directory '{}': {err}",
            parent.display()
        )
    })?;
    let data = serde_json::to_vec_pretty(sessions)
        .map_err(|err| format!("failed to serialize admin sessions: {err}"))?;
    let (tmp_path, mut tmp_file) = create_session_temp_file(parent, path).map_err(|err| {
        format!(
            "failed to create temporary admin session file in '{}': {err}",
            parent.display()
        )
    })?;

    let result = (|| -> io::Result<()> {
        tmp_file.write_all(&data)?;
        tmp_file.sync_all()?;
        drop(tmp_file);
        fs::rename(&tmp_path, path)?;
        // A directory fsync failure happens after the rename is already
        // visible. Returning it as an ordinary save failure would cause the
        // caller to roll back its in-memory session map while a restart loads
        // the new on-disk session state. Keep the two authorities coherent and
        // make the reduced crash-durability visible in logs instead.
        if let Err(err) = sync_directory(parent) {
            tracing::warn!(
                path = %path.display(),
                error = %err,
                "admin session file was renamed but its parent directory could not be fsynced"
            );
        }
        Ok(())
    })();

    if let Err(err) = result {
        // If rename already succeeded this is harmless; if it did not, do
        // not leave session data in a stale temporary file.
        let _ = fs::remove_file(&tmp_path);
        return Err(format!(
            "failed to durably save admin sessions to '{}': {err}",
            path.display()
        ));
    }
    Ok(())
}

fn session_parent_dir(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

fn create_session_temp_file(parent: &Path, path: &Path) -> io::Result<(PathBuf, File)> {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("admin-sessions.json");
    let tmp_path = parent.join(format!(".{name}.{}.tmp", Uuid::new_v4().simple()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(&tmp_path)?;
    Ok((tmp_path, file))
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> io::Result<()> {
    Ok(())
}

fn verify_totp(secret: &str, otp: &str, now: SystemTime) -> bool {
    let otp = otp.trim();
    if otp.len() != TOTP_DIGITS as usize || !otp.chars().all(|c| c.is_ascii_digit()) {
        return false;
    }
    let Some(secret_bytes) = decode_totp_secret(secret) else {
        return false;
    };
    let Ok(duration) = now.duration_since(UNIX_EPOCH) else {
        return false;
    };
    let step = duration.as_secs() / TOTP_STEP_SECONDS;
    for delta in -TOTP_WINDOW_STEPS..=TOTP_WINDOW_STEPS {
        let candidate_step = step as i64 + delta;
        if candidate_step < 0 {
            continue;
        }
        if timing_safe_eq(otp, &hotp(&secret_bytes, candidate_step as u64)) {
            return true;
        }
    }
    false
}

fn hotp(secret: &[u8], counter: u64) -> String {
    let mut mac = HmacSha1::new_from_slice(secret).expect("valid hmac key length");
    mac.update(&counter.to_be_bytes());
    let result = mac.finalize().into_bytes();
    let offset = (result[19] & 0x0f) as usize;
    let binary = ((u32::from(result[offset]) & 0x7f) << 24)
        | (u32::from(result[offset + 1]) << 16)
        | (u32::from(result[offset + 2]) << 8)
        | u32::from(result[offset + 3]);
    format!("{:06}", binary % 10u32.pow(TOTP_DIGITS))
}

fn decode_totp_secret(secret: &str) -> Option<Vec<u8>> {
    let normalized = secret
        .chars()
        .filter(|ch| !matches!(ch, ' ' | '-' | '='))
        .flat_map(|ch| ch.to_uppercase())
        .collect::<String>();
    if normalized.is_empty() {
        return None;
    }
    BASE32_NOPAD.decode(normalized.as_bytes()).ok()
}

fn read_cookie_value(headers: &HeaderMap, target_name: &str) -> Option<String> {
    let cookies = headers.get(header::COOKIE)?.to_str().ok()?;
    cookies.split(';').find_map(|cookie| {
        let (name, value) = cookie.trim().split_once('=')?;
        if name.trim() == target_name {
            Some(value.trim().to_string())
        } else {
            None
        }
    })
}

fn prune_expired_sessions(sessions: &mut HashMap<String, AdminSession>) {
    let now = now_unix_seconds();
    sessions.retain(|_, session| session.expires_at_unix > now);
}

fn prune_attempt_state(state: &mut LoginAttemptState, now_unix: u64) {
    while let Some(oldest) = state.recent_failures_unix.front().copied() {
        if now_unix.saturating_sub(oldest) > LOGIN_FAILURE_WINDOW_SECONDS {
            state.recent_failures_unix.pop_front();
        } else {
            break;
        }
    }
    if state
        .locked_until_unix
        .is_some_and(|locked_until| locked_until <= now_unix)
    {
        state.locked_until_unix = None;
    }
}

fn now_unix_seconds() -> u64 {
    unix_seconds(SystemTime::now())
}

fn unix_seconds(now: SystemTime) -> u64 {
    now.duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn timing_safe_eq(left: &str, right: &str) -> bool {
    let left = left.as_bytes();
    let right = right.as_bytes();
    let mut diff = left.len() ^ right.len();
    let max_len = left.len().max(right.len());
    for idx in 0..max_len {
        diff |= usize::from(*left.get(idx).unwrap_or(&0) ^ *right.get(idx).unwrap_or(&0));
    }
    diff == 0
}

fn env_value(keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        std::env::var(key).ok().and_then(|value| {
            let trimmed = value.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        })
    })
}

fn parse_bool(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

fn lockout_message(remaining_seconds: u64) -> String {
    format!(
        "Too many failed login attempts. Try again in {}.",
        human_duration(remaining_seconds)
    )
}

fn human_duration(seconds: u64) -> String {
    let rounded = seconds.max(1);
    let hours = rounded / 3600;
    let minutes = (rounded % 3600) / 60;
    let secs = rounded % 60;
    if hours > 0 {
        if minutes > 0 {
            format!("{}h {}m", hours, minutes)
        } else {
            format!("{}h", hours)
        }
    } else if minutes > 1 {
        if secs > 0 {
            format!("{}m {}s", minutes, secs)
        } else {
            format!("{} minutes", minutes)
        }
    } else if minutes == 1 {
        if secs > 0 {
            format!("1m {}s", secs)
        } else {
            "1 minute".to_string()
        }
    } else {
        format!("{}s", secs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config(api_key: Option<&str>) -> AdminAuthConfig {
        AdminAuthConfig {
            enabled: true,
            api_key: api_key.map(ToOwned::to_owned),
            totp_secret: Some("GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ".to_string()),
            session_ttl_seconds: None,
            secure_cookies: false,
        }
    }

    #[test]
    fn verify_login_accepts_rfc_totp_vector() {
        let cfg = test_config(Some("admin-key"));

        let now = UNIX_EPOCH + std::time::Duration::from_secs(59);
        assert!(verify_login(&cfg, "287082", Some("admin-key"), now).is_ok());
    }

    #[test]
    fn verify_login_rejects_wrong_totp() {
        let cfg = test_config(Some("admin-key"));

        let now = UNIX_EPOCH + std::time::Duration::from_secs(59);
        assert!(verify_login(&cfg, "000000", Some("admin-key"), now).is_err());
    }

    #[test]
    fn verify_login_requires_configured_admin_api_key() {
        let cfg = test_config(Some("admin-key"));
        let now = UNIX_EPOCH + std::time::Duration::from_secs(59);

        assert!(verify_login(&cfg, "287082", None, now).is_err());
        assert!(verify_login(&cfg, "287082", Some("wrong-key"), now).is_err());
        assert!(verify_login(&cfg, "287082", Some("admin-key"), now).is_ok());
    }

    #[test]
    fn verify_login_remains_otp_only_without_admin_api_key() {
        let cfg = test_config(None);
        let now = UNIX_EPOCH + std::time::Duration::from_secs(59);

        assert!(!requires_api_key(&cfg));
        assert!(verify_login(&cfg, "287082", None, now).is_ok());
    }

    #[test]
    fn configured_admin_key_without_totp_fails_closed() {
        let cfg = AdminAuthConfig {
            enabled: false,
            api_key: Some("admin-key".to_string()),
            totp_secret: None,
            session_ttl_seconds: None,
            secure_cookies: false,
        };

        assert!(is_enabled(&cfg));
        assert!(!is_configured(&cfg));
        assert!(verify_login(&cfg, "287082", Some("admin-key"), UNIX_EPOCH).is_err());
    }

    #[test]
    fn session_cookie_round_trip_validates_and_clears() {
        let cfg = test_config(Some("admin-key"));
        let mut sessions = HashMap::new();
        let session_id = create_session(&mut sessions, 600, &cfg);
        let cookie = build_session_cookie(&session_id, 600, false);
        let cookie_pair = cookie.split(';').next().unwrap_or_default();

        let mut headers = HeaderMap::new();
        headers.insert(header::COOKIE, HeaderValue::from_str(cookie_pair).unwrap());

        assert!(validate_session(&headers, &mut sessions, &cfg));
        remove_session(&headers, &mut sessions);
        assert!(!validate_session(&headers, &mut sessions, &cfg));
    }

    #[test]
    fn changing_admin_credentials_invalidates_existing_sessions() {
        let original_cfg = test_config(Some("admin-key"));
        let changed_cfg = test_config(Some("rotated-admin-key"));
        let mut sessions = HashMap::new();
        let session_id = create_session(&mut sessions, 600, &original_cfg);
        let cookie = build_session_cookie(&session_id, 600, false);
        let cookie_pair = cookie.split(';').next().unwrap_or_default();
        let mut headers = HeaderMap::new();
        headers.insert(header::COOKIE, HeaderValue::from_str(cookie_pair).unwrap());

        assert!(!validate_session(&headers, &mut sessions, &changed_cfg));
        assert!(!sessions.contains_key(&session_id));
    }

    #[test]
    fn changing_totp_secret_invalidates_existing_sessions() {
        let original_cfg = test_config(Some("admin-key"));
        let mut changed_cfg = original_cfg.clone();
        changed_cfg.totp_secret = Some("JBSWY3DPEHPK3PXP".to_string());
        let mut sessions = HashMap::new();
        let session_id = create_session(&mut sessions, 600, &original_cfg);
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_str(&format!("{ADMIN_SESSION_COOKIE}={session_id}")).unwrap(),
        );

        assert!(!validate_session(&headers, &mut sessions, &changed_cfg));
        assert!(sessions.is_empty());
    }

    #[test]
    fn enabling_admin_api_key_requirement_invalidates_otp_only_sessions() {
        let otp_only_cfg = test_config(None);
        let key_and_otp_cfg = test_config(Some("admin-key"));
        let mut sessions = HashMap::new();
        let session_id = create_session(&mut sessions, 600, &otp_only_cfg);
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_str(&format!("{ADMIN_SESSION_COOKIE}={session_id}")).unwrap(),
        );

        assert!(!validate_session(&headers, &mut sessions, &key_and_otp_cfg));
        assert!(sessions.is_empty());
    }

    #[test]
    fn legacy_sessions_without_auth_context_are_rejected() {
        let cfg = test_config(None);
        let session_id = "legacy-session".to_string();
        let mut sessions = HashMap::from([(
            session_id.clone(),
            AdminSession {
                expires_at_unix: now_unix_seconds().saturating_add(600),
                auth_context_version: 0,
                auth_context: String::new(),
            },
        )]);
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_str(&format!("{ADMIN_SESSION_COOKIE}={session_id}")).unwrap(),
        );

        assert!(!validate_session(&headers, &mut sessions, &cfg));
        assert!(sessions.is_empty());
    }

    #[test]
    fn durable_session_save_round_trips_with_restrictive_permissions() {
        let directory =
            std::env::temp_dir().join(format!("io-gateway-admin-session-tests-{}", Uuid::new_v4()));
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("admin-sessions.json");
        let cfg = test_config(Some("admin-key"));
        let mut sessions = HashMap::new();
        let session_id = create_session(&mut sessions, 600, &cfg);

        save_sessions(&path, &sessions).expect("save succeeds");
        assert!(fs::metadata(&path).unwrap().is_file());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        let mut loaded = load_sessions(&path);
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_str(&format!("{ADMIN_SESSION_COOKIE}={session_id}")).unwrap(),
        );
        assert!(validate_session(&headers, &mut loaded, &cfg));

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn save_sessions_surfaces_unwritable_parent_errors() {
        let directory =
            std::env::temp_dir().join(format!("io-gateway-admin-session-tests-{}", Uuid::new_v4()));
        fs::create_dir_all(&directory).unwrap();
        let non_directory = directory.join("not-a-directory");
        fs::write(&non_directory, b"not a directory").unwrap();

        let err = save_sessions(&non_directory.join("admin-sessions.json"), &HashMap::new())
            .expect_err("invalid parent must be returned to the caller");
        assert!(err.contains("failed to create admin session directory"));

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn login_lockout_identity_uses_peer_ip_not_rotatable_user_agent() {
        let peer_addr: SocketAddr = "203.0.113.10:4242".parse().unwrap();
        let mut attempts = HashMap::new();

        for (index, user_agent) in ["attempt-one", "attempt-two", "attempt-three"]
            .into_iter()
            .enumerate()
        {
            let mut headers = HeaderMap::new();
            headers.insert(header::USER_AGENT, HeaderValue::from_static(user_agent));
            // An untrusted client can send a spoofed forwarding header, but it
            // must not determine the lockout bucket when trusted_proxy=false.
            headers.insert("x-forwarded-for", HeaderValue::from_static("198.51.100.99"));
            let client_key = login_client_key(&headers, false, Some(peer_addr));
            assert_eq!(client_key, "peer-ip:203.0.113.10");

            let result = record_failed_login(
                &mut attempts,
                &client_key,
                UNIX_EPOCH + std::time::Duration::from_secs(index as u64),
            );
            if index < 2 {
                assert!(result.is_none());
            } else {
                assert!(result.is_some());
            }
        }

        let mut rotated_headers = HeaderMap::new();
        rotated_headers.insert(
            header::USER_AGENT,
            HeaderValue::from_static("new-user-agent"),
        );
        let rotated_key = login_client_key(&rotated_headers, false, Some(peer_addr));
        assert!(current_lockout_message(
            &mut attempts,
            &rotated_key,
            UNIX_EPOCH + std::time::Duration::from_secs(4)
        )
        .is_some());
    }

    #[test]
    fn login_lockout_identity_uses_validated_forwarded_ip_only_when_trusted() {
        let peer_addr: SocketAddr = "192.0.2.10:4242".parse().unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-forwarded-for",
            HeaderValue::from_static("198.51.100.10, 192.0.2.1"),
        );
        headers.insert(
            header::USER_AGENT,
            HeaderValue::from_static("rotatable-agent"),
        );

        assert_eq!(
            login_client_key(&headers, true, Some(peer_addr)),
            "forwarded-ip:198.51.100.10"
        );
        assert_eq!(
            login_client_key(&headers, false, Some(peer_addr)),
            "peer-ip:192.0.2.10"
        );

        headers.insert("x-forwarded-for", HeaderValue::from_static("not-an-ip"));
        assert_eq!(
            login_client_key(&headers, true, Some(peer_addr)),
            "peer-ip:192.0.2.10"
        );
        assert_eq!(login_client_key(&headers, false, None), "global");
    }

    #[test]
    fn failed_login_lockout_escalates() {
        let mut attempts = HashMap::new();
        let client_key = "ip:127.0.0.1|ua:test";

        assert!(record_failed_login(&mut attempts, client_key, UNIX_EPOCH).is_none());
        assert!(record_failed_login(
            &mut attempts,
            client_key,
            UNIX_EPOCH + std::time::Duration::from_secs(60)
        )
        .is_none());
        let first_lockout = record_failed_login(
            &mut attempts,
            client_key,
            UNIX_EPOCH + std::time::Duration::from_secs(120),
        )
        .unwrap();
        assert!(first_lockout.contains("5 minutes"));
        assert!(current_lockout_message(
            &mut attempts,
            client_key,
            UNIX_EPOCH + std::time::Duration::from_secs(121)
        )
        .is_some());
        assert!(current_lockout_message(
            &mut attempts,
            client_key,
            UNIX_EPOCH + std::time::Duration::from_secs(421)
        )
        .is_none());

        assert!(record_failed_login(
            &mut attempts,
            client_key,
            UNIX_EPOCH + std::time::Duration::from_secs(430)
        )
        .is_none());
        assert!(record_failed_login(
            &mut attempts,
            client_key,
            UNIX_EPOCH + std::time::Duration::from_secs(460)
        )
        .is_none());
        let second_lockout = record_failed_login(
            &mut attempts,
            client_key,
            UNIX_EPOCH + std::time::Duration::from_secs(490),
        )
        .unwrap();
        assert!(second_lockout.contains("10 minutes"));
    }
}
