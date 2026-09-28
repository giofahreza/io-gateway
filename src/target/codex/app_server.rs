//! Minimal, local-only Codex App Server client for reset-credit automation.
//!
//! Reset-credit consumption is a ChatGPT-account operation. The documented
//! interface is Codex App Server's JSON-RPC account API, not an undocumented
//! direct HTTP backend. App Server is currently experimental/unsupported for
//! production workloads, so this adapter is disabled unless an administrator
//! explicitly configures a trusted executable and per-account managed profiles.

use serde_json::{json, Value};
use std::{
    fs::File,
    io::Read,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout, Command},
    time::timeout,
};

const APP_SERVER_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const APP_SERVER_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_PROTOCOL_LINE_BYTES: usize = 1024 * 1024;
// Do not inherit a caller-controlled PATH into the profile that contains a
// managed ChatGPT session.  This covers the common `/usr/bin/env node`
// launcher while keeping lookup restricted to administrator-owned system
// locations.
const APP_SERVER_SAFE_PATH_DIRECTORIES: &[&str] = &[
    "/usr/local/sbin",
    "/usr/local/bin",
    "/usr/sbin",
    "/usr/bin",
    "/sbin",
    "/bin",
];
const MAX_SHEBANG_BYTES: usize = 4096;

struct TrustedAppServerCommand {
    path: PathBuf,
    safe_path: std::ffi::OsString,
}

/// Validates the explicit App Server configuration without launching a child.
/// An enabled policy calls this before scheduling automatic work; disabled
/// policies remain removable if the optional experimental dependency goes
/// away.
pub(crate) fn validate_config(cfg: &crate::Config) -> Result<(), String> {
    let settings = &cfg.codex_reset_credit_app_server;
    if !settings.experimental_opt_in {
        return Err(
            "Codex reset-credit automation requires codex_reset_credit_app_server.experimental_opt_in=true because Codex App Server is experimental"
                .to_string(),
        );
    }
    let _ = configured_command(cfg)?;
    let _ = configured_profile_root(cfg)?;
    if settings.profiles.is_empty() {
        return Err(
            "configure at least one managed Codex App Server profile before enabling reset-credit automation"
                .to_string(),
        );
    }
    Ok(())
}

/// Validates the profile binding for one account.  This is deliberately more
/// narrow than [`validate_config`]: a stale profile belonging to account B
/// must not prevent account A from using its independently-managed profile.
/// Ambiguous bindings for the selected account or its resolved profile path
/// still fail closed.
pub(crate) fn validate_config_for_account(
    cfg: &crate::Config,
    account_key: &str,
) -> Result<(), String> {
    let _ = configured_profile(cfg, account_key)?;
    Ok(())
}

/// Fetches a fresh, authoritative App Server `account/rateLimits/read`
/// result. The caller must validate the result's rate-limit and credit
/// structure before letting it authorize a redemption.
pub(crate) async fn read_rate_limits(
    cfg: &crate::Config,
    account_key: &str,
) -> Result<Value, String> {
    let mut session = AppServerSession::launch(cfg, account_key).await?;
    let result = session.request("account/rateLimits/read", None).await;
    session.shutdown().await;
    result
}

/// Sends one documented `account/rateLimitResetCredit/consume` request. The
/// durable action ledger owns both the exact opaque credit ID and idempotency
/// UUID; neither comes from a browser or gets logged by this adapter.
pub(crate) async fn consume_rate_limit_reset_credit(
    cfg: &crate::Config,
    account_key: &str,
    credit_id: &str,
    idempotency_key: &str,
) -> Result<String, String> {
    if credit_id.is_empty() {
        return Err("Codex reset-credit ID must not be empty".to_string());
    }
    if credit_id != credit_id.trim() {
        return Err(
            "Codex reset-credit ID must not have leading or trailing whitespace".to_string(),
        );
    }
    if idempotency_key.is_empty() {
        return Err("Codex reset-credit idempotency key must not be empty".to_string());
    }
    if idempotency_key != idempotency_key.trim() {
        return Err(
            "Codex reset-credit idempotency key must not have leading or trailing whitespace"
                .to_string(),
        );
    }
    let mut session = AppServerSession::launch(cfg, account_key).await?;
    let result = session
        .request(
            "account/rateLimitResetCredit/consume",
            Some(json!({
                "idempotencyKey": idempotency_key,
                "creditId": credit_id,
            })),
        )
        .await
        .and_then(|value| {
            value
                .get("outcome")
                .and_then(Value::as_str)
                .filter(|outcome| !outcome.is_empty())
                .map(str::to_string)
                .ok_or_else(|| {
                    "Codex App Server returned an invalid reset-credit outcome".to_string()
                })
        });
    session.shutdown().await;
    result
}

fn configured_command(cfg: &crate::Config) -> Result<TrustedAppServerCommand, String> {
    let command = cfg
        .codex_reset_credit_app_server
        .command
        .as_deref()
        .ok_or_else(|| {
            "configure codex_reset_credit_app_server.command with an absolute path to the local Codex executable before enabling reset-credit automation"
                .to_string()
        })?;
    if !command.is_absolute() {
        return Err(
            "codex_reset_credit_app_server.command must be an absolute path to the trusted Codex executable"
                .to_string(),
        );
    }
    let command = std::fs::canonicalize(command).map_err(|_| {
        "the configured Codex App Server command is not an accessible regular file".to_string()
    })?;
    let metadata = std::fs::metadata(&command).map_err(|_| {
        "the configured Codex App Server command is not an accessible regular file".to_string()
    })?;
    if !metadata.is_file() {
        return Err("the configured Codex App Server command must be a regular file".to_string());
    }
    ensure_not_group_or_other_writable(&metadata, "the configured Codex App Server command")?;
    ensure_owned_by_gateway_or_root(&metadata, "the configured Codex App Server command")?;
    ensure_trusted_ancestors(&command, "the configured Codex App Server command")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o111 == 0 {
            return Err("the configured Codex App Server command is not executable".to_string());
        }
    }
    let (safe_path, safe_path_directories) = trusted_app_server_path()?;
    validate_command_interpreter(&command, &safe_path_directories)?;
    Ok(TrustedAppServerCommand {
        path: command,
        safe_path,
    })
}

/// Builds a deliberately small PATH for a trusted App Server launcher.  A
/// common Codex wrapper uses `#!/usr/bin/env node`; validating the directories
/// used by that `env` lookup is therefore part of validating the configured
/// command, not merely process hygiene.
fn trusted_app_server_path() -> Result<(std::ffi::OsString, Vec<PathBuf>), String> {
    let mut directories = Vec::new();
    for configured in APP_SERVER_SAFE_PATH_DIRECTORIES {
        let configured = Path::new(configured);
        let Some(directory) = trusted_system_path_directory(configured) else {
            // A missing or unsafe conventional system directory cannot be
            // used for an interpreter lookup.  Omit it rather than making a
            // deployment depend on every path in this portable shortlist.
            continue;
        };
        if directories.iter().any(|known| known == &directory) {
            continue;
        }
        directories.push(directory);
    }
    if directories.is_empty() {
        return Err(
            "no trusted system PATH directory is available for Codex App Server".to_string(),
        );
    }
    let safe_path = std::env::join_paths(&directories)
        .map_err(|_| "failed to construct a trusted PATH for Codex App Server".to_string())?;
    Ok((safe_path, directories))
}

fn trusted_system_path_directory(configured: &Path) -> Option<PathBuf> {
    let directory = std::fs::canonicalize(configured).ok()?;
    let metadata = std::fs::metadata(&directory).ok()?;
    if !metadata.is_dir()
        || ensure_not_group_or_other_writable(
            &metadata,
            "a trusted system PATH directory for Codex App Server",
        )
        .is_err()
        || ensure_owned_by_gateway_or_root(
            &metadata,
            "a trusted system PATH directory for Codex App Server",
        )
        .is_err()
        || ensure_trusted_ancestors(
            &directory,
            "a trusted system PATH directory for Codex App Server",
        )
        .is_err()
    {
        return None;
    }
    Some(directory)
}

fn validate_command_interpreter(
    command: &Path,
    safe_path_directories: &[PathBuf],
) -> Result<(), String> {
    let mut file = File::open(command)
        .map_err(|_| "failed to inspect the configured Codex App Server command".to_string())?;
    let mut bytes = Vec::new();
    file.by_ref()
        .take((MAX_SHEBANG_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| "failed to inspect the configured Codex App Server command".to_string())?;
    if !bytes.starts_with(b"#!") {
        return Ok(());
    }
    let Some(line_end) = bytes.iter().position(|byte| *byte == b'\n') else {
        return Err(
            "the configured Codex App Server command has an oversized or invalid shebang"
                .to_string(),
        );
    };
    let shebang = std::str::from_utf8(&bytes[2..line_end]).map_err(|_| {
        "the configured Codex App Server command has a non-UTF-8 shebang".to_string()
    })?;
    let mut fields = shebang.split_ascii_whitespace();
    let interpreter = fields.next().ok_or_else(|| {
        "the configured Codex App Server command has an invalid shebang".to_string()
    })?;
    let interpreter = Path::new(interpreter);
    if !interpreter.is_absolute() {
        return Err(
            "the configured Codex App Server command shebang must use an absolute interpreter path"
                .to_string(),
        );
    }
    let interpreter = validate_trusted_executable(
        interpreter,
        "the configured Codex App Server command shebang interpreter",
    )?;
    if interpreter.file_name().and_then(|name| name.to_str()) != Some("env") {
        return Ok(());
    }

    // Linux passes the text following an interpreter as its argument.  The
    // supported Codex wrapper form is `#!/usr/bin/env node` (or env's `-S`
    // spelling).  Reject broader env syntax instead of trying to emulate it
    // and accidentally accepting injected assignments or another command.
    let first_argument = fields.next();
    let node_command = match first_argument {
        Some("node") if fields.next().is_none() => Some("node"),
        Some("-S") if fields.next() == Some("node") && fields.next().is_none() => Some("node"),
        _ => None,
    }
    .ok_or_else(|| {
        "the configured Codex App Server command may use /usr/bin/env only to launch node"
            .to_string()
    })?;
    let _ = resolve_trusted_path_executable(node_command, safe_path_directories)?;
    Ok(())
}

fn validate_trusted_executable(path: &Path, description: &str) -> Result<PathBuf, String> {
    let path = std::fs::canonicalize(path)
        .map_err(|_| format!("{description} is not an accessible regular file"))?;
    let metadata = std::fs::metadata(&path)
        .map_err(|_| format!("{description} is not an accessible regular file"))?;
    if !metadata.is_file() {
        return Err(format!("{description} must be a regular file"));
    }
    ensure_not_group_or_other_writable(&metadata, description)?;
    ensure_owned_by_gateway_or_root(&metadata, description)?;
    ensure_trusted_ancestors(&path, description)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o111 == 0 {
            return Err(format!("{description} is not executable"));
        }
    }
    Ok(path)
}

fn resolve_trusted_path_executable(
    command: &str,
    safe_path_directories: &[PathBuf],
) -> Result<PathBuf, String> {
    for directory in safe_path_directories {
        let candidate = directory.join(command);
        if !candidate.exists() {
            continue;
        }
        return validate_trusted_executable(
            &candidate,
            "the node interpreter used by the configured Codex App Server command",
        );
    }
    Err(
        "the configured Codex App Server command needs node, but no trusted node interpreter is available"
            .to_string(),
    )
}

fn configured_profile_root(cfg: &crate::Config) -> Result<PathBuf, String> {
    let root = cfg
        .codex_reset_credit_app_server
        .profile_root
        .as_deref()
        .ok_or_else(|| {
            "configure codex_reset_credit_app_server.profile_root with an absolute private directory of managed Codex profiles"
                .to_string()
        })?;
    if !root.is_absolute() {
        return Err(
            "codex_reset_credit_app_server.profile_root must be an absolute private directory"
                .to_string(),
        );
    }
    ensure_private_existing_directory(root, "Codex App Server profile root")?;
    let root = std::fs::canonicalize(root)
        .map_err(|_| "failed to resolve the private Codex App Server profile root".to_string())?;
    ensure_private_existing_directory(&root, "Codex App Server profile root")?;
    ensure_trusted_ancestors(&root, "Codex App Server profile root")?;
    Ok(root)
}

fn configured_profile<'a>(
    cfg: &'a crate::Config,
    account_key: &str,
) -> Result<(&'a crate::CodexResetCreditAppServerProfile, PathBuf), String> {
    validate_config(cfg)?;
    let account_key = normalized_account_key(account_key)?;
    let mut selected = None;
    for (index, candidate) in cfg
        .codex_reset_credit_app_server
        .profiles
        .iter()
        .enumerate()
    {
        let Ok(candidate_account_key) = normalized_account_key(&candidate.account_key) else {
            // An invalid binding for another account is not allowed to block
            // a valid account's independent automation.  It remains unusable
            // if an operator tries to enable that account.
            continue;
        };
        if candidate_account_key == account_key {
            if selected.is_some() {
                return Err(
                    "each Codex App Server profile must bind a unique stable account_key"
                        .to_string(),
                );
            }
            selected = Some((index, candidate));
        }
    }
    let (selected_index, profile) = selected.ok_or_else(|| {
        "no managed Codex App Server profile is configured for this stable account".to_string()
    })?;
    let root = configured_profile_root(cfg)?;
    let profile_name = normalized_profile_name(&profile.profile)?;
    let email = profile.expected_email.as_deref().ok_or_else(|| {
        "the selected Codex App Server profile requires expected_email to confirm its managed ChatGPT account"
            .to_string()
    })?;
    let _ = normalized_email(email)?;
    let home = resolve_managed_profile_directory(&root, profile_name)?;
    ensure_no_conflicting_profile_path(cfg, selected_index, &root, &home)?;
    Ok((profile, home))
}

fn resolve_managed_profile_directory(root: &Path, profile_name: &str) -> Result<PathBuf, String> {
    let requested_home = root.join(profile_name);
    ensure_private_existing_directory(&requested_home, "Codex App Server account profile")?;
    let home = std::fs::canonicalize(&requested_home).map_err(|_| {
        "failed to resolve the private Codex App Server account profile".to_string()
    })?;
    if home.parent() != Some(root) {
        return Err(
            "Codex App Server account profile must resolve directly beneath its configured profile root"
                .to_string(),
        );
    }
    // Re-check the resolved leaf after canonicalization.  This protects both
    // case-folded file systems and the ordinary check-then-use replacement
    // race around a configured profile directory.
    ensure_private_existing_directory(&home, "Codex App Server account profile")?;
    ensure_trusted_ancestors(&home, "Codex App Server account profile")?;
    ensure_managed_profile_state_isolated(&home)?;
    Ok(home)
}

fn ensure_no_conflicting_profile_path(
    cfg: &crate::Config,
    selected_index: usize,
    root: &Path,
    selected_home: &Path,
) -> Result<(), String> {
    for (index, candidate) in cfg
        .codex_reset_credit_app_server
        .profiles
        .iter()
        .enumerate()
    {
        if index == selected_index {
            continue;
        }
        let Ok(profile_name) = normalized_profile_name(&candidate.profile) else {
            // An invalid unrelated binding cannot name the selected profile.
            continue;
        };
        let Ok(candidate_home) = std::fs::canonicalize(root.join(profile_name)) else {
            // A stale/missing unrelated profile is intentionally harmless.
            continue;
        };
        if candidate_home == selected_home {
            return Err(
                "each Codex App Server profile directory may be bound to only one account"
                    .to_string(),
            );
        }
    }
    Ok(())
}

fn normalized_account_key(value: &str) -> Result<&str, String> {
    let value = value.trim();
    let Some(account_id) = value.strip_prefix("codex:account_id:") else {
        return Err(
            "Codex App Server profile account_key must use codex:account_id:<id>".to_string(),
        );
    };
    if account_id.is_empty() || value.len() > 512 || value.chars().any(char::is_control) {
        return Err("Codex App Server profile account_key is invalid".to_string());
    }
    Ok(value)
}

fn normalized_profile_name(value: &str) -> Result<&str, String> {
    let value = value.trim();
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
    {
        return Err(
            "Codex App Server profile must be a simple ASCII letter, digit, underscore, or hyphen directory name"
                .to_string(),
        );
    }
    Ok(value)
}

fn normalized_email(value: &str) -> Result<String, String> {
    let value = value.trim();
    if value.is_empty()
        || value.len() > 320
        || value.chars().any(char::is_control)
        || !value.contains('@')
    {
        return Err("Codex App Server expected_email is invalid".to_string());
    }
    Ok(value.to_ascii_lowercase())
}

fn ensure_private_existing_directory(path: &Path, description: &str) -> Result<(), String> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|_| format!("{description} must be an existing private directory"))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(format!(
            "{description} must be a real directory, not a symlink"
        ));
    }
    ensure_owned_by_gateway_user(&metadata, description)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(format!(
                "{description} must not be readable or writable by group or other users"
            ));
        }
    }
    Ok(())
}

fn ensure_private_existing_regular_file(path: &Path, description: &str) -> Result<(), String> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|_| format!("{description} must be an existing private regular file"))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(format!(
            "{description} must be a regular file, not a symlink"
        ));
    }
    ensure_owned_by_gateway_user(&metadata, description)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(format!(
                "{description} must not be readable or writable by group or other users"
            ));
        }
    }
    Ok(())
}

/// A managed profile must use its private `auth.json`, rather than an OS-wide
/// keyring selected by ordinary user configuration.  The fixed App Server
/// `-c cli_auth_credentials_store=\"file\"` override below enforces this at
/// launch too; checking the file here makes a broken or copied profile fail
/// before a worker attempts a reset-credit redemption.
fn ensure_managed_profile_state_isolated(home: &Path) -> Result<(), String> {
    ensure_private_existing_regular_file(
        &home.join("auth.json"),
        "Codex App Server account profile auth.json",
    )?;

    let config = home.join("config.toml");
    let metadata = match std::fs::symlink_metadata(&config) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(_) => {
            return Err(
                "failed to inspect Codex App Server account profile config.toml".to_string(),
            )
        }
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(
            "Codex App Server account profile config.toml must be a regular file, not a symlink"
                .to_string(),
        );
    }
    ensure_owned_by_gateway_user(&metadata, "Codex App Server account profile config.toml")?;
    ensure_not_group_or_other_writable(&metadata, "Codex App Server account profile config.toml")?;
    const MAX_PROFILE_CONFIG_BYTES: u64 = 1024 * 1024;
    if metadata.len() > MAX_PROFILE_CONFIG_BYTES {
        return Err(
            "Codex App Server account profile config.toml is too large to validate safely"
                .to_string(),
        );
    }
    let config_text = std::fs::read_to_string(&config).map_err(|_| {
        "failed to read Codex App Server account profile config.toml safely".to_string()
    })?;
    let config_value: toml::Value = config_text.parse().map_err(|_| {
        "Codex App Server account profile config.toml is not valid TOML".to_string()
    })?;
    let table = config_value.as_table().ok_or_else(|| {
        "Codex App Server account profile config.toml must be a TOML table".to_string()
    })?;
    if table.contains_key("sqlite_home") {
        return Err(
            "Codex App Server account profile config.toml must not set sqlite_home; it would override the isolated CODEX_SQLITE_HOME"
                .to_string(),
        );
    }
    if let Some(store) = table.get("cli_auth_credentials_store") {
        if store.as_str() != Some("file") {
            return Err(
                "Codex App Server account profile config.toml may set cli_auth_credentials_store only to \"file\""
                    .to_string(),
            );
        }
    }
    Ok(())
}

fn ensure_not_group_or_other_writable(
    metadata: &std::fs::Metadata,
    description: &str,
) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o022 != 0 {
            return Err(format!(
                "{description} must not be writable by group or other users"
            ));
        }
    }
    Ok(())
}

fn ensure_owned_by_gateway_user(
    metadata: &std::fs::Metadata,
    description: &str,
) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.uid() != unsafe { libc::geteuid() } {
            return Err(format!(
                "{description} must be owned by the gateway process user"
            ));
        }
    }
    Ok(())
}

fn ensure_owned_by_gateway_or_root(
    metadata: &std::fs::Metadata,
    description: &str,
) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let effective_uid = unsafe { libc::geteuid() };
        if metadata.uid() != effective_uid && metadata.uid() != 0 {
            return Err(format!(
                "{description} must be owned by the gateway process user or root"
            ));
        }
    }
    Ok(())
}

/// A private leaf is not enough if an untrusted directory above it can replace
/// the leaf between validation and process launch.  The canonical path has no
/// symlink components, so checking each real ancestor closes the ordinary
/// profile/executable replacement path.  A sticky shared parent is allowed
/// only when it belongs to root or the gateway user: otherwise its owner can
/// replace a gateway-owned leaf despite the sticky bit.
fn ensure_trusted_ancestors(path: &Path, description: &str) -> Result<(), String> {
    let mut ancestor = path.parent();
    while let Some(current) = ancestor {
        let metadata = std::fs::metadata(current)
            .map_err(|_| format!("failed to inspect an ancestor of {description}"))?;
        if !metadata.is_dir() {
            return Err(format!("an ancestor of {description} is not a directory"));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::{MetadataExt, PermissionsExt};
            let mode = metadata.permissions().mode();
            if mode & 0o022 != 0 {
                if mode & 0o1000 == 0 {
                    return Err(format!(
                        "an ancestor of {description} must not be writable by group or other users"
                    ));
                }
                let owner = metadata.uid();
                let effective_uid = unsafe { libc::geteuid() };
                if owner != 0 && owner != effective_uid {
                    return Err(format!(
                        "a shared sticky ancestor of {description} must be owned by root or the gateway process user"
                    ));
                }
            }
        }
        ancestor = current.parent();
    }
    Ok(())
}

struct AppServerSession {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl AppServerSession {
    async fn launch(cfg: &crate::Config, account_key: &str) -> Result<Self, String> {
        let (profile, home) = configured_profile(cfg, account_key)?;
        let command = configured_command(cfg)?;
        let mut child = Command::new(&command.path)
            .arg("app-server")
            .arg("--stdio")
            // The profile must never fall back to an OS-wide keyring.  Codex
            // accepts documented inline TOML overrides for App Server; pair
            // this with a private profile auth.json checked above.
            .arg("-c")
            .arg("cli_auth_credentials_store=\"file\"")
            // The executable/profile are administrator-owned, but do not
            // inherit ambient credentials, global plugins, or user settings.
            .env_clear()
            .env("PATH", &command.safe_path)
            .env("HOME", &home)
            .env("CODEX_HOME", &home)
            .env("CODEX_SQLITE_HOME", &home)
            .env("LANG", "C.UTF-8")
            .current_dir(&home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|_| "failed to start the configured Codex App Server".to_string())?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| "failed to open Codex App Server input".to_string())?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "failed to open Codex App Server output".to_string())?;
        let mut session = Self {
            child,
            stdin,
            stdout: BufReader::new(stdout),
            next_id: 1,
        };
        let initialized = session
            .request(
                "initialize",
                Some(json!({
                    "clientInfo": {
                        "name": "io_gateway",
                        "title": "IO Gateway reset-credit automation",
                        "version": env!("CARGO_PKG_VERSION"),
                    },
                })),
            )
            .await;
        if let Err(error) = initialized {
            session.shutdown().await;
            return Err(error);
        }
        if let Err(error) = session.notify("initialized", Some(json!({}))).await {
            session.shutdown().await;
            return Err(error);
        }
        let account = session
            .request("account/read", Some(json!({ "refreshToken": false })))
            .await;
        let account = match account {
            Ok(account) => account,
            Err(error) => {
                session.shutdown().await;
                return Err(error);
            }
        };
        if let Err(error) =
            verify_managed_profile_account(&account, profile.expected_email.as_deref())
        {
            session.shutdown().await;
            return Err(error);
        }
        Ok(session)
    }

    async fn request(&mut self, method: &str, params: Option<Value>) -> Result<Value, String> {
        let id = self.next_id;
        self.next_id = self.next_id.checked_add(1).ok_or_else(|| {
            "Codex App Server request identifier overflowed unexpectedly".to_string()
        })?;
        let mut request = json!({ "method": method, "id": id });
        if let Some(params) = params {
            request["params"] = params;
        }
        self.write_message(request).await?;
        timeout(APP_SERVER_REQUEST_TIMEOUT, async {
            loop {
                let message = self.read_message().await?;
                if message.get("id") == Some(&json!(id)) {
                    if message.get("error").is_some() {
                        return Err(
                            "Codex App Server rejected the requested account operation".to_string()
                        );
                    }
                    return message.get("result").cloned().ok_or_else(|| {
                        "Codex App Server response did not include a result".to_string()
                    });
                }
                self.respond_to_server_request_if_needed(&message).await?;
            }
        })
        .await
        .map_err(|_| "Codex App Server account request timed out".to_string())?
    }

    async fn notify(&mut self, method: &str, params: Option<Value>) -> Result<(), String> {
        let mut notification = json!({ "method": method });
        if let Some(params) = params {
            notification["params"] = params;
        }
        self.write_message(notification).await
    }

    async fn read_message(&mut self) -> Result<Value, String> {
        // `read_line` allocates the whole line before exposing its length.
        // This child is deliberately treated as a trust boundary because it
        // can read a managed profile, so bound the allocation while reading.
        let mut line = Vec::new();
        loop {
            let (consumed, complete) = {
                let buffer = self
                    .stdout
                    .fill_buf()
                    .await
                    .map_err(|_| "failed to read Codex App Server response".to_string())?;
                if buffer.is_empty() {
                    return Err("Codex App Server closed before responding".to_string());
                }
                let consumed = buffer
                    .iter()
                    .position(|byte| *byte == b'\n')
                    .map_or(buffer.len(), |index| index + 1);
                let complete = buffer.get(consumed.saturating_sub(1)) == Some(&b'\n');
                let content_length = consumed.saturating_sub(usize::from(complete));
                if line.len().saturating_add(content_length) > MAX_PROTOCOL_LINE_BYTES {
                    return Err(
                        "Codex App Server returned an oversized protocol message".to_string()
                    );
                }
                line.extend_from_slice(&buffer[..consumed]);
                (consumed, complete)
            };
            self.stdout.consume(consumed);
            if complete {
                break;
            }
        }
        // Do not echo parse errors: a compromised/misconfigured child must
        // not make account data visible in the dashboard, action ledger, or
        // operator log.
        serde_json::from_slice(&line)
            .map_err(|_| "Codex App Server returned an invalid protocol message".to_string())
    }

    async fn respond_to_server_request_if_needed(&mut self, message: &Value) -> Result<(), String> {
        if message.get("method").is_none() || message.get("id").is_none() {
            return Ok(());
        }
        // This managed-profile integration does not expose any server-request
        // capability. Reply instead of silently hanging the local App Server.
        self.write_message(json!({
            "id": message.get("id").cloned().unwrap_or(Value::Null),
            "error": {
                "code": -32000,
                "message": "IO Gateway does not handle this App Server request"
            }
        }))
        .await
    }

    async fn write_message(&mut self, value: Value) -> Result<(), String> {
        let mut line = serde_json::to_vec(&value)
            .map_err(|_| "failed to encode Codex App Server request".to_string())?;
        line.push(b'\n');
        self.stdin
            .write_all(&line)
            .await
            .map_err(|_| "failed to write Codex App Server request".to_string())?;
        self.stdin
            .flush()
            .await
            .map_err(|_| "failed to flush Codex App Server request".to_string())
    }

    async fn shutdown(self) {
        // `AsyncWriteExt::shutdown` only operates on the borrowed writer for
        // some child-pipe implementations. Drop the owned pipe before
        // waiting so a well-behaved JSONL App Server deterministically sees
        // EOF and can exit; otherwise every fresh read would pay the timeout.
        let AppServerSession {
            mut child,
            stdin,
            stdout: _,
            next_id: _,
        } = self;
        drop(stdin);
        match timeout(APP_SERVER_SHUTDOWN_TIMEOUT, child.wait()).await {
            Ok(_) => {}
            Err(_) => {
                let _ = child.start_kill();
                let _ = child.wait().await;
            }
        }
    }
}

fn verify_managed_profile_account(
    account: &Value,
    expected_email: Option<&str>,
) -> Result<(), String> {
    let account = account
        .get("account")
        .and_then(Value::as_object)
        .ok_or_else(|| {
            "the configured Codex App Server profile is not logged in with managed ChatGPT authentication"
                .to_string()
        })?;
    let account_type = account.get("type").and_then(Value::as_str).map(str::trim);
    if account_type != Some("chatgpt") {
        return Err(
            "the configured Codex App Server profile must use managed ChatGPT authentication, not an API key or externally supplied token"
                .to_string(),
        );
    }
    if let Some(expected_email) = expected_email {
        let actual_email = account
            .get("email")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                "the configured Codex App Server profile did not disclose the expected ChatGPT email"
                    .to_string()
            })?;
        if normalized_email(actual_email)? != normalized_email(expected_email)? {
            return Err(
                "the configured Codex App Server profile does not match its expected ChatGPT email"
                    .to_string(),
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_server_configuration_rejects_an_implicit_or_traversal_profile() {
        let directory = std::env::temp_dir().join(format!(
            "io-gateway-app-server-config-tests-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let mut cfg = test_config(&directory);
        assert!(validate_config(&cfg).is_err());
        cfg.codex_reset_credit_app_server.experimental_opt_in = true;
        cfg.codex_reset_credit_app_server.command = Some(PathBuf::from("relative-codex"));
        assert!(validate_config(&cfg).is_err());
        assert!(normalized_profile_name("../escape").is_err());
        assert!(normalized_profile_name("valid_profile-1").is_ok());
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn managed_profile_verification_rejects_external_or_api_key_authentication() {
        for account in [
            json!({"account": {"type": "apiKey"}}),
            json!({"account": {"type": "chatgptAuthTokens"}}),
            json!({"account": null}),
        ] {
            assert!(verify_managed_profile_account(&account, None).is_err());
        }
        assert!(verify_managed_profile_account(
            &json!({"account": {"type": "chatgpt", "email": "Owner@Example.test"}}),
            Some("owner@example.test"),
        )
        .is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn selected_account_validation_ignores_an_unrelated_stale_profile() {
        let fixture = PrivateProfileFixture::new();
        fixture.create_profile("selected");
        let mut cfg = fixture.config(vec![profile("selected-account", "selected")]);
        cfg.codex_reset_credit_app_server
            .profiles
            .push(crate::CodexResetCreditAppServerProfile {
                account_key: "not-a-stable-account-key".to_string(),
                profile: "missing-profile".to_string(),
                expected_email: None,
            });

        assert!(validate_config(&cfg).is_ok());
        assert!(validate_config_for_account(&cfg, "codex:account_id:selected-account").is_ok());
        assert!(validate_config_for_account(&cfg, "codex:account_id:other-account").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn selected_account_validation_rejects_a_canonical_profile_collision() {
        let fixture = PrivateProfileFixture::new();
        fixture.create_profile("selected");
        // On a case-sensitive filesystem use the same spelling; on a
        // case-folded filesystem this becomes `SELECTED`, proving that we
        // compare resolved directories rather than raw profile strings.
        let conflicting_name = if fixture.root.join("SELECTED").exists() {
            "SELECTED"
        } else {
            "selected"
        };
        let cfg = fixture.config(vec![
            profile("selected-account", "selected"),
            profile("other-account", conflicting_name),
        ]);

        let error = validate_config_for_account(&cfg, "codex:account_id:selected-account")
            .expect_err("two account bindings must not resolve to one profile directory");
        assert!(error.contains("only one account"));
    }

    #[cfg(unix)]
    #[test]
    fn selected_profile_requires_private_file_backed_auth_and_safe_config() {
        let fixture = PrivateProfileFixture::new();
        let home = fixture.create_profile("selected");
        let cfg = fixture.config(vec![profile("selected-account", "selected")]);
        let account_key = "codex:account_id:selected-account";

        assert!(validate_config_for_account(&cfg, account_key).is_ok());

        // Quoted TOML keys exercise the real TOML parser rather than a
        // brittle line-oriented check for the isolation override.
        write_private_file(
            &home.join("config.toml"),
            "\"sqlite_home\" = \"../outside\"\n",
        );
        let error = validate_config_for_account(&cfg, account_key).unwrap_err();
        assert!(error.contains("sqlite_home"));

        write_private_file(
            &home.join("config.toml"),
            "cli_auth_credentials_store = \"keyring\"\n",
        );
        let error = validate_config_for_account(&cfg, account_key).unwrap_err();
        assert!(error.contains("cli_auth_credentials_store"));

        write_private_file(
            &home.join("config.toml"),
            "cli_auth_credentials_store = \"file\"\n",
        );
        assert!(validate_config_for_account(&cfg, account_key).is_ok());

        set_mode(&home.join("auth.json"), 0o644);
        assert!(validate_config_for_account(&cfg, account_key).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn command_validation_rejects_an_ambiguous_env_shebang() {
        let fixture = PrivateProfileFixture::new();
        let command = fixture._temporary.path.join("unsafe-codex-wrapper");
        write_private_file(&command, "#!/usr/bin/env PATH=/tmp node\n");
        set_mode(&command, 0o700);
        let mut cfg = test_config(&fixture.root);
        cfg.codex_reset_credit_app_server.experimental_opt_in = true;
        cfg.codex_reset_credit_app_server.command = Some(command);

        let error = match configured_command(&cfg) {
            Ok(_) => panic!("unsafe /usr/bin/env shebang must be rejected"),
            Err(error) => error,
        };
        assert!(error.contains("/usr/bin/env only to launch node"));
    }

    #[cfg(unix)]
    #[test]
    fn command_validation_rejects_env_split_string_node_arguments() {
        let fixture = PrivateProfileFixture::new();
        let command = fixture._temporary.path.join("unsafe-codex-wrapper");
        // `env -S` legitimately splits its one shebang argument. It must
        // still be restricted to exactly `node`, otherwise a wrapper can add
        // arbitrary Node preload/runtime flags despite the fixed gateway
        // launcher arguments.
        write_private_file(&command, "#!/usr/bin/env -S node --require unsafe-module\n");
        set_mode(&command, 0o700);
        let mut cfg = test_config(&fixture.root);
        cfg.codex_reset_credit_app_server.experimental_opt_in = true;
        cfg.codex_reset_credit_app_server.command = Some(command);

        let error = match configured_command(&cfg) {
            Ok(_) => panic!("env -S node with trailing arguments must be rejected"),
            Err(error) => error,
        };
        assert!(error.contains("/usr/bin/env only to launch node"));
    }

    #[tokio::test]
    async fn opaque_reset_credit_identifiers_reject_whitespace_instead_of_normalizing() {
        let directory = std::env::temp_dir().join(format!(
            "io-gateway-app-server-opaque-id-tests-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let cfg = test_config(&directory);
        let error = consume_rate_limit_reset_credit(
            &cfg,
            "codex:account_id:unused",
            " credit-id",
            "idempotency-key",
        )
        .await
        .unwrap_err();
        assert!(error.contains("ID"));
        let error = consume_rate_limit_reset_credit(
            &cfg,
            "codex:account_id:unused",
            "credit-id",
            "idempotency-key ",
        )
        .await
        .unwrap_err();
        assert!(error.contains("idempotency"));
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    fn profile(account_id: &str, directory: &str) -> crate::CodexResetCreditAppServerProfile {
        crate::CodexResetCreditAppServerProfile {
            account_key: format!("codex:account_id:{account_id}"),
            profile: directory.to_string(),
            expected_email: Some(format!("{account_id}@example.test")),
        }
    }

    #[cfg(unix)]
    struct PrivateProfileFixture {
        root: PathBuf,
        _temporary: TempDirectory,
    }

    #[cfg(unix)]
    impl PrivateProfileFixture {
        fn new() -> Self {
            let temporary = TempDirectory::new("io-gateway-app-server-profile-tests");
            let root = temporary.path.join("profiles");
            private_directory(&root);
            Self {
                root,
                _temporary: temporary,
            }
        }

        fn create_profile(&self, name: &str) -> PathBuf {
            let home = self.root.join(name);
            private_directory(&home);
            write_private_file(&home.join("auth.json"), "{\"synthetic\":true}\n");
            home
        }

        fn config(&self, profiles: Vec<crate::CodexResetCreditAppServerProfile>) -> crate::Config {
            let mut cfg = test_config(&self.root);
            cfg.codex_reset_credit_app_server.experimental_opt_in = true;
            cfg.codex_reset_credit_app_server.command = Some(test_command());
            cfg.codex_reset_credit_app_server.profile_root = Some(self.root.clone());
            cfg.codex_reset_credit_app_server.profiles = profiles;
            cfg
        }
    }

    #[cfg(unix)]
    struct TempDirectory {
        path: PathBuf,
    }

    #[cfg(unix)]
    impl TempDirectory {
        fn new(prefix: &str) -> Self {
            let path = std::env::temp_dir().join(format!("{prefix}-{}", uuid::Uuid::new_v4()));
            private_directory(&path);
            Self { path }
        }
    }

    #[cfg(unix)]
    impl Drop for TempDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    #[cfg(unix)]
    fn private_directory(path: &Path) {
        std::fs::create_dir_all(path).unwrap();
        set_mode(path, 0o700);
    }

    #[cfg(unix)]
    fn write_private_file(path: &Path, contents: &str) {
        std::fs::write(path, contents).unwrap();
        set_mode(path, 0o600);
    }

    #[cfg(unix)]
    fn set_mode(path: &Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    #[cfg(unix)]
    fn test_command() -> PathBuf {
        ["/usr/bin/true", "/bin/true"]
            .iter()
            .map(PathBuf::from)
            .find(|path| path.is_file())
            .expect("a standard test command is available")
    }

    fn test_config(directory: &Path) -> crate::Config {
        crate::Config {
            listen: "127.0.0.1:0".to_string(),
            upstream_base: "https://example.test".to_string(),
            proxy_api_key: String::new(),
            tokens: Vec::new(),
            auth_dir: Some(directory.to_string_lossy().to_string()),
            disabled_files: None,
            admin_auth: Default::default(),
            oauth: Default::default(),
            codex_reset_credit_app_server: Default::default(),
            max_request_body_bytes: crate::default_max_request_body_bytes(),
            request_body_limit_enabled: crate::default_request_body_limit_enabled(),
            max_concurrent_requests: crate::default_max_concurrent_requests(),
            trusted_proxy: false,
            history_retention_days: crate::default_history_retention_days(),
            history_max_entries: crate::default_history_max_entries(),
            upstream_connect_timeout_seconds: crate::default_upstream_connect_timeout_seconds(),
            upstream_read_timeout_seconds: crate::default_upstream_read_timeout_seconds(),
            upstream_first_event_timeout_seconds:
                crate::default_upstream_first_event_timeout_seconds(),
        }
    }
}
