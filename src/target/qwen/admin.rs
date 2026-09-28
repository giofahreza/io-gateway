use axum::{
    body::Bytes,
    extract::{Form, Query, State},
    http::{HeaderMap, HeaderValue, Method, StatusCode, Uri},
    response::{Html, IntoResponse, Response},
};
use serde::Deserialize;
use std::collections::HashMap;

use super::super::oauth::OAuthProvider;

#[derive(Deserialize)]
struct LoginStartRequest {
    token: String,
}

#[derive(Deserialize)]
pub struct CallbackForm {
    pub redirect_url: String,
}

#[derive(Deserialize)]
pub struct LoginStatusQuery {
    pub state: Option<String>,
}

pub async fn accounts_json(State(state): State<crate::AppState>) -> impl IntoResponse {
    let usage_by_key = {
        let stats = state.stats.lock().unwrap();
        stats
            .qwen_accounts
            .iter()
            .map(|usage| {
                (
                    usage.key.clone(),
                    (
                        usage.requests,
                        usage.errors,
                        usage.prompt_total,
                        usage.input_tokens,
                        usage.output_tokens,
                        usage.total_tokens,
                        usage.cache_tokens,
                        usage.reasoning_tokens,
                        usage.last_success_at.clone(),
                        usage.last_error_at.clone(),
                        usage.last_error_message.clone(),
                    ),
                )
            })
            .collect::<std::collections::HashMap<_, _>>()
    };

    let accounts = state
        .qwen_accounts
        .lock()
        .unwrap()
        .iter()
        .map(|account| {
            let stats_key = crate::qwen_stats_key(account);
            let runtime =
                crate::router_account_runtime_json(&state, "qwen", &stats_key, account.enabled);
            let (
                requests,
                errors,
                prompt_total,
                input_tokens,
                output_tokens,
                total_tokens,
                cache_tokens,
                reasoning_tokens,
                last_success_at,
                last_error_at,
                last_error_message,
            ) = usage_by_key
                .get(&stats_key)
                .cloned()
                .unwrap_or((0, 0, 0, 0, 0, 0, 0, 0, None, None, None));
            serde_json::json!({
                "account_id": account.account_id,
                "label": account.label,
                "email": account.email,
                "subject": account.subject,
                "file_name": account.file_name,
                "enabled": account.enabled,
                "runtime": runtime,
                "resource_url": account.resource_url,
                "expired_at": account.expired_at,
                "requests": requests,
                "errors": errors,
                "prompt_total": prompt_total,
                "input_tokens": input_tokens,
                "output_tokens": output_tokens,
                "total_tokens": total_tokens,
                "cache_tokens": cache_tokens,
                "reasoning_tokens": reasoning_tokens,
                "last_success_at": last_success_at,
                "last_error_at": last_error_at,
                "last_error_message": last_error_message
            })
        })
        .collect::<Vec<_>>();

    axum::Json(serde_json::json!({ "accounts": accounts }))
}

pub async fn login_start(
    State(state): State<crate::AppState>,
    method: Method,
    body: Bytes,
) -> impl IntoResponse {
    match method {
        Method::GET => Html(browser_token_helper_html()).into_response(),
        Method::POST => submit_browser_token(&state, &body).await,
        _ => (
            StatusCode::METHOD_NOT_ALLOWED,
            "Qwen login start only supports GET for the browser-token helper or POST for direct token submission",
        )
            .into_response(),
    }
}

pub async fn login_submit(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    Form(form): Form<CallbackForm>,
) -> impl IntoResponse {
    let redirect_url = form.redirect_url.trim();
    if redirect_url.is_empty() {
        return axum::Json(serde_json::json!({
            "ok": false,
            "message": "redirect_url is required"
        }))
        .into_response();
    }

    let query = match super::auth::parse_oauth_callback_url(redirect_url) {
        Ok(query) => query,
        Err(err) => return callback_response(StatusCode::BAD_REQUEST, &err, false),
    };

    complete_oauth_callback(&state, &headers, query).await
}

pub async fn login_status(
    State(state): State<crate::AppState>,
    Query(query): Query<LoginStatusQuery>,
) -> impl IntoResponse {
    let state_token = match query
        .state
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        Some(state_token) => state_token.to_string(),
        None => {
            return axum::Json(serde_json::json!({
                "ok": false,
                "status": "invalid",
                "message": "state is required"
            }))
            .into_response();
        }
    };

    let mut pending = state.qwen_oauth_pending.lock().unwrap();
    prune_expired_pending(&mut pending);
    let Some(entry) = pending.get(&state_token) else {
        return axum::Json(serde_json::json!({
            "ok": false,
            "status": "invalid",
            "message": "invalid or expired state"
        }))
        .into_response();
    };

    match &entry.status {
        super::auth::PendingStatus::Pending => axum::Json(serde_json::json!({
            "ok": true,
            "status": "pending",
            "message": "Waiting for Qwen OAuth callback"
        }))
        .into_response(),
        super::auth::PendingStatus::Completed { saved_path, label } => {
            axum::Json(serde_json::json!({
                "ok": true,
                "status": "completed",
                "message": format!("saved Qwen credentials to {}", saved_path),
                "saved_path": saved_path,
                "label": label
            }))
            .into_response()
        }
        super::auth::PendingStatus::Error { message } => axum::Json(serde_json::json!({
            "ok": false,
            "status": "error",
            "message": message
        }))
        .into_response(),
    }
}

pub async fn login_callback_from_uri(
    state: crate::AppState,
    method: Method,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    if method != Method::GET {
        return callback_response(
            StatusCode::METHOD_NOT_ALLOWED,
            "Qwen OAuth callback only supports GET",
            true,
        );
    }

    let query = match super::auth::parse_callback_query_from_uri(&uri) {
        Ok(query) => query,
        Err(err) => return callback_response(StatusCode::BAD_REQUEST, &err, true),
    };

    complete_oauth_callback(&state, &headers, query).await
}

async fn complete_oauth_callback(
    state: &crate::AppState,
    headers: &HeaderMap,
    query: super::auth::OAuthCallbackQuery,
) -> Response {
    if let Some(message) = super::auth::callback_error_message(&query) {
        mark_pending_error(state, query.state.as_deref(), &message);
        return callback_response(StatusCode::BAD_REQUEST, &message, true);
    }

    let (code, state_token) = match super::auth::extract_callback_code_state(&query) {
        Ok(values) => values,
        Err(err) => return callback_response(StatusCode::BAD_REQUEST, &err, true),
    };

    if let Err(err) =
        super::super::oauth::validate_state_cookie(headers, OAuthProvider::Qwen, &state_token)
    {
        mark_pending_error(state, Some(state_token.as_str()), &err);
        return callback_response(StatusCode::BAD_REQUEST, &err, true);
    }

    let pending_auth = {
        let mut pending = state.qwen_oauth_pending.lock().unwrap();
        prune_expired_pending(&mut pending);
        let Some(entry) = pending.get(&state_token).cloned() else {
            return callback_response(StatusCode::BAD_REQUEST, "invalid or expired state", true);
        };
        match &entry.status {
            super::auth::PendingStatus::Pending => entry,
            super::auth::PendingStatus::Completed { saved_path, label } => {
                let message = format!(
                    "Qwen login already completed for {}. Credentials were saved to {}. You can close this tab.",
                    label, saved_path
                );
                return callback_response(StatusCode::OK, &message, true);
            }
            super::auth::PendingStatus::Error { message } => {
                return callback_response(StatusCode::BAD_REQUEST, message, true);
            }
        }
    };

    match super::auth::exchange_code_and_save_auth(state, &pending_auth, &code).await {
        Ok((saved_path, label)) => {
            mark_pending_completed(state, &state_token, &saved_path, &label);
            let message = format!(
                "Qwen login complete for {}. Credentials saved to {}. You can close this tab and return to the dashboard.",
                label, saved_path
            );
            callback_response(StatusCode::OK, &message, true)
        }
        Err(err) => {
            let message = format!("Qwen OAuth callback failed: {}", err);
            mark_pending_error(state, Some(state_token.as_str()), &message);
            callback_response(StatusCode::BAD_REQUEST, &message, true)
        }
    }
}

async fn submit_browser_token(state: &crate::AppState, body: &Bytes) -> Response {
    let token = match extract_token(body) {
        Ok(token) => token,
        Err(message) => {
            return axum::Json(serde_json::json!({
                "ok": false,
                "message": message
            }))
            .into_response();
        }
    };

    match super::auth::validate_and_save_auth(state, &token).await {
        Ok((saved_path, label)) => axum::Json(serde_json::json!({
            "ok": true,
            "message": format!("saved Qwen credentials to {}", saved_path),
            "saved_path": saved_path,
            "label": label
        }))
        .into_response(),
        Err(err) => axum::Json(serde_json::json!({
            "ok": false,
            "message": err
        }))
        .into_response(),
    }
}

fn prune_expired_pending(pending: &mut HashMap<String, super::auth::PendingOAuth>) {
    pending.retain(|_, entry| !super::auth::pending_is_expired(entry));
}

fn mark_pending_completed(
    state: &crate::AppState,
    state_token: &str,
    saved_path: &str,
    label: &str,
) {
    let mut pending = state.qwen_oauth_pending.lock().unwrap();
    prune_expired_pending(&mut pending);
    if let Some(entry) = pending.get_mut(state_token) {
        entry.status = super::auth::PendingStatus::Completed {
            saved_path: saved_path.to_string(),
            label: label.to_string(),
        };
    }
}

fn mark_pending_error(state: &crate::AppState, state_token: Option<&str>, message: &str) {
    let Some(state_token) = state_token.map(str::trim).filter(|value| !value.is_empty()) else {
        return;
    };

    let mut pending = state.qwen_oauth_pending.lock().unwrap();
    prune_expired_pending(&mut pending);
    if let Some(entry) = pending.get_mut(state_token) {
        entry.status = super::auth::PendingStatus::Error {
            message: message.to_string(),
        };
    }
}

fn callback_response(status: StatusCode, message: &str, clear_state_cookie: bool) -> Response {
    let mut response = (status, message.to_string()).into_response();
    if clear_state_cookie {
        append_set_cookie(
            &mut response,
            &super::super::oauth::clear_state_cookie(OAuthProvider::Qwen),
        );
    }
    response
}

fn append_set_cookie(response: &mut Response, cookie_value: &str) {
    if let Ok(header_value) = HeaderValue::from_str(cookie_value) {
        response
            .headers_mut()
            .append(axum::http::header::SET_COOKIE, header_value);
    }
}

fn extract_token(body: &Bytes) -> Result<String, String> {
    if body.is_empty() {
        return Err(
            "Submit a Qwen browser token as JSON: {\"token\":\"...\"}. Copy it from chat.qwen.ai localStorage.token."
                .to_string(),
        );
    }

    if let Ok(payload) = serde_json::from_slice::<LoginStartRequest>(body) {
        let token = payload.token.trim().to_string();
        if !token.is_empty() {
            return Ok(token);
        }
    }

    let raw = std::str::from_utf8(body)
        .map_err(|_| "Qwen token payload must be valid UTF-8".to_string())?
        .trim()
        .to_string();
    if raw.is_empty() {
        return Err("Qwen browser token is required".to_string());
    }

    Ok(raw)
}

fn browser_token_helper_html() -> String {
    let snippet = qwen_token_extractor_snippet();
    let snippet_json = serde_json::to_string(snippet).unwrap_or_else(|_| "\"\"".to_string());
    format!(
        r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>Qwen Browser Token Login</title>
  <style>
    body {{
      font-family: ui-sans-serif, system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif;
      background: #0f172a;
      color: #e2e8f0;
      margin: 0;
      padding: 32px 16px;
    }}
    .card {{
      max-width: 760px;
      margin: 0 auto;
      background: rgba(15, 23, 42, 0.94);
      border: 1px solid rgba(148, 163, 184, 0.25);
      border-radius: 18px;
      padding: 24px;
      box-shadow: 0 24px 60px rgba(2, 6, 23, 0.45);
    }}
    h1 {{
      margin-top: 0;
      font-size: 28px;
    }}
    p, li {{
      line-height: 1.6;
    }}
    a, button {{
      font: inherit;
    }}
    .actions {{
      display: flex;
      gap: 12px;
      flex-wrap: wrap;
      margin: 16px 0;
    }}
    .button {{
      display: inline-flex;
      align-items: center;
      justify-content: center;
      border: 0;
      border-radius: 10px;
      background: #38bdf8;
      color: #082f49;
      padding: 10px 16px;
      cursor: pointer;
      text-decoration: none;
      font-weight: 600;
    }}
    .button.secondary {{
      background: #1e293b;
      color: #e2e8f0;
      border: 1px solid rgba(148, 163, 184, 0.3);
    }}
    pre {{
      white-space: pre-wrap;
      overflow-wrap: anywhere;
      background: #020617;
      border: 1px solid rgba(148, 163, 184, 0.2);
      border-radius: 12px;
      padding: 16px;
      color: #bfdbfe;
    }}
    textarea {{
      width: 100%;
      min-height: 160px;
      box-sizing: border-box;
      border-radius: 12px;
      border: 1px solid rgba(148, 163, 184, 0.25);
      background: #020617;
      color: #e2e8f0;
      padding: 12px;
      font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace;
    }}
    #status {{
      margin-top: 12px;
      min-height: 24px;
      color: #cbd5e1;
    }}
  </style>
</head>
<body>
  <div class="card">
    <h1>Qwen Browser Token Login</h1>
    <p>This gateway now follows the <code>qwen-api</code> browser-token flow instead of sending you into Qwen's broken public OAuth authorize URL.</p>
    <ol>
      <li>Open <code>chat.qwen.ai</code> and sign in there.</li>
      <li>Open the browser devtools console on <code>chat.qwen.ai</code>.</li>
      <li>Paste the extractor snippet below. It reads <code>localStorage.getItem("token")</code> and copies it.</li>
      <li>Paste the copied token into the form below and save it back to this gateway.</li>
    </ol>
    <div class="actions">
      <a class="button" href="https://chat.qwen.ai" target="_blank" rel="noopener noreferrer">Open chat.qwen.ai</a>
      <button class="button secondary" type="button" onclick="copySnippet()">Copy Extractor</button>
    </div>
    <pre id="snippet"></pre>
    <h2>Paste Token</h2>
    <textarea id="tokenInput" placeholder="Paste the chat.qwen.ai browser token here"></textarea>
    <div class="actions">
      <button class="button" type="button" onclick="submitToken()">Save Token</button>
    </div>
    <div id="status"></div>
  </div>
  <script>
    const snippet = {snippet_json};
    document.getElementById('snippet').textContent = snippet;

    async function copySnippet() {{
      try {{
        await navigator.clipboard.writeText(snippet);
        document.getElementById('status').textContent = 'Copied the Qwen token extractor. Paste it into the chat.qwen.ai console.';
      }} catch (_) {{
        document.getElementById('status').textContent = 'Clipboard copy failed. Copy the snippet manually from the box above.';
      }}
    }}

    async function submitToken() {{
      const token = document.getElementById('tokenInput').value.trim();
      if (!token) {{
        document.getElementById('status').textContent = 'Paste the Qwen browser token first.';
        return;
      }}
      document.getElementById('status').textContent = 'Saving token...';
      try {{
        const response = await fetch('/login/qwen/start', {{
          method: 'POST',
          headers: {{ 'Content-Type': 'application/json' }},
          body: JSON.stringify({{ token }})
        }});
        const data = await response.json();
        document.getElementById('status').textContent = data.message || 'Failed to save Qwen token.';
        if (data.ok) {{
          document.getElementById('tokenInput').value = '';
        }}
      }} catch (error) {{
        document.getElementById('status').textContent = 'Failed to save token: ' + error;
      }}
    }}
  </script>
</body>
</html>
"#
    )
}

fn qwen_token_extractor_snippet() -> &'static str {
    r#"javascript:(function () {
  if (window.location.hostname !== "chat.qwen.ai") {
    alert("This code is for chat.qwen.ai");
    window.open("https://chat.qwen.ai", "_blank");
    return;
  }
  const token = localStorage.getItem("token");
  if (!token) {
    alert("qwen access_token not found");
    return;
  }
  navigator.clipboard.writeText(token).then(
    () => alert("Qwen access_token copied to clipboard"),
    () => prompt("Qwen access_token:", token)
  );
})();"#
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::target::{oauth, qwen::auth};
    use axum::{
        body::to_bytes,
        http::{header, StatusCode},
        response::Html,
        routing::{get, post},
        Json, Router,
    };
    use chrono::Utc;
    use serde_json::{json, Value};
    use std::{
        collections::HashSet,
        path::PathBuf,
        sync::{Arc, Mutex},
    };
    use tokio::task::JoinHandle;

    #[tokio::test]
    async fn qwen_login_start_returns_browser_token_helper_page() {
        let ctx = TestContext::new("http://unused-qwen-base");
        let response = login_start(State(ctx.state.clone()), Method::GET, Bytes::new())
            .await
            .into_response();
        let status = response.status();
        let content_type = response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap()
            .to_string();
        let body = response_text(response).await;

        assert_eq!(status, StatusCode::OK);
        assert!(content_type.starts_with("text/html"));
        assert!(body.contains("Qwen Browser Token Login"));
        assert!(body.contains("https://chat.qwen.ai"));
        assert!(body.contains("localStorage.getItem(\"token\")"));
        assert!(body.contains("fetch('/login/qwen/start'"));
    }

    #[tokio::test]
    async fn qwen_shared_callback_route_saves_auth_on_success() {
        let server = MockQwenServer::spawn().await;
        let ctx = TestContext::new(&server.base_url);
        let started = start_qwen_login(&ctx.state).await;
        let response = oauth::login_callback_route(
            ctx.state.clone(),
            "qwen".to_string(),
            Method::GET,
            cookie_headers(&started.cookie_pair),
            format!(
                "/login/qwen/callback?state={}&code=good-code",
                started.state_token
            )
            .parse()
            .unwrap(),
        )
        .await;
        let body = response_text(response).await;

        assert!(body.contains("Qwen login complete for qwen@example.com"));

        let token_requests = server.token_requests.lock().unwrap().clone();
        assert_eq!(token_requests.len(), 1);
        assert_eq!(
            token_requests[0].get("grant_type").map(String::as_str),
            Some("authorization_code")
        );
        assert_eq!(
            token_requests[0].get("client_id").map(String::as_str),
            Some("test-qwen-client")
        );
        assert_eq!(
            token_requests[0].get("client_secret").map(String::as_str),
            Some("test-qwen-secret")
        );
        assert_eq!(
            token_requests[0].get("code").map(String::as_str),
            Some("good-code")
        );
        assert_eq!(
            token_requests[0].get("redirect_uri").map(String::as_str),
            Some("http://127.0.0.1:39000/login/qwen/callback")
        );
        assert!(token_requests[0]
            .get("code_verifier")
            .map(|value| !value.trim().is_empty())
            .unwrap_or(false));

        let auth_path = ctx.auth_dir.join("qwen-qwen_example.com.json");
        let saved: Value =
            serde_json::from_str(&std::fs::read_to_string(&auth_path).unwrap()).unwrap();
        assert_eq!(saved["type"], Value::String("qwen".to_string()));
        assert_eq!(
            saved["auth_method"],
            Value::String("oauth_authorization_code".to_string())
        );
        assert_eq!(
            saved["account_id"],
            Value::String("qwen-sub-123".to_string())
        );
        assert_eq!(saved["subject"], Value::String("qwen-sub-123".to_string()));
        assert_eq!(
            saved["email"],
            Value::String("qwen@example.com".to_string())
        );
        assert_eq!(
            saved["access_token"],
            Value::String("issued-access-token".to_string())
        );
        assert_eq!(
            saved["refresh_token"],
            Value::String("issued-refresh-token".to_string())
        );
        assert_eq!(ctx.state.qwen_accounts.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn qwen_shared_callback_route_rejects_invalid_state() {
        let server = MockQwenServer::spawn().await;
        let ctx = TestContext::new(&server.base_url);
        let response = oauth::login_callback_route(
            ctx.state.clone(),
            "qwen".to_string(),
            Method::GET,
            cookie_headers("io_gateway_oauth_state_qwen=missing-state"),
            "/login/qwen/callback?state=missing-state&code=good-code"
                .parse()
                .unwrap(),
        )
        .await;
        let status = response.status();
        let body = response_text(response).await;

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body, "invalid or expired state");
    }

    #[tokio::test]
    async fn qwen_shared_callback_route_reports_upstream_token_failure() {
        let server = MockQwenServer::spawn().await;
        let ctx = TestContext::new(&server.base_url);
        let started = start_qwen_login(&ctx.state).await;
        let response = oauth::login_callback_route(
            ctx.state.clone(),
            "qwen".to_string(),
            Method::GET,
            cookie_headers(&started.cookie_pair),
            format!(
                "/login/qwen/callback?state={}&code=bad-code",
                started.state_token
            )
            .parse()
            .unwrap(),
        )
        .await;
        let status = response.status();
        let body = response_text(response).await;

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body.contains("Qwen OAuth token exchange failed"));
        assert!(body.contains("mock token exchange failure"));
    }

    #[tokio::test]
    async fn qwen_shared_callback_route_reports_upstream_profile_failure() {
        let server = MockQwenServer::spawn().await;
        let ctx = TestContext::new(&server.base_url);
        let started = start_qwen_login(&ctx.state).await;
        let response = oauth::login_callback_route(
            ctx.state.clone(),
            "qwen".to_string(),
            Method::GET,
            cookie_headers(&started.cookie_pair),
            format!(
                "/login/qwen/callback?state={}&code=profile-fail-code",
                started.state_token
            )
            .parse()
            .unwrap(),
        )
        .await;
        let status = response.status();
        let body = response_text(response).await;

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body.contains("Qwen token validation failed"));
        assert!(body.contains("mock profile failure"));
    }

    #[tokio::test]
    async fn shared_callback_route_leaves_existing_providers_on_submit_flow() {
        let server = MockQwenServer::spawn().await;
        let ctx = TestContext::new(&server.base_url);
        let response = oauth::login_callback_route(
            ctx.state.clone(),
            "codex".to_string(),
            Method::GET,
            HeaderMap::new(),
            "/login/codex/callback?state=test&code=test"
                .parse()
                .unwrap(),
        )
        .await;
        let status = response.status();
        let body = response_text(response).await;

        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(body.contains("/login/codex/submit"));
    }

    struct StartedLogin {
        state_token: String,
        cookie_pair: String,
    }

    struct TestContext {
        auth_dir: PathBuf,
        state: crate::AppState,
    }

    impl TestContext {
        fn new(base_url: &str) -> Self {
            let auth_dir = unique_test_dir();
            let cfg = crate::Config {
                listen: "127.0.0.1:39000".to_string(),
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
                codex_reset_credit_app_server: crate::CodexResetCreditAppServerConfig::default(),
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
                api_key_registry: crate::ApiKeyRegistry::new(
                    crate::api_keys::ApiKeyStore::default(),
                ),
                api_key_last_used: Arc::new(Mutex::new(HashMap::new())),
                request_api_key_id: None,
                request_source_api: crate::SourceApi::V1,
                request_api_key_budget: None,
                request_api_key_quota: None,
                request_api_key_audit: None,
                internal_proxy_secret: Arc::new("test-internal-proxy-secret".to_string()),
                notification_settings: Arc::new(Mutex::new(
                    crate::notifications::NotificationSettings::default(),
                )),
                account_routing: Arc::new(Mutex::new(crate::AccountRoutingSettings::default())),
                disabled: Arc::new(Mutex::new(HashSet::new())),
                persistence_tx: std::sync::mpsc::channel().0,
                account_router: Arc::new(Mutex::new(HashMap::new())),
                account_refresh_locks: Arc::new(Mutex::new(HashMap::new())),
                codex_reset_credit_wake: Arc::new(tokio::sync::Notify::new()),
            };

            Self { auth_dir, state }
        }
    }

    impl Drop for TestContext {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.auth_dir);
        }
    }

    #[derive(Clone, Default)]
    struct MockQwenServerState {
        token_requests: Arc<Mutex<Vec<HashMap<String, String>>>>,
    }

    struct MockQwenServer {
        base_url: String,
        token_requests: Arc<Mutex<Vec<HashMap<String, String>>>>,
        handle: JoinHandle<()>,
    }

    impl MockQwenServer {
        async fn spawn() -> Self {
            let state = MockQwenServerState::default();
            let token_requests = state.token_requests.clone();
            let app = Router::new()
                .route("/oauth2/token", post(mock_token_handler))
                .route("/auths/", get(mock_profile_handler))
                .with_state(state);
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let handle = tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });

            Self {
                base_url: format!("http://{}", addr),
                token_requests,
                handle,
            }
        }
    }

    impl Drop for MockQwenServer {
        fn drop(&mut self) {
            self.handle.abort();
        }
    }

    async fn mock_token_handler(
        State(state): State<MockQwenServerState>,
        body: String,
    ) -> Response {
        let form = url::form_urlencoded::parse(body.as_bytes())
            .into_owned()
            .collect::<HashMap<_, _>>();
        state.token_requests.lock().unwrap().push(form.clone());

        match form.get("code").map(String::as_str) {
            Some("good-code") => Json(json!({
                "access_token": "issued-access-token",
                "refresh_token": "issued-refresh-token",
                "token_type": "Bearer",
                "expires_in": 3600,
                "resource_url": "https://resource.qwen.test/v1"
            }))
            .into_response(),
            Some("profile-fail-code") => Json(json!({
                "access_token": "profile-fail-access",
                "refresh_token": "profile-fail-refresh",
                "token_type": "Bearer",
                "expires_in": 3600
            }))
            .into_response(),
            Some("bad-code") => (
                StatusCode::BAD_GATEWAY,
                Html("<html>mock token exchange failure</html>"),
            )
                .into_response(),
            _ => (
                StatusCode::BAD_REQUEST,
                Json(json!({ "message": "unexpected test code" })),
            )
                .into_response(),
        }
    }

    async fn mock_profile_handler(headers: HeaderMap) -> Response {
        let token = headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
            .unwrap_or_default();

        match token {
            "issued-access-token" => Json(json!({
                "id": "qwen-sub-123",
                "email": "qwen@example.com",
                "name": "Qwen Example",
                "role": "user",
                "profile_image_url": "https://example.com/avatar.png",
                "tier": "plus",
                "access_token": "validated-session-token",
                "token_type": "Bearer",
                "expires_at": Utc::now().timestamp() + 3600,
                "permissions": {
                    "chat": {
                        "edit": true
                    }
                }
            }))
            .into_response(),
            "profile-fail-access" => (
                StatusCode::UNAUTHORIZED,
                Json(json!({ "message": "mock profile failure" })),
            )
                .into_response(),
            _ => (
                StatusCode::UNAUTHORIZED,
                Json(json!({ "message": "unknown mock token" })),
            )
                .into_response(),
        }
    }

    async fn start_qwen_login(state: &crate::AppState) -> StartedLogin {
        let (auth_url, state_token, pending_auth) =
            auth::build_auth_url(state.cfg.as_ref()).unwrap();
        {
            let mut pending = state.qwen_oauth_pending.lock().unwrap();
            pending.insert(state_token.clone(), pending_auth);
        }
        let cookie_pair = oauth::build_state_cookie(OAuthProvider::Qwen, &state_token)
            .split(';')
            .next()
            .unwrap()
            .to_string();
        assert_eq!(query_param(&auth_url, "state").unwrap(), state_token);

        StartedLogin {
            state_token,
            cookie_pair,
        }
    }

    fn cookie_headers(cookie_pair: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(header::COOKIE, HeaderValue::from_str(cookie_pair).unwrap());
        headers
    }

    async fn response_text(response: Response) -> String {
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    fn query_param(raw_url: &str, name: &str) -> Option<String> {
        let url = url::Url::parse(raw_url).ok()?;
        url.query_pairs()
            .find_map(|(key, value)| (key == name).then_some(value.into_owned()))
    }

    fn unique_test_dir() -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("io-gateway-qwen-tests-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).unwrap();
        path
    }
}
