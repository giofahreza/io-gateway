use base64::Engine;
use chrono::Utc;
use rand::{distr::Alphanumeric, Rng};
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    time::{Duration, Instant},
};
use url::Url;

use super::super::oauth::{provider_config, OAuthProvider, OAuthProviderConfig};
use super::accounts::QwenAccount;

const QWEN_SESSION_USER_AGENT: &str = "Mozilla/5.0";
const QWEN_USER_AGENT: &str = "google-api-nodejs-client/9.15.1";
const QWEN_X_GOOG_API_CLIENT: &str = "gl-node/22.17.0";
const QWEN_CLIENT_METADATA: &str =
    "ideType=IDE_UNSPECIFIED,platform=PLATFORM_UNSPECIFIED,pluginType=GEMINI";
const QWEN_OAUTH_PENDING_TTL_SECONDS: u64 = 900;
const QWEN_PORTAL_BASE_URL: &str = "https://portal.qwen.ai/v1";

#[derive(Clone)]
pub struct PendingOAuth {
    pub code_verifier: String,
    pub redirect_uri: String,
    pub created_at: Instant,
    pub status: PendingStatus,
}

#[derive(Clone)]
pub enum PendingStatus {
    Pending,
    Completed { saved_path: String, label: String },
    Error { message: String },
}

#[derive(Clone, Deserialize)]
pub struct OAuthCallbackQuery {
    pub code: Option<String>,
    pub state: Option<String>,
    pub error: Option<String>,
    pub error_description: Option<String>,
}

#[derive(Deserialize)]
pub struct OAuthTokenResponse {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub token_type: Option<String>,
    pub expires_in: Option<i64>,
    pub resource_url: Option<String>,
}

#[derive(Clone, Deserialize)]
pub struct ValidatedSession {
    pub id: String,
    pub email: Option<String>,
    pub name: Option<String>,
    pub role: Option<String>,
    pub profile_image_url: Option<String>,
    pub tier: Option<String>,
    #[serde(alias = "access_token")]
    pub token: String,
    pub token_type: Option<String>,
    pub expires_at: Option<i64>,
    pub permissions: Option<Value>,
}

pub struct RefreshResult {
    pub account_id: String,
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub resource_url: Option<String>,
    pub expired_at: String,
    pub expires_at_unix: Option<i64>,
    pub email: Option<String>,
    pub subject: Option<String>,
    pub label: String,
    pub role: Option<String>,
    pub tier: Option<String>,
    pub token_type: Option<String>,
    pub profile_image_url: Option<String>,
    pub permissions: Option<Value>,
}

#[derive(Clone, Debug)]
pub struct QwenIdentity {
    pub account_id: String,
    pub email: Option<String>,
    pub subject: Option<String>,
    pub label: String,
    pub file_key: String,
}

pub fn qwen_headers(
    builder: reqwest::RequestBuilder,
    access_token: &str,
) -> reqwest::RequestBuilder {
    builder
        .header("Authorization", format!("Bearer {}", access_token))
        .header("User-Agent", QWEN_USER_AGENT)
        .header("X-Goog-Api-Client", QWEN_X_GOOG_API_CLIENT)
        .header("Client-Metadata", QWEN_CLIENT_METADATA)
}

pub fn identity_from_access_token(access_token: &str, fallback_label: &str) -> QwenIdentity {
    let claims = parse_access_token_claims(access_token);
    let email = claims
        .as_ref()
        .and_then(|claims| claims.get("email"))
        .and_then(|v| v.as_str())
        .map(|value| value.to_string());
    let preferred_username = claims
        .as_ref()
        .and_then(|claims| claims.get("preferred_username"))
        .and_then(|v| v.as_str())
        .map(|value| value.to_string());
    let name = claims
        .as_ref()
        .and_then(|claims| claims.get("name"))
        .and_then(|v| v.as_str())
        .map(|value| value.to_string());
    let subject = claims
        .as_ref()
        .and_then(|claims| claims.get("sub"))
        .and_then(|v| v.as_str())
        .map(|value| value.to_string());

    let label = email
        .clone()
        .or(preferred_username.clone())
        .or(name)
        .or(subject.clone())
        .unwrap_or_else(|| fallback_label.to_string());
    let file_key = email
        .clone()
        .or(subject.clone())
        .unwrap_or_else(|| fallback_label.to_string());
    let account_id = subject
        .clone()
        .or(email.clone())
        .unwrap_or_else(|| fallback_label.to_string());

    QwenIdentity {
        account_id,
        email,
        subject,
        label,
        file_key,
    }
}

pub fn build_auth_url(cfg: &crate::Config) -> Result<(String, String, PendingOAuth), String> {
    let provider = qwen_provider_config(Some(cfg));
    let client_id = provider_client_id(&provider)?;
    let redirect_uri = provider_redirect_uri(cfg, &provider)?;
    let authorize_url = provider_authorize_url(&provider)?;
    let code_verifier = generate_code_verifier();
    let code_challenge = code_challenge(&code_verifier);
    let state_token: String = rand::rng()
        .sample_iter(&Alphanumeric)
        .take(32)
        .map(char::from)
        .collect();

    let mut url = Url::parse(&authorize_url).map_err(|e| e.to_string())?;
    url.query_pairs_mut()
        .append_pair("client_id", &client_id)
        .append_pair("response_type", "code")
        .append_pair("redirect_uri", &redirect_uri)
        .append_pair("scope", &provider.scopes.join(" "))
        .append_pair("state", &state_token)
        .append_pair("code_challenge", &code_challenge)
        .append_pair("code_challenge_method", "S256");

    Ok((
        url.to_string(),
        state_token,
        PendingOAuth {
            code_verifier,
            redirect_uri,
            created_at: Instant::now(),
            status: PendingStatus::Pending,
        },
    ))
}

pub fn pending_is_expired(pending: &PendingOAuth) -> bool {
    pending.created_at.elapsed() >= Duration::from_secs(QWEN_OAUTH_PENDING_TTL_SECONDS)
}

pub fn callback_error_message(query: &OAuthCallbackQuery) -> Option<String> {
    let error = query
        .error
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())?;
    let description = query
        .error_description
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    Some(match description {
        Some(description) => format!("Qwen OAuth callback returned {}: {}", error, description),
        None => format!("Qwen OAuth callback returned {}", error),
    })
}

pub fn extract_callback_code_state(query: &OAuthCallbackQuery) -> Result<(String, String), String> {
    if let Some(message) = callback_error_message(query) {
        return Err(message);
    }

    let code = query
        .code
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "Qwen OAuth callback did not include a code".to_string())?
        .to_string();
    let state = query
        .state
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "Qwen OAuth callback did not include a state".to_string())?
        .to_string();

    Ok((code, state))
}

pub fn parse_oauth_callback_url(redirect_url: &str) -> Result<OAuthCallbackQuery, String> {
    let url = Url::parse(redirect_url).map_err(|_| "invalid redirect_url".to_string())?;
    let params = url
        .query_pairs()
        .into_owned()
        .collect::<std::collections::HashMap<_, _>>();
    Ok(OAuthCallbackQuery {
        code: params.get("code").cloned(),
        state: params.get("state").cloned(),
        error: params.get("error").cloned(),
        error_description: params.get("error_description").cloned(),
    })
}

pub fn parse_callback_query_from_uri(uri: &axum::http::Uri) -> Result<OAuthCallbackQuery, String> {
    let params = url::form_urlencoded::parse(uri.query().unwrap_or_default().as_bytes())
        .into_owned()
        .collect::<std::collections::HashMap<_, _>>();
    Ok(OAuthCallbackQuery {
        code: params.get("code").cloned(),
        state: params.get("state").cloned(),
        error: params.get("error").cloned(),
        error_description: params.get("error_description").cloned(),
    })
}

pub async fn validate_and_save_auth(
    state: &crate::AppState,
    browser_token: &str,
) -> Result<(String, String), String> {
    let provider = qwen_provider_config(Some(state.cfg.as_ref()));
    let session = validate_browser_token(&state.client, &provider, browser_token).await?;
    save_validated_session(
        state,
        &provider,
        &session,
        session.token.as_str(),
        Some(browser_token.trim()),
        None,
        None,
        None,
        "browser_token_validate",
    )
}

pub async fn validate_browser_token(
    client: &reqwest::Client,
    provider: &OAuthProviderConfig,
    browser_token: &str,
) -> Result<ValidatedSession, String> {
    fetch_validated_session(client, &provider_validate_url(provider)?, browser_token).await
}

pub async fn ensure_access_token(
    state: &crate::AppState,
    account: &QwenAccount,
) -> Result<String, String> {
    let account_key = crate::qwen_stats_key(account);
    let refresh_lock = crate::account_refresh_lock(state, "qwen", &account_key);
    let _guard = refresh_lock.lock().await;
    let current = state
        .qwen_accounts
        .lock()
        .unwrap()
        .iter()
        .find(|candidate| candidate.file_name == account.file_name)
        .cloned()
        .unwrap_or_else(|| account.clone());
    if let Some(access_token) = current.access_token.as_ref() {
        let still_valid = current
            .expired_at
            .as_deref()
            .and_then(parse_rfc3339)
            .map(|expires_at| expires_at > Utc::now() + chrono::Duration::seconds(60))
            .unwrap_or(true);
        if still_valid {
            return Ok(access_token.clone());
        }
    }

    let refreshed = refresh_access_token(&state.client, &state.cfg, &current.refresh_token).await?;
    persist_refreshed_account(state, &current, &refreshed)?;
    Ok(refreshed.access_token)
}

pub async fn refresh_access_token(
    client: &reqwest::Client,
    cfg: &crate::Config,
    refresh_token: &str,
) -> Result<RefreshResult, String> {
    let provider = qwen_provider_config(Some(cfg));
    if !looks_like_jwt(refresh_token) {
        let token_resp = exchange_refresh_token(client, &provider, refresh_token).await?;
        return build_refresh_result_from_token_response(
            client,
            &provider,
            &token_resp,
            Some(refresh_token),
        )
        .await;
    }

    let session =
        fetch_validated_session(client, &provider_refresh_url(&provider)?, refresh_token).await?;
    build_refresh_result(&provider, session, Some(refresh_token))
}

pub async fn exchange_code_and_save_auth(
    state: &crate::AppState,
    pending: &PendingOAuth,
    code: &str,
) -> Result<(String, String), String> {
    let provider = qwen_provider_config(Some(state.cfg.as_ref()));
    let token_resp = exchange_code_for_tokens(
        &state.client,
        &provider,
        code,
        &pending.code_verifier,
        &pending.redirect_uri,
    )
    .await?;
    save_oauth_auth(state, &provider, &token_resp).await
}

pub fn base_url(state: &crate::AppState, account: &QwenAccount) -> String {
    let provider = qwen_provider_config(Some(state.cfg.as_ref()));
    resource_to_base_url(account.resource_url.as_deref(), &provider)
}

async fn fetch_validated_session(
    client: &reqwest::Client,
    session_url: &str,
    browser_token: &str,
) -> Result<ValidatedSession, String> {
    let browser_token = browser_token.trim();
    if browser_token.is_empty() {
        return Err("Qwen browser token is required".to_string());
    }

    let resp = client
        .get(session_url)
        .header("Authorization", format!("Bearer {}", browser_token))
        .header("Accept", "application/json")
        .header("User-Agent", QWEN_SESSION_USER_AGENT)
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .map_err(|e| e.to_string())?;

    let status = resp.status();
    let body = resp.text().await.map_err(|e| e.to_string())?;
    if !status.is_success() {
        return Err(format!(
            "Qwen token validation failed ({}): {}",
            status,
            describe_error_body(&body)
        ));
    }

    let session: ValidatedSession = serde_json::from_str(&body).map_err(|e| e.to_string())?;
    if session.token.trim().is_empty() {
        return Err("Qwen validation response did not include a usable token".to_string());
    }

    Ok(session)
}

fn build_refresh_result(
    provider: &OAuthProviderConfig,
    session: ValidatedSession,
    refresh_token: Option<&str>,
) -> Result<RefreshResult, String> {
    let access_token = session.token.trim().to_string();
    if access_token.is_empty() {
        return Err("Qwen validation response did not include a usable token".to_string());
    }

    let fallback_label = format!("qwen-{}", Utc::now().timestamp_millis());
    let identity =
        normalized_identity_from_validated_session(&session, &access_token, &fallback_label)?;
    let expires_at_unix = session
        .expires_at
        .or_else(|| parse_access_token_exp(&access_token));
    let expired_at = expires_at_unix
        .and_then(unix_to_rfc3339)
        .ok_or_else(|| "Qwen validation response did not include a valid expiry".to_string())?;

    Ok(RefreshResult {
        account_id: identity.account_id.clone(),
        access_token: access_token.clone(),
        refresh_token: refresh_token
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(|value| value.to_string())
            .or(Some(access_token.clone())),
        resource_url: Some(provider_base_url(provider)),
        expired_at,
        expires_at_unix,
        email: identity.email.clone(),
        subject: identity.subject.clone(),
        label: identity.label,
        role: session.role.clone(),
        tier: session.tier.clone(),
        token_type: session.token_type.clone(),
        profile_image_url: session.profile_image_url.clone(),
        permissions: session.permissions.clone(),
    })
}

fn save_validated_session(
    state: &crate::AppState,
    provider: &OAuthProviderConfig,
    session: &ValidatedSession,
    access_token: &str,
    refresh_token: Option<&str>,
    resource_url: Option<&str>,
    token_type: Option<&str>,
    expires_at_unix: Option<i64>,
    auth_method: &str,
) -> Result<(String, String), String> {
    let persisted_access_token = access_token.trim().to_string();
    if persisted_access_token.is_empty() {
        return Err("Qwen credential save requires a usable access token".to_string());
    }

    let fallback_label = format!("qwen-{}", Utc::now().timestamp_millis());
    let identity = normalized_identity_from_validated_session(
        session,
        &persisted_access_token,
        &fallback_label,
    )?;
    let resolved_expires_at_unix = expires_at_unix
        .or(session.expires_at)
        .or_else(|| parse_access_token_exp(&persisted_access_token));
    let expired_at = resolved_expires_at_unix
        .and_then(unix_to_rfc3339)
        .ok_or_else(|| "Qwen validation response did not include a valid expiry".to_string())?;
    let persisted_refresh_token = refresh_token
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(persisted_access_token.as_str())
        .to_string();
    let persisted_resource_url = resource_url
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| value.to_string())
        .unwrap_or_else(|| provider_base_url(provider));
    let persisted_token_type = token_type
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| value.to_string())
        .or_else(|| session.token_type.clone());

    let auth_dir = state
        .cfg
        .auth_dir
        .clone()
        .unwrap_or_else(|| "/root/dev/yow/io-gateway/auths".to_string());
    let file_name = format!("qwen-{}.json", sanitize_label(&identity.file_key));
    let path = std::path::Path::new(&auth_dir).join(file_name);
    let now = Utc::now().to_rfc3339();
    let out = serde_json::json!({
        "type": "qwen",
        "id": identity.account_id.clone(),
        "account_id": identity.account_id.clone(),
        "email": identity.email.clone().unwrap_or_else(|| identity.label.clone()),
        "subject": identity.subject.clone(),
        "label": identity.label.clone(),
        "name": session.name,
        "role": session.role,
        "tier": session.tier,
        "profile_image_url": session.profile_image_url,
        "token_type": persisted_token_type,
        "access_token": persisted_access_token,
        "refresh_token": persisted_refresh_token,
        "resource_url": persisted_resource_url,
        "permissions": session.permissions,
        "last_refresh": now,
        "validated_at": now,
        "expired": expired_at,
        "expires_at": resolved_expires_at_unix,
        "auth_method": auth_method
    });

    std::fs::create_dir_all(&auth_dir).map_err(|e| e.to_string())?;
    super::super::atomic_write_json(&path, &out)?;
    super::accounts::reload_state(state);
    Ok((path.to_string_lossy().to_string(), identity.label))
}

async fn save_oauth_auth(
    state: &crate::AppState,
    provider: &OAuthProviderConfig,
    token_resp: &OAuthTokenResponse,
) -> Result<(String, String), String> {
    let issued_access_token = token_resp.access_token.trim().to_string();
    if issued_access_token.is_empty() {
        return Err("Qwen OAuth token exchange did not return an access token".to_string());
    }
    let refresh_token = token_resp
        .refresh_token
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| value.to_string())
        .or_else(|| {
            if looks_like_jwt(&issued_access_token) {
                Some(issued_access_token.clone())
            } else {
                None
            }
        })
        .ok_or_else(|| {
            "Qwen OAuth token exchange did not return a usable refresh token".to_string()
        })?;

    let session = validate_browser_token(&state.client, provider, &issued_access_token).await?;
    let expires_at_unix = token_resp
        .expires_in
        .map(|expires_in| Utc::now() + chrono::Duration::seconds(expires_in.max(0)))
        .map(|expires_at| expires_at.timestamp());
    save_validated_session(
        state,
        provider,
        &session,
        &issued_access_token,
        Some(refresh_token.as_str()),
        token_resp.resource_url.as_deref(),
        token_resp.token_type.as_deref(),
        expires_at_unix,
        "oauth_authorization_code",
    )
}

async fn build_refresh_result_from_token_response(
    client: &reqwest::Client,
    provider: &OAuthProviderConfig,
    token_resp: &OAuthTokenResponse,
    fallback_refresh_token: Option<&str>,
) -> Result<RefreshResult, String> {
    let access_token = token_resp.access_token.trim().to_string();
    if access_token.is_empty() {
        return Err("Qwen OAuth token refresh did not return an access token".to_string());
    }
    let refresh_token = token_resp
        .refresh_token
        .clone()
        .or_else(|| {
            fallback_refresh_token
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(|value| value.to_string())
        })
        .or_else(|| {
            if looks_like_jwt(&access_token) {
                Some(access_token.clone())
            } else {
                None
            }
        });

    let session = validate_browser_token(client, provider, &access_token).await?;
    let fallback_label = format!("qwen-{}", Utc::now().timestamp_millis());
    let identity =
        normalized_identity_from_validated_session(&session, &access_token, &fallback_label)?;
    let expires_at_unix = token_resp
        .expires_in
        .map(|expires_in| Utc::now() + chrono::Duration::seconds(expires_in.max(0)))
        .map(|expires_at| expires_at.timestamp())
        .or(session.expires_at)
        .or_else(|| parse_access_token_exp(&access_token));
    let expired_at = expires_at_unix
        .and_then(unix_to_rfc3339)
        .ok_or_else(|| "Qwen OAuth token response did not include a valid expiry".to_string())?;

    Ok(RefreshResult {
        account_id: identity.account_id.clone(),
        access_token: access_token.clone(),
        refresh_token,
        resource_url: token_resp
            .resource_url
            .clone()
            .or(Some(provider_base_url(provider))),
        expired_at,
        expires_at_unix,
        email: identity.email.clone(),
        subject: identity.subject.clone(),
        label: identity.label,
        role: session.role.clone(),
        tier: session.tier.clone(),
        token_type: token_resp.token_type.clone().or(session.token_type.clone()),
        profile_image_url: session.profile_image_url.clone(),
        permissions: session.permissions.clone(),
    })
}

async fn exchange_code_for_tokens(
    client: &reqwest::Client,
    provider: &OAuthProviderConfig,
    code: &str,
    code_verifier: &str,
    redirect_uri: &str,
) -> Result<OAuthTokenResponse, String> {
    let client_id = provider_client_id(provider)?;
    let token_url = provider_token_url(provider)?;
    let mut params = vec![
        ("grant_type", "authorization_code".to_string()),
        ("client_id", client_id),
        ("code", code.trim().to_string()),
        ("redirect_uri", redirect_uri.trim().to_string()),
        ("code_verifier", code_verifier.trim().to_string()),
    ];
    if let Some(client_secret) = provider_client_secret(provider) {
        params.push(("client_secret", client_secret));
    }

    exchange_token_request(client, &token_url, &params, "Qwen OAuth token exchange").await
}

async fn exchange_refresh_token(
    client: &reqwest::Client,
    provider: &OAuthProviderConfig,
    refresh_token: &str,
) -> Result<OAuthTokenResponse, String> {
    let client_id = provider_client_id(provider)?;
    let token_url = provider_token_url(provider)?;
    let mut params = vec![
        ("grant_type", "refresh_token".to_string()),
        ("client_id", client_id),
        ("refresh_token", refresh_token.trim().to_string()),
    ];
    if let Some(client_secret) = provider_client_secret(provider) {
        params.push(("client_secret", client_secret));
    }

    exchange_token_request(client, &token_url, &params, "Qwen OAuth token refresh").await
}

async fn exchange_token_request(
    client: &reqwest::Client,
    token_url: &str,
    params: &[(&str, String)],
    operation: &str,
) -> Result<OAuthTokenResponse, String> {
    let form = params
        .iter()
        .map(|(key, value)| (*key, value.as_str()))
        .collect::<Vec<_>>();
    let resp = client
        .post(token_url)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "application/json")
        .form(&form)
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .map_err(|e| format!("{} failed: {}", operation, e))?;

    let status = resp.status();
    let body = resp.text().await.map_err(|e| e.to_string())?;
    if !status.is_success() {
        return Err(format!(
            "{} failed ({}): {}",
            operation,
            status,
            describe_error_body(&body)
        ));
    }

    let token_resp: OAuthTokenResponse = serde_json::from_str(&body).map_err(|e| {
        format!(
            "{} returned invalid JSON: {}",
            operation,
            compact_error_body(&body, 240).unwrap_or_else(|| e.to_string())
        )
    })?;
    if token_resp.access_token.trim().is_empty() {
        return Err(format!(
            "{} succeeded but did not return an access token",
            operation
        ));
    }

    Ok(token_resp)
}

fn persist_refreshed_account(
    state: &crate::AppState,
    account: &QwenAccount,
    refreshed: &RefreshResult,
) -> Result<(), String> {
    let Some(file_name) = account.file_name.as_ref() else {
        return Ok(());
    };

    let auth_dir = state
        .cfg
        .auth_dir
        .clone()
        .unwrap_or_else(|| "/root/dev/yow/io-gateway/auths".to_string());
    let path = std::path::Path::new(&auth_dir).join(file_name);
    let data = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
    let mut value: serde_json::Value = serde_json::from_str(&data).map_err(|e| e.to_string())?;

    if let serde_json::Value::Object(map) = &mut value {
        map.insert(
            "access_token".to_string(),
            serde_json::Value::String(refreshed.access_token.clone()),
        );
        map.insert(
            "expired".to_string(),
            serde_json::Value::String(refreshed.expired_at.clone()),
        );
        map.insert(
            "last_refresh".to_string(),
            serde_json::Value::String(Utc::now().to_rfc3339()),
        );
        map.insert(
            "email".to_string(),
            serde_json::Value::String(
                refreshed
                    .email
                    .clone()
                    .unwrap_or_else(|| refreshed.label.clone()),
            ),
        );
        map.insert(
            "label".to_string(),
            serde_json::Value::String(refreshed.label.clone()),
        );
        map.insert(
            "account_id".to_string(),
            serde_json::Value::String(refreshed.account_id.clone()),
        );
        if let Some(subject) = refreshed.subject.clone() {
            map.insert("subject".to_string(), serde_json::Value::String(subject));
        }
        if let Some(refresh_token) = refreshed.refresh_token.as_ref() {
            map.insert(
                "refresh_token".to_string(),
                serde_json::Value::String(refresh_token.clone()),
            );
        }
        if let Some(resource_url) = refreshed.resource_url.as_ref() {
            map.insert(
                "resource_url".to_string(),
                serde_json::Value::String(resource_url.clone()),
            );
        }
        if let Some(role) = refreshed.role.as_ref() {
            map.insert("role".to_string(), serde_json::Value::String(role.clone()));
        }
        if let Some(tier) = refreshed.tier.as_ref() {
            map.insert("tier".to_string(), serde_json::Value::String(tier.clone()));
        }
        if let Some(token_type) = refreshed.token_type.as_ref() {
            map.insert(
                "token_type".to_string(),
                serde_json::Value::String(token_type.clone()),
            );
        }
        if let Some(profile_image_url) = refreshed.profile_image_url.as_ref() {
            map.insert(
                "profile_image_url".to_string(),
                serde_json::Value::String(profile_image_url.clone()),
            );
        }
        if let Some(permissions) = refreshed.permissions.as_ref() {
            map.insert("permissions".to_string(), permissions.clone());
        }
        if let Some(expires_at_unix) = refreshed.expires_at_unix {
            map.insert("expires_at".to_string(), serde_json::json!(expires_at_unix));
        }
        let auth_method = map
            .get("auth_method")
            .and_then(|value| value.as_str())
            .map(|value| value.to_string())
            .unwrap_or_else(|| "browser_token_validate".to_string());
        map.insert(
            "auth_method".to_string(),
            serde_json::Value::String(auth_method),
        );
    }

    super::super::atomic_write_json(&path, &value)?;

    let mut accounts = state.qwen_accounts.lock().unwrap();
    if let Some(current) = accounts
        .iter_mut()
        .find(|current| current.file_name.as_deref() == Some(file_name.as_str()))
    {
        current.access_token = Some(refreshed.access_token.clone());
        current.expired_at = Some(refreshed.expired_at.clone());
        current.email = refreshed
            .email
            .clone()
            .unwrap_or_else(|| refreshed.label.clone());
        current.label = refreshed.label.clone();
        current.account_id = refreshed.account_id.clone();
        current.subject = refreshed.subject.clone();
        if let Some(refresh_token) = refreshed.refresh_token.as_ref() {
            current.refresh_token = refresh_token.clone();
        }
        if let Some(resource_url) = refreshed.resource_url.as_ref() {
            current.resource_url = Some(resource_url.clone());
        }
    }

    Ok(())
}

fn normalized_identity_from_validated_session(
    session: &ValidatedSession,
    access_token: &str,
    fallback_label: &str,
) -> Result<QwenIdentity, String> {
    let token_identity = identity_from_access_token(access_token, fallback_label);
    let subject = trimmed_non_empty(Some(session.id.as_str()))
        .or_else(|| token_identity.subject.clone())
        .ok_or_else(|| "Qwen identity payload did not include a subject/user id".to_string())?;
    let email = trimmed_non_empty(session.email.as_deref()).or(token_identity.email.clone());
    let name = trimmed_non_empty(session.name.as_deref());
    let label = email
        .clone()
        .or(name)
        .or_else(|| {
            if token_identity.label.trim() != fallback_label {
                Some(token_identity.label.clone())
            } else {
                None
            }
        })
        .unwrap_or_else(|| subject.clone());
    let file_key = email.clone().unwrap_or_else(|| subject.clone());

    Ok(QwenIdentity {
        account_id: subject.clone(),
        email,
        subject: Some(subject),
        label,
        file_key,
    })
}

fn parse_access_token_exp(access_token: &str) -> Option<i64> {
    parse_access_token_claims(access_token)?
        .get("exp")
        .and_then(|value| value.as_i64())
}

fn parse_access_token_claims(access_token: &str) -> Option<serde_json::Value> {
    let mut parts = access_token.split('.');
    let _header = parts.next()?;
    let payload = parts.next()?;
    let _sig = parts.next()?;
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    serde_json::from_slice(&decoded).ok()
}

fn looks_like_jwt(value: &str) -> bool {
    parse_access_token_claims(value).is_some()
}

fn unix_to_rfc3339(value: i64) -> Option<String> {
    chrono::DateTime::from_timestamp(value, 0).map(|dt| dt.to_rfc3339())
}

fn parse_rfc3339(value: &str) -> Option<chrono::DateTime<Utc>> {
    chrono::DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

fn trimmed_non_empty(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| value.to_string())
}

fn sanitize_label(value: &str) -> String {
    value
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn describe_error_body(body: &str) -> String {
    serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|value| {
            value
                .get("detail")
                .and_then(|detail| detail.as_str())
                .map(|detail| detail.to_string())
                .or_else(|| {
                    value
                        .get("message")
                        .and_then(|message| message.as_str())
                        .map(|message| message.to_string())
                })
        })
        .or_else(|| compact_error_body(body, 240))
        .unwrap_or_else(|| "empty response body".to_string())
}

fn qwen_provider_config(cfg: Option<&crate::Config>) -> OAuthProviderConfig {
    provider_config(cfg, OAuthProvider::Qwen)
}

fn provider_client_id(provider: &OAuthProviderConfig) -> Result<String, String> {
    provider
        .client_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| value.to_string())
        .ok_or_else(|| "oauth.providers.qwen.client_id is required".to_string())
}

fn provider_client_secret(provider: &OAuthProviderConfig) -> Option<String> {
    provider
        .client_secret
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| value.to_string())
}

fn provider_redirect_uri(
    cfg: &crate::Config,
    provider: &OAuthProviderConfig,
) -> Result<String, String> {
    if let Some(redirect_uri) = provider
        .redirect_uri
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| value.to_string())
    {
        return Ok(redirect_uri);
    }

    derive_local_redirect_uri(&cfg.listen)
}

fn provider_authorize_url(provider: &OAuthProviderConfig) -> Result<String, String> {
    provider
        .authorize_url
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| value.to_string())
        .ok_or_else(|| "oauth.providers.qwen.authorize_url is required".to_string())
}

fn provider_token_url(provider: &OAuthProviderConfig) -> Result<String, String> {
    provider
        .token_url
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| value.to_string())
        .ok_or_else(|| "oauth.providers.qwen.token_url is required".to_string())
}

fn derive_local_redirect_uri(listen: &str) -> Result<String, String> {
    let addr = listen.parse::<SocketAddr>().map_err(|_| {
        "oauth.providers.qwen.redirect_uri is required when listen is invalid".to_string()
    })?;
    let host = match addr.ip() {
        IpAddr::V4(ip) if ip.is_unspecified() => IpAddr::V4(Ipv4Addr::LOCALHOST),
        IpAddr::V6(ip) if ip.is_unspecified() => IpAddr::V4(Ipv4Addr::LOCALHOST),
        ip => ip,
    };
    Ok(format!(
        "http://{}:{}/login/qwen/callback",
        host,
        addr.port()
    ))
}

fn provider_base_url(provider: &OAuthProviderConfig) -> String {
    normalize_base_url(provider.base_url.as_deref())
}

fn provider_validate_url(provider: &OAuthProviderConfig) -> Result<String, String> {
    provider
        .validate_url
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| value.to_string())
        .or_else(|| {
            provider
                .session_url
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(|value| value.to_string())
        })
        .ok_or_else(|| "oauth.providers.qwen.validate_url is required".to_string())
}

fn provider_refresh_url(provider: &OAuthProviderConfig) -> Result<String, String> {
    provider
        .refresh_url
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| value.to_string())
        .or_else(|| {
            provider
                .session_url
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(|value| value.to_string())
        })
        .or_else(|| {
            provider
                .validate_url
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(|value| value.to_string())
        })
        .ok_or_else(|| "oauth.providers.qwen.refresh_url is required".to_string())
}

fn generate_code_verifier() -> String {
    rand::rng()
        .sample_iter(&Alphanumeric)
        .take(64)
        .map(char::from)
        .collect()
}

fn code_challenge(code_verifier: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(code_verifier.as_bytes());
    let digest = hasher.finalize();
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest)
}

fn resource_to_base_url(resource_url: Option<&str>, provider: &OAuthProviderConfig) -> String {
    if resource_url
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .is_some()
    {
        return normalize_base_url(resource_url);
    }

    provider_base_url(provider)
}

pub fn normalize_resource_url(resource_url: Option<&str>) -> Option<String> {
    resource_url
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| normalize_base_url(Some(value)))
}

fn normalize_base_url(base_url: Option<&str>) -> String {
    let Some(base_url) = base_url.map(str::trim).filter(|value| !value.is_empty()) else {
        return QWEN_PORTAL_BASE_URL.to_string();
    };

    let trimmed = base_url.trim_end_matches('/');
    let lower = trimmed.to_ascii_lowercase();

    if matches!(
        lower.as_str(),
        "https://chat.qwen.ai"
            | "https://chat.qwen.ai/api/v1"
            | "chat.qwen.ai"
            | "chat.qwen.ai/api/v1"
            | "https://portal.qwen.ai"
            | "portal.qwen.ai"
    ) {
        return QWEN_PORTAL_BASE_URL.to_string();
    }

    if lower == QWEN_PORTAL_BASE_URL {
        return QWEN_PORTAL_BASE_URL.to_string();
    }

    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        return trimmed.to_string();
    }

    if trimmed.contains('/') {
        return format!("https://{}", trimmed);
    }

    format!("https://{}/v1", trimmed)
}

fn compact_error_body(body: &str, max_len: usize) -> Option<String> {
    let compact = body
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .trim()
        .to_string();
    if compact.is_empty() {
        return None;
    }
    if compact.len() <= max_len {
        return Some(compact);
    }
    Some(format!("{}...", &compact[..max_len]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::target::qwen::accounts;
    use axum::{
        extract::State,
        http::{header, HeaderMap, StatusCode},
        response::{IntoResponse, Response},
        routing::get,
        Json, Router,
    };
    use serde_json::{json, Value};
    use std::{
        collections::{HashMap, HashSet},
        path::{Path, PathBuf},
        sync::{Arc, Mutex},
    };
    use tokio::task::JoinHandle;

    #[test]
    fn normalized_identity_prefers_validated_session_fields() {
        let token = token_with_claims(serde_json::json!({
            "sub": "token-subject",
            "email": "token@example.com"
        }));
        let session = ValidatedSession {
            id: "session-subject".to_string(),
            email: Some("session@example.com".to_string()),
            name: Some("Session User".to_string()),
            role: None,
            profile_image_url: None,
            tier: None,
            token: token.clone(),
            token_type: Some("Bearer".to_string()),
            expires_at: Some(1_800_000_000),
            permissions: None,
        };

        let identity =
            normalized_identity_from_validated_session(&session, &token, "fallback-label").unwrap();

        assert_eq!(identity.account_id, "session-subject");
        assert_eq!(identity.subject.as_deref(), Some("session-subject"));
        assert_eq!(identity.email.as_deref(), Some("session@example.com"));
        assert_eq!(identity.label, "session@example.com");
        assert_eq!(identity.file_key, "session@example.com");
    }

    #[test]
    fn normalized_identity_falls_back_to_token_claims() {
        let token = token_with_claims(serde_json::json!({
            "sub": "token-subject",
            "preferred_username": "token-user"
        }));
        let session = ValidatedSession {
            id: "   ".to_string(),
            email: None,
            name: None,
            role: None,
            profile_image_url: None,
            tier: None,
            token: token.clone(),
            token_type: Some("Bearer".to_string()),
            expires_at: Some(1_800_000_000),
            permissions: None,
        };

        let identity =
            normalized_identity_from_validated_session(&session, &token, "fallback-label").unwrap();

        assert_eq!(identity.account_id, "token-subject");
        assert_eq!(identity.subject.as_deref(), Some("token-subject"));
        assert_eq!(identity.email, None);
        assert_eq!(identity.label, "token-user");
        assert_eq!(identity.file_key, "token-subject");
    }

    #[test]
    fn normalized_identity_requires_subject() {
        let session = ValidatedSession {
            id: "".to_string(),
            email: Some("partial@example.com".to_string()),
            name: None,
            role: None,
            profile_image_url: None,
            tier: None,
            token: "opaque-token".to_string(),
            token_type: None,
            expires_at: None,
            permissions: None,
        };

        let err =
            normalized_identity_from_validated_session(&session, "opaque-token", "fallback-label")
                .unwrap_err();

        assert_eq!(
            err,
            "Qwen identity payload did not include a subject/user id"
        );
    }

    #[test]
    fn normalize_base_url_maps_legacy_chat_host_to_portal_host() {
        assert_eq!(
            normalize_resource_url(Some("https://chat.qwen.ai/api/v1")).as_deref(),
            Some("https://portal.qwen.ai/v1")
        );
        assert_eq!(
            normalize_resource_url(Some("chat.qwen.ai/api/v1")).as_deref(),
            Some("https://portal.qwen.ai/v1")
        );
        assert_eq!(
            normalize_resource_url(Some("https://portal.qwen.ai")).as_deref(),
            Some("https://portal.qwen.ai/v1")
        );
    }

    #[tokio::test]
    async fn browser_token_refresh_preserves_original_refresh_token_across_session_refreshes() {
        let browser_token = token_with_claims(json!({
            "sub": "browser-refresh-subject",
            "email": "browser-refresh@example.com",
            "exp": Utc::now().timestamp() + 86_400
        }));
        let server = MockRefreshServer::spawn(browser_token.clone()).await;
        let ctx = TestContext::new(&server.base_url);

        let (saved_path, label) = validate_and_save_auth(&ctx.state, &browser_token)
            .await
            .unwrap();
        assert_eq!(label, "qwen@example.com");

        let saved = read_auth_json(&saved_path);
        assert_eq!(saved["refresh_token"], Value::String(browser_token.clone()));
        assert_eq!(
            ctx.state.qwen_accounts.lock().unwrap()[0].refresh_token,
            browser_token
        );

        force_account_expired(&ctx.state, &saved_path);
        let first_account = accounts::first_enabled(&ctx.state).unwrap();
        let refreshed_access_token = ensure_access_token(&ctx.state, &first_account)
            .await
            .unwrap();
        let first_saved = read_auth_json(&saved_path);
        let first_state_account = accounts::first_enabled(&ctx.state).unwrap();

        assert_eq!(
            first_saved["refresh_token"],
            Value::String(browser_token.clone())
        );
        assert_eq!(first_state_account.refresh_token, browser_token);
        assert_eq!(
            first_saved["access_token"],
            Value::String(refreshed_access_token.clone())
        );
        assert_eq!(
            first_state_account.access_token.as_deref(),
            Some(refreshed_access_token.as_str())
        );

        force_account_expired(&ctx.state, &saved_path);
        let second_account = accounts::first_enabled(&ctx.state).unwrap();
        let refreshed_access_token_2 = ensure_access_token(&ctx.state, &second_account)
            .await
            .unwrap();
        let second_saved = read_auth_json(&saved_path);
        let second_state_account = accounts::first_enabled(&ctx.state).unwrap();

        assert_eq!(
            second_saved["refresh_token"],
            Value::String(browser_token.clone())
        );
        assert_eq!(second_state_account.refresh_token, browser_token);
        assert_eq!(
            second_saved["access_token"],
            Value::String(refreshed_access_token_2.clone())
        );
        assert_eq!(
            second_state_account.access_token.as_deref(),
            Some(refreshed_access_token_2.as_str())
        );
        assert_ne!(refreshed_access_token, refreshed_access_token_2);

        let validate_tokens = server.validate_tokens.lock().unwrap().clone();
        assert_eq!(
            validate_tokens,
            vec![
                browser_token.clone(),
                browser_token.clone(),
                browser_token.clone()
            ]
        );
    }

    struct TestContext {
        auth_dir: PathBuf,
        state: crate::AppState,
    }

    impl TestContext {
        fn new(base_url: &str) -> Self {
            let auth_dir = unique_test_dir();
            let cfg = crate::Config {
                listen: "127.0.0.1:39010".to_string(),
                upstream_base: "http://unused-upstream".to_string(),
                proxy_api_key: "test-proxy-key".to_string(),
                tokens: vec![],
                auth_dir: Some(auth_dir.to_string_lossy().to_string()),
                disabled_files: None,
                admin_auth: crate::admin_auth::AdminAuthConfig::default(),
                request_body_limit_enabled: crate::default_request_body_limit_enabled(),
                max_request_body_bytes: crate::default_max_request_body_bytes(),
                max_concurrent_requests: crate::default_max_concurrent_requests(),
                trusted_proxy: false,
                history_retention_days: crate::default_history_retention_days(),
                history_max_entries: crate::default_history_max_entries(),
                upstream_connect_timeout_seconds: crate::default_upstream_connect_timeout_seconds(),
                upstream_read_timeout_seconds: crate::default_upstream_read_timeout_seconds(),
                upstream_first_event_timeout_seconds:
                    crate::default_upstream_first_event_timeout_seconds(),
                oauth: super::super::super::oauth::OAuthConfig {
                    providers: super::super::super::oauth::OAuthProvidersConfig {
                        qwen: super::super::super::oauth::OAuthProviderOverride {
                            client_id: Some("test-qwen-client".to_string()),
                            client_secret: Some("test-qwen-secret".to_string()),
                            redirect_uri: None,
                            authorize_url: Some(format!("{}/oauth/authorize", base_url)),
                            token_url: Some(format!("{}/oauth2/token", base_url)),
                            device_code_url: None,
                            validate_url: Some(format!("{}/auths/", base_url)),
                            refresh_url: Some(format!("{}/auths/", base_url)),
                            session_url: Some(format!("{}/auths/", base_url)),
                            base_url: Some(format!("{}/api/v1", base_url)),
                            scopes: Some(vec![
                                "openid".to_string(),
                                "profile".to_string(),
                                "email".to_string(),
                                "model.completion".to_string(),
                            ]),
                        },
                        ..Default::default()
                    },
                },
            };
            let state = crate::AppState {
                cfg: Arc::new(cfg),
                config_path: PathBuf::from("config.json"),
                rr: Arc::new(Mutex::new(0)),
                agw_rr: Arc::new(Mutex::new(0)),
                gemini_rr: Arc::new(Mutex::new(0)),
                qwen_rr: Arc::new(Mutex::new(0)),
                deepseek_rr: Arc::new(Mutex::new(0)),
                grok_rr: Arc::new(Mutex::new(0)),
                minimax_rr: Arc::new(Mutex::new(0)),
                copilot_rr: Arc::new(Mutex::new(0)),
                claude_rr: Arc::new(Mutex::new(0)),
                glm_rr: Arc::new(Mutex::new(0)),
                custom_model_rr: Arc::new(Mutex::new(HashMap::new())),
                client: reqwest::Client::builder().build().unwrap(),
                tokens: Arc::new(Mutex::new(Vec::new())),
                agw_accounts: Arc::new(Mutex::new(Vec::new())),
                gemini_accounts: Arc::new(Mutex::new(Vec::new())),
                qwen_accounts: Arc::new(Mutex::new(Vec::new())),
                deepseek_accounts: Arc::new(Mutex::new(Vec::new())),
                grok_accounts: Arc::new(Mutex::new(Vec::new())),
                minimax_accounts: Arc::new(Mutex::new(Vec::new())),
                minimax_video_tasks: Arc::new(Mutex::new(HashMap::new())),
                copilot_accounts: Arc::new(Mutex::new(Vec::new())),
                claude_accounts: Arc::new(Mutex::new(Vec::new())),
                glm_accounts: Arc::new(Mutex::new(Vec::new())),
                custom_models: Arc::new(Mutex::new(Vec::new())),
                stats: Arc::new(Mutex::new(crate::UsageStats::default())),
                persisted_stats: Arc::new(Mutex::new(crate::stats_store::StatsStore::default())),
                quota_cache: Arc::new(Mutex::new(Vec::new())),
                agw_quota_cache: Arc::new(Mutex::new(HashMap::new())),
                gemini_quota_cache: Arc::new(Mutex::new(HashMap::new())),
                qwen_quota_cache: Arc::new(Mutex::new(HashMap::new())),
                minimax_quota_cache: Arc::new(Mutex::new(HashMap::new())),
                deepseek_quota_cache: Arc::new(Mutex::new(HashMap::new())),
                claude_quota_cache: Arc::new(Mutex::new(HashMap::new())),
                glm_quota_cache: Arc::new(Mutex::new(HashMap::new())),
                quota_snapshots: Arc::new(Mutex::new(HashMap::new())),
                oauth_pending: Arc::new(Mutex::new(HashMap::new())),
                agw_oauth_pending: Arc::new(Mutex::new(HashMap::new())),
                gemini_oauth_pending: Arc::new(Mutex::new(HashMap::new())),
                qwen_oauth_pending: Arc::new(Mutex::new(HashMap::new())),
                grok_oauth_pending: Arc::new(Mutex::new(HashMap::new())),
                copilot_oauth_pending: Arc::new(Mutex::new(HashMap::new())),
                claude_oauth_pending: Arc::new(Mutex::new(HashMap::new())),
                admin_sessions: Arc::new(Mutex::new(HashMap::new())),
                admin_login_attempts: Arc::new(Mutex::new(HashMap::new())),
                api_keys: Arc::new(Mutex::new(crate::api_keys::ApiKeyStore::default())),
                api_key_cache: Arc::new(std::sync::RwLock::new(HashMap::new())),
                api_key_last_used: Arc::new(Mutex::new(HashMap::new())),
                request_api_key_id: None,
                internal_proxy_secret: Arc::new("test-internal-proxy-secret".to_string()),
                notification_settings: Arc::new(Mutex::new(
                    crate::notifications::NotificationSettings::default(),
                )),
                account_routing: Arc::new(Mutex::new(crate::AccountRoutingSettings::default())),
                disabled: Arc::new(Mutex::new(HashSet::new())),
                persistence_tx: std::sync::mpsc::channel().0,
                account_router: Arc::new(Mutex::new(HashMap::new())),
                account_refresh_locks: Arc::new(Mutex::new(HashMap::new())),
            };

            Self { auth_dir, state }
        }
    }

    impl Drop for TestContext {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.auth_dir);
        }
    }

    #[derive(Clone)]
    struct MockRefreshServerState {
        browser_token: String,
        validate_tokens: Arc<Mutex<Vec<String>>>,
        validate_calls: Arc<Mutex<u32>>,
    }

    struct MockRefreshServer {
        base_url: String,
        validate_tokens: Arc<Mutex<Vec<String>>>,
        handle: JoinHandle<()>,
    }

    impl MockRefreshServer {
        async fn spawn(browser_token: String) -> Self {
            let validate_tokens = Arc::new(Mutex::new(Vec::new()));
            let state = MockRefreshServerState {
                browser_token,
                validate_tokens: validate_tokens.clone(),
                validate_calls: Arc::new(Mutex::new(0)),
            };
            let app = Router::new()
                .route("/auths/", get(mock_refresh_validate_handler))
                .with_state(state);
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let handle = tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });

            Self {
                base_url: format!("http://{}", addr),
                validate_tokens,
                handle,
            }
        }
    }

    impl Drop for MockRefreshServer {
        fn drop(&mut self) {
            self.handle.abort();
        }
    }

    async fn mock_refresh_validate_handler(
        State(state): State<MockRefreshServerState>,
        headers: HeaderMap,
    ) -> Response {
        let token = headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
            .unwrap_or_default()
            .to_string();
        state.validate_tokens.lock().unwrap().push(token.clone());

        if token != state.browser_token {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({ "message": "unexpected refresh token" })),
            )
                .into_response();
        }

        let call_number = {
            let mut calls = state.validate_calls.lock().unwrap();
            *calls += 1;
            *calls
        };
        let access_token = token_with_claims(json!({
            "sub": "qwen-sub-123",
            "email": "qwen@example.com",
            "exp": Utc::now().timestamp() + 3600,
            "refresh_call": call_number
        }));

        Json(json!({
            "id": "qwen-sub-123",
            "email": "qwen@example.com",
            "name": "Qwen Example",
            "role": "user",
            "tier": "plus",
            "access_token": access_token,
            "token_type": "Bearer",
            "expires_at": Utc::now().timestamp() + 3600,
            "permissions": {
                "chat": {
                    "edit": true
                }
            }
        }))
        .into_response()
    }

    fn force_account_expired(state: &crate::AppState, saved_path: &str) {
        let expired_at = (Utc::now() - chrono::Duration::minutes(5)).to_rfc3339();
        let expires_at = Utc::now().timestamp() - 300;
        let mut saved = read_auth_json(saved_path);
        saved["expired"] = Value::String(expired_at.clone());
        saved["expires_at"] = json!(expires_at);
        std::fs::write(saved_path, serde_json::to_vec_pretty(&saved).unwrap()).unwrap();

        let file_name = Path::new(saved_path)
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap();
        let mut accounts = state.qwen_accounts.lock().unwrap();
        let current = accounts
            .iter_mut()
            .find(|account| account.file_name.as_deref() == Some(file_name))
            .unwrap();
        current.expired_at = Some(expired_at);
    }

    fn read_auth_json(saved_path: &str) -> Value {
        serde_json::from_str(&std::fs::read_to_string(saved_path).unwrap()).unwrap()
    }

    fn unique_test_dir() -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "io-gateway-qwen-auth-tests-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn token_with_claims(claims: serde_json::Value) -> String {
        let header = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(r#"{"alg":"none"}"#);
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&claims).unwrap());
        format!("{}.{}.", header, payload)
    }
}
