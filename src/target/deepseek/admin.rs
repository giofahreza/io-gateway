use axum::{
    body::Bytes,
    extract::State,
    http::{Method, StatusCode},
    response::{Html, IntoResponse},
};
use serde::Deserialize;

#[derive(Deserialize)]
struct LoginStartRequest {
    api_key: String,
    label: Option<String>,
    base_url: Option<String>,
}

pub async fn accounts_json(State(state): State<crate::AppState>) -> impl IntoResponse {
    let usage_by_key = {
        let stats = state.stats.lock().unwrap();
        stats
            .deepseek_accounts
            .iter()
            .map(|usage| (usage.key.clone(), usage.clone()))
            .collect::<std::collections::HashMap<_, _>>()
    };

    let accounts = state
        .deepseek_accounts
        .lock()
        .unwrap()
        .iter()
        .map(|account| {
            let stats_key = crate::deepseek_stats_key(account);
            let usage = usage_by_key.get(&stats_key).cloned().unwrap_or_default();
            let runtime =
                crate::router_account_runtime_json(&state, "deepseek", &stats_key, account.enabled);
            serde_json::json!({
                "account_id": account.account_id,
                "label": account.label,
                "file_name": account.file_name,
                "enabled": account.enabled,
                "runtime": runtime,
                "base_url": account.base_url,
                "requests": usage.requests,
                "errors": usage.errors,
                "last_success_at": usage.last_success_at,
                "last_error_at": usage.last_error_at,
                "last_error_message": usage.last_error_message
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
        Method::GET => Html(helper_html()).into_response(),
        Method::POST => save_account(&state, &body).await,
        _ => (
            StatusCode::METHOD_NOT_ALLOWED,
            "DeepSeek setup only supports GET for instructions or POST for API key submission",
        )
            .into_response(),
    }
}

async fn save_account(state: &crate::AppState, body: &Bytes) -> axum::response::Response {
    let payload: LoginStartRequest = match serde_json::from_slice(body) {
        Ok(payload) => payload,
        Err(_) => {
            return axum::Json(serde_json::json!({
                "ok": false,
                "message": "Submit DeepSeek credentials as JSON: {\"api_key\":\"...\",\"label\":\"optional\",\"base_url\":\"optional\"}"
            }))
            .into_response();
        }
    };

    let api_key = payload.api_key.trim();
    if api_key.is_empty() {
        return axum::Json(serde_json::json!({
            "ok": false,
            "message": "DeepSeek api_key is required"
        }))
        .into_response();
    }

    let base_url = super::api::normalize_base_url(payload.base_url.as_deref());
    if let Err(err) = super::api::validate_api_key(&state.client, api_key, &base_url).await {
        return axum::Json(serde_json::json!({
            "ok": false,
            "message": err
        }))
        .into_response();
    }

    let requested_label = payload
        .label
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| value.to_string());
    let label = requested_label.unwrap_or_else(|| {
        let suffix = api_key
            .chars()
            .rev()
            .take(6)
            .collect::<String>()
            .chars()
            .rev()
            .collect::<String>();
        format!("deepseek-{}", suffix)
    });
    let account_id = label.clone();

    let auth_dir = state
        .cfg
        .auth_dir
        .clone()
        .unwrap_or_else(|| "/root/dev/yow/io-gateway/auths".to_string());
    let file_name = format!("deepseek-{}.json", sanitize_label(&label));
    let path = std::path::Path::new(&auth_dir).join(file_name);
    let now = chrono::Utc::now().to_rfc3339();
    let out = serde_json::json!({
        "type": "deepseek",
        "account_id": account_id,
        "label": label,
        "api_key": api_key,
        "base_url": base_url,
        "validated_at": now
    });

    if let Err(err) = std::fs::create_dir_all(&auth_dir) {
        return axum::Json(serde_json::json!({
            "ok": false,
            "message": format!("failed to create auth dir: {}", err)
        }))
        .into_response();
    }
    if let Err(err) = super::super::atomic_write_json(&path, &out) {
        return axum::Json(serde_json::json!({
            "ok": false,
            "message": format!("failed to write auth file: {}", err)
        }))
        .into_response();
    }

    super::accounts::reload_state(state);
    axum::Json(serde_json::json!({
        "ok": true,
        "message": format!("saved DeepSeek credentials to {}", path.to_string_lossy()),
        "saved_path": path.to_string_lossy()
    }))
    .into_response()
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

fn helper_html() -> String {
    r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>DeepSeek Provider Setup</title>
</head>
<body>
  <h1>DeepSeek Provider Setup</h1>
  <p>Submit a DeepSeek API key to <code>POST /login/deepseek/start</code> as JSON:</p>
  <pre>{"api_key":"YOUR_DEEPSEEK_API_KEY","label":"optional","base_url":"https://api.deepseek.com"}</pre>
  <p>The gateway validates the key against <code>/models</code> before saving it.</p>
</body>
</html>"#
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::to_bytes, http::StatusCode, response::IntoResponse, routing::get, Json, Router,
    };
    use bytes::Bytes;
    use serde_json::{json, Value};
    use std::{
        collections::{HashMap, HashSet},
        path::PathBuf,
        sync::{Arc, Mutex},
    };
    use tokio::task::JoinHandle;

    #[tokio::test]
    async fn deepseek_login_start_returns_helper_html() {
        let ctx = TestContext::new("http://unused-deepseek-base");
        let response = login_start(State(ctx.state.clone()), Method::GET, Bytes::new())
            .await
            .into_response();
        let status = response.status();
        let body = response_text(response).await;

        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("DeepSeek Provider Setup"));
        assert!(body.contains("POST /login/deepseek/start"));
    }

    #[tokio::test]
    async fn deepseek_login_start_saves_validated_api_key() {
        let server = MockDeepSeekServer::spawn().await;
        let ctx = TestContext::new(&server.base_url);
        let body = Bytes::from(
            serde_json::to_vec(&json!({
                "api_key": "sk-test-deepseek",
                "label": "ds-test",
                "base_url": server.base_url
            }))
            .unwrap(),
        );

        let response = login_start(State(ctx.state.clone()), Method::POST, body)
            .await
            .into_response();
        let status = response.status();
        let payload: Value = serde_json::from_str(&response_text(response).await).unwrap();

        assert_eq!(status, StatusCode::OK);
        assert_eq!(payload["ok"], Value::Bool(true));
        assert_eq!(ctx.state.deepseek_accounts.lock().unwrap().len(), 1);

        let auth_path = ctx.auth_dir.join("deepseek-ds-test.json");
        let saved: Value =
            serde_json::from_str(&std::fs::read_to_string(&auth_path).unwrap()).unwrap();
        assert_eq!(saved["type"], Value::String("deepseek".to_string()));
        assert_eq!(saved["account_id"], Value::String("ds-test".to_string()));
        assert_eq!(saved["label"], Value::String("ds-test".to_string()));
        assert_eq!(
            saved["api_key"],
            Value::String("sk-test-deepseek".to_string())
        );
        assert_eq!(saved["base_url"], Value::String(server.base_url.clone()));
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
                oauth: crate::target::oauth::OAuthConfig::default(),
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

            let _ = base_url;

            Self { auth_dir, state }
        }
    }

    impl Drop for TestContext {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.auth_dir);
        }
    }

    struct MockDeepSeekServer {
        base_url: String,
        handle: JoinHandle<()>,
    }

    impl MockDeepSeekServer {
        async fn spawn() -> Self {
            let app = Router::new().route(
                "/models",
                get(|| async {
                    Json(json!({
                        "object": "list",
                        "data": [{
                            "id": "deepseek-v4-pro",
                            "object": "model",
                            "owned_by": "deepseek"
                        }]
                    }))
                }),
            );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let handle = tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });

            Self {
                base_url: format!("http://{}", addr),
                handle,
            }
        }
    }

    impl Drop for MockDeepSeekServer {
        fn drop(&mut self) {
            self.handle.abort();
        }
    }

    async fn response_text(response: axum::response::Response) -> String {
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    fn unique_test_dir() -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "io-gateway-deepseek-tests-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&path).unwrap();
        path
    }
}
