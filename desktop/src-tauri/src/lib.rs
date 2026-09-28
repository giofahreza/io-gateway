use reqwest::{
    header::{COOKIE, SET_COOKIE},
    Client, Method, Url,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{collections::HashMap, path::PathBuf, sync::Arc};
use tauri::State;
use tokio::sync::Mutex;

#[derive(Default)]
struct AppState {
    session: Arc<Mutex<GatewaySession>>,
}

#[derive(Default)]
struct GatewaySession {
    base_url: Option<String>,
    client: Option<Client>,
    cookie: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GatewayRequest {
    base_url: String,
    path: String,
    method: Option<String>,
    body: Option<Value>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LoginRequest {
    base_url: String,
    otp: String,
    api_key: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct GatewayResponse {
    status: u16,
    body: Value,
}

#[tauri::command]
async fn gateway_request(
    request: GatewayRequest,
    state: State<'_, AppState>,
) -> Result<GatewayResponse, String> {
    let (client, cookie) = session_client(&state, &request.base_url).await?;
    let url = gateway_url(&request.base_url, &request.path)?;
    let method = match request
        .method
        .as_deref()
        .unwrap_or("GET")
        .to_ascii_uppercase()
        .as_str()
    {
        "GET" => Method::GET,
        "POST" => Method::POST,
        "DELETE" => Method::DELETE,
        "PUT" => Method::PUT,
        "PATCH" => Method::PATCH,
        method => return Err(format!("unsupported method: {method}")),
    };

    let mut builder = client.request(method, url);
    if let Some(cookie) = cookie {
        builder = builder.header(COOKIE, cookie);
    }
    if let Some(body) = request.body {
        builder = builder.json(&body);
    }

    let response = decode_response(builder.send().await.map_err(|err| err.to_string())?).await?;
    if request.path.trim_start_matches('/') == "admin/session"
        && response
            .body
            .get("enabled")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        && !response
            .body
            .get("authenticated")
            .and_then(Value::as_bool)
            .unwrap_or(false)
    {
        clear_session_cookie(&state, &request.base_url).await?;
    }
    Ok(response)
}

#[tauri::command]
async fn gateway_login(
    request: LoginRequest,
    state: State<'_, AppState>,
) -> Result<GatewayResponse, String> {
    let (client, cookie) = session_client(&state, &request.base_url).await?;
    let url = gateway_url(&request.base_url, "/admin/login")?;
    let mut params = vec![("otp", request.otp)];
    if let Some(api_key) = request.api_key.filter(|value| !value.trim().is_empty()) {
        params.push(("api_key", api_key));
    }
    let mut builder = client.post(url).form(&params);
    if let Some(cookie) = cookie {
        builder = builder.header(COOKIE, cookie);
    }
    let response = builder.send().await.map_err(|err| err.to_string())?;
    if let Some(cookie) = extract_session_cookie(response.headers()) {
        save_session_cookie(&state, &request.base_url, cookie).await?;
    }
    decode_response(response).await
}

async fn session_client(
    state: &State<'_, AppState>,
    base_url: &str,
) -> Result<(Client, Option<String>), String> {
    let normalized = normalize_base_url(base_url)?;
    let mut session = state.session.lock().await;
    if session.base_url.as_deref() != Some(&normalized) || session.client.is_none() {
        session.cookie = load_persisted_cookie(&normalized)?;
        session.base_url = Some(normalized.clone());
        session.client = Some(
            Client::builder()
                .user_agent("IO Gateway Usage Desktop")
                .build()
                .map_err(|err| err.to_string())?,
        );
    }
    let client = session
        .client
        .as_ref()
        .cloned()
        .ok_or_else(|| "gateway client was not initialized".to_string())?;
    Ok((client, session.cookie.clone()))
}

async fn decode_response(response: reqwest::Response) -> Result<GatewayResponse, String> {
    let status = response.status().as_u16();
    let text = response.text().await.map_err(|err| err.to_string())?;
    let body = if text.trim().is_empty() {
        json!(null)
    } else {
        serde_json::from_str(&text).unwrap_or_else(|_| json!({ "message": text }))
    };
    Ok(GatewayResponse { status, body })
}

fn gateway_url(base_url: &str, path: &str) -> Result<Url, String> {
    let normalized = normalize_base_url(base_url)?;
    let base = Url::parse(&normalized).map_err(|err| err.to_string())?;
    base.join(path.trim_start_matches('/'))
        .map_err(|err| err.to_string())
}

fn normalize_base_url(base_url: &str) -> Result<String, String> {
    let trimmed = base_url.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return Err("gateway URL is required".to_string());
    }
    let parsed = Url::parse(trimmed).map_err(|err| format!("invalid gateway URL: {err}"))?;
    match parsed.scheme() {
        "http" | "https" => Ok(trimmed.to_string()),
        scheme => Err(format!("unsupported gateway URL scheme: {scheme}")),
    }
}

async fn save_session_cookie(
    state: &State<'_, AppState>,
    base_url: &str,
    cookie: String,
) -> Result<(), String> {
    let normalized = normalize_base_url(base_url)?;
    {
        let mut session = state.session.lock().await;
        if session.base_url.as_deref() == Some(&normalized) {
            session.cookie = Some(cookie.clone());
        }
    }
    let mut store = load_cookie_store()?;
    store.insert(normalized, cookie);
    save_cookie_store(&store)
}

async fn clear_session_cookie(state: &State<'_, AppState>, base_url: &str) -> Result<(), String> {
    let normalized = normalize_base_url(base_url)?;
    {
        let mut session = state.session.lock().await;
        if session.base_url.as_deref() == Some(&normalized) {
            session.cookie = None;
        }
    }
    let mut store = load_cookie_store()?;
    store.remove(&normalized);
    save_cookie_store(&store)
}

fn extract_session_cookie(headers: &reqwest::header::HeaderMap) -> Option<String> {
    headers.get_all(SET_COOKIE).iter().find_map(|value| {
        let raw = value.to_str().ok()?;
        let cookie = raw.split(';').next()?.trim();
        cookie
            .starts_with("io_gateway_admin_session=")
            .then(|| cookie.to_string())
    })
}

fn load_persisted_cookie(base_url: &str) -> Result<Option<String>, String> {
    Ok(load_cookie_store()?.get(base_url).cloned())
}

fn load_cookie_store() -> Result<HashMap<String, String>, String> {
    let path = cookie_store_path()?;
    if !path.exists() {
        return Ok(HashMap::new());
    }
    let data = std::fs::read_to_string(&path)
        .map_err(|err| format!("failed to read desktop session store: {err}"))?;
    serde_json::from_str(&data)
        .map_err(|err| format!("failed to parse desktop session store: {err}"))
}

fn save_cookie_store(store: &HashMap<String, String>) -> Result<(), String> {
    let path = cookie_store_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|err| format!("failed to create desktop session dir: {err}"))?;
    }
    let data = serde_json::to_string_pretty(store)
        .map_err(|err| format!("failed to encode desktop session store: {err}"))?;
    std::fs::write(&path, data)
        .map_err(|err| format!("failed to write desktop session store: {err}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .map_err(|err| format!("failed to protect desktop session store: {err}"))?;
    }
    Ok(())
}

fn cookie_store_path() -> Result<PathBuf, String> {
    let home = std::env::var_os("HOME").ok_or_else(|| "HOME is not set".to_string())?;
    Ok(PathBuf::from(home).join(".local/share/io-gateway-usage/sessions.json"))
}

pub fn run() {
    tauri::Builder::default()
        .manage(AppState::default())
        .invoke_handler(tauri::generate_handler![gateway_request, gateway_login])
        .run(tauri::generate_context!())
        .expect("error while running IO Gateway Usage desktop app");
}
