//! Qwen Cloud management login, separate from the inference API key.
//! Follows QwenCloud CLI's PKCE device flow. Provider credentials stay on the
//! gateway; browsers and the TUI receive only a verification URL and local ID.
use super::accounts::{CompatibleAccount, Service};
use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};

#[derive(Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct Credentials {
    pub access_token: String,
    pub expires_at: String,
    pub email: String,
    pub user_id: String,
}

impl Credentials {
    pub fn expired(&self) -> bool {
        chrono::DateTime::parse_from_rfc3339(&self.expires_at)
            .map(|at| at.timestamp() <= chrono::Utc::now().timestamp())
            .unwrap_or(true)
    }
}

pub fn connection(account: &CompatibleAccount) -> Value {
    match &account.cloud_auth {
        Some(auth) => json!({"status":if auth.expired(){"expired"}else{"connected"},
            "email":auth.email,"expires_at":auth.expires_at}),
        None => json!({"status":if account.console_cookie.is_some(){"cookie"}else{"disconnected"}}),
    }
}

#[derive(Clone)]
pub struct PendingLogin {
    pub expires_at: Instant,
    file_name: String,
    device: Arc<tokio::sync::Mutex<DeviceLogin>>,
}

struct DeviceLogin {
    client_id: String,
    token: String,
    verifier: String,
    account_id: String,
    api_key: String,
    service: Service,
    interval: u64,
    next_poll: Instant,
}

pub(super) struct Endpoints<'a> {
    pub auth: &'a str,
    pub api: &'a str,
    pub quota: &'a str,
}

pub(super) const ENDPOINTS: Endpoints<'static> = Endpoints {
    auth: "https://t.qwencloud.com",
    api: "https://cli.qwencloud.com",
    quota: "https://ap-southeast-1-cs-data.qwencloud.com",
};

pub(super) fn client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(15))
        .connect_timeout(Duration::from_secs(5))
        .user_agent("io-gateway/0.1.0")
        .build()
        .map_err(|_| "Could not initialize the Qwen Cloud connection.".into())
}

fn failure(message: impl Into<String>) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({"ok":false,"message":message.into()})),
    )
        .into_response()
}

fn account(state: &crate::AppState, file: &str) -> Option<CompatibleAccount> {
    state
        .compatible_accounts
        .lock()
        .unwrap()
        .iter()
        .find(|a| a.file_name.as_deref() == Some(file) && a.service != Service::Custom)
        .cloned()
}

#[derive(Deserialize)]
pub struct AccountRequest {
    pub file_name: String,
}
#[derive(Deserialize)]
pub struct LoginRequest {
    pub login_id: String,
}

fn pkce() -> (String, String) {
    let bytes: [u8; 32] = rand::random();
    let verifier = URL_SAFE_NO_PAD.encode(bytes);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    (verifier, challenge)
}

fn verification_url(value: &str) -> bool {
    url::Url::parse(value).is_ok_and(|url| {
        url.scheme() == "https"
            && url.username().is_empty()
            && url.password().is_none()
            && url.port().is_none_or(|port| port == 443)
            && url
                .host_str()
                .is_some_and(|host| host == "qwencloud.com" || host.ends_with(".qwencloud.com"))
    })
}

async fn begin(
    client: &reqwest::Client,
    endpoints: &Endpoints<'_>,
    account: &CompatibleAccount,
) -> Result<(PendingLogin, String, u64), String> {
    let client_id = uuid::Uuid::new_v4().to_string();
    let (verifier, challenge) = pkce();
    let response = client
        .post(format!("{}/cli/device/code", endpoints.auth))
        .query(&[
            ("client_id", client_id.as_str()),
            ("code_challenge", challenge.as_str()),
            ("code_challenge_method", "S256"),
        ])
        .send()
        .await
        .map_err(|_| "Could not start Qwen Cloud sign-in. Try again.")?;
    let raw = super::quota::body(response).await?;
    let data = &raw["Data"];
    let token = data["Token"]
        .as_str()
        .filter(|s| !s.is_empty() && s.len() <= 16384);
    let url = data["VerificationUrl"]
        .as_str()
        .filter(|s| s.len() <= 16384 && verification_url(s));
    let expires = data["ExpiresIn"]
        .as_u64()
        .filter(|s| *s > 0)
        .map(|s| s.min(900));
    let (Some(token), Some(url), Some(expires)) = (token, url, expires) else {
        return Err("Qwen Cloud did not provide a valid sign-in link. Try again.".into());
    };
    if raw["Success"] != true {
        return Err("Qwen Cloud could not start sign-in.".into());
    }
    let interval = data["Interval"].as_u64().unwrap_or(5).clamp(5, 60);
    Ok((
        PendingLogin {
            expires_at: Instant::now() + Duration::from_secs(expires),
            file_name: account
                .file_name
                .clone()
                .ok_or("Save the API account before connecting quota.")?,
            device: Arc::new(tokio::sync::Mutex::new(DeviceLogin {
                client_id,
                token: token.into(),
                verifier,
                account_id: account.account_id.clone(),
                api_key: account.api_key.clone(),
                service: account.service,
                interval,
                next_poll: Instant::now() + Duration::from_secs(interval),
            })),
        },
        url.into(),
        interval,
    ))
}

pub async fn start(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    Json(req): Json<AccountRequest>,
) -> Response {
    if let Some(response) = crate::require_admin_session_json(&state, &headers) {
        return response;
    }
    let Some(account) = account(&state, &req.file_name) else {
        return failure("Save a Qwen Cloud API account before connecting quota.");
    };
    {
        let mut pending = state.qwen_cloud_pending.lock().unwrap();
        pending.retain(|_, login| login.expires_at > Instant::now());
        if pending.len() >= 32 {
            return failure(
                "Too many pending sign-ins. Cancel an existing sign-in or wait for it to expire.",
            );
        }
    }
    let result = match client() {
        Ok(client) => begin(&client, &ENDPOINTS, &account).await,
        Err(err) => Err(err),
    };
    match result {
        Ok((pending, url, interval)) => {
            let expires = pending
                .expires_at
                .saturating_duration_since(Instant::now())
                .as_secs();
            let login_id = uuid::Uuid::new_v4().to_string();
            let mut logins = state.qwen_cloud_pending.lock().unwrap();
            logins.retain(|_, login| {
                login.file_name != req.file_name && login.expires_at > Instant::now()
            });
            if logins.len() >= 32 {
                return failure("Too many pending sign-ins. Cancel an existing sign-in or wait for it to expire.");
            }
            logins.insert(login_id.clone(), pending);
            Json(json!({"ok":true,"login_id":login_id,"verification_url":url,"expires_in":expires,"interval":interval})).into_response()
        }
        Err(err) => failure(err),
    }
}

enum Poll {
    Pending,
    SlowDown,
    Complete(Credentials),
    Denied,
    Expired,
}

async fn poll(
    client: &reqwest::Client,
    endpoints: &Endpoints<'_>,
    device: &DeviceLogin,
) -> Result<Poll, String> {
    let response = client
        .post(format!("{}/cli/device/token", endpoints.auth))
        .query(&[
            ("client_id", device.client_id.as_str()),
            ("token", device.token.as_str()),
            ("code_verifier", device.verifier.as_str()),
        ])
        .send()
        .await
        .map_err(|_| "Could not check Qwen Cloud sign-in. Try again.")?;
    // Some device-flow signals are sent as non-2xx JSON responses.
    let http_status = response.status();
    let raw = super::quota::json_body(response).await?;
    let data = raw.get("Data").unwrap_or(&raw);
    let status = data["Status"]
        .as_str()
        .or_else(|| data["status"].as_str())
        .or_else(|| raw["error"].as_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match status.as_str() {
        "authorization_pending" => Ok(Poll::Pending),
        "slow_down" => Ok(Poll::SlowDown),
        "access_denied" => Ok(Poll::Denied),
        "expired_token" => Ok(Poll::Expired),
        "complete" if http_status.is_success() => {
            if raw["Success"] == false {
                return Err("Qwen Cloud rejected sign-in.".into());
            }
            let creds = data
                .get("Credentials")
                .or_else(|| data.get("credentials"))
                .ok_or("Qwen Cloud returned incomplete sign-in credentials.")?;
            let text = |a: &str, b: &str| {
                creds[a]
                    .as_str()
                    .or_else(|| creds[b].as_str())
                    .unwrap_or("")
                    .to_string()
            };
            let user = creds
                .get("User")
                .or_else(|| creds.get("user"))
                .unwrap_or(&Value::Null);
            let auth = Credentials {
                access_token: text("AccessToken", "access_token"),
                expires_at: text("ExpireTime", "expire_time"),
                email: user["Email"]
                    .as_str()
                    .or_else(|| user["email"].as_str())
                    .unwrap_or("")
                    .to_string(),
                user_id: user["AliyunId"]
                    .as_str()
                    .or_else(|| user["aliyunId"].as_str())
                    .or_else(|| user["Organization"].as_str())
                    .unwrap_or("")
                    .to_string(),
            };
            if auth.access_token.is_empty()
                || auth.access_token.len() > 32768
                || auth.access_token.chars().any(char::is_control)
                || auth.expired()
            {
                return Err(
                    "Qwen Cloud returned incomplete or expired credentials. Start sign-in again."
                        .into(),
                );
            }
            Ok(Poll::Complete(auth))
        }
        _ => Err("Qwen Cloud could not confirm sign-in. Try again or start a new sign-in.".into()),
    }
}

pub async fn status(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    Json(req): Json<LoginRequest>,
) -> Response {
    if let Some(response) = crate::require_admin_session_json(&state, &headers) {
        return response;
    }
    let pending = state
        .qwen_cloud_pending
        .lock()
        .unwrap()
        .get(&req.login_id)
        .cloned();
    let Some(pending) = pending else {
        return failure("Sign-in expired or was cancelled. Connect Qwen Cloud account again.");
    };
    let mut device = pending.device.lock().await;
    if pending.expires_at <= Instant::now() {
        state
            .qwen_cloud_pending
            .lock()
            .unwrap()
            .remove(&req.login_id);
        return failure("Qwen Cloud sign-in expired. Connect again.");
    }
    if device.next_poll > Instant::now() {
        return Json(
            json!({"ok":true,"status":"authorization_pending","interval":device.interval}),
        )
        .into_response();
    }
    device.next_poll = Instant::now() + Duration::from_secs(device.interval);
    let result = match client() {
        Ok(client) => poll(&client, &ENDPOINTS, &device).await,
        Err(err) => Err(err),
    };
    match result {
        Ok(Poll::Complete(auth)) => {
            let result = persist_connection(&state, &req.login_id, &pending, &device, &auth);
            if let Err(err) = result {
                return failure(err);
            }
            super::accounts::reload_state(&state);
            Json(json!({"ok":true,"status":"connected","email":auth.email,"expires_at":auth.expires_at,
                "message":"Qwen Cloud account connected for quota."})).into_response()
        }
        Ok(Poll::Denied | Poll::Expired) => {
            state
                .qwen_cloud_pending
                .lock()
                .unwrap()
                .remove(&req.login_id);
            failure("Qwen Cloud sign-in was denied or expired. Connect again when ready.")
        }
        Ok(Poll::Pending | Poll::SlowDown) => {
            if matches!(result, Ok(Poll::SlowDown)) {
                device.interval = (device.interval + 5).min(60);
            }
            device.next_poll = Instant::now() + Duration::from_secs(device.interval);
            Json(json!({"ok":true,"status":"authorization_pending","interval":device.interval}))
                .into_response()
        }
        Err(err) => failure(err),
    }
}

fn persist_connection(
    state: &crate::AppState,
    login_id: &str,
    pending: &PendingLogin,
    device: &DeviceLogin,
    auth: &Credentials,
) -> Result<(), String> {
    // Cancellation/disconnection wins over a response already in flight.
    let mut logins = state.qwen_cloud_pending.lock().unwrap();
    if !logins.contains_key(login_id) || pending.expires_at <= Instant::now() {
        return Err("Sign-in was cancelled or expired. Connect again when ready.".into());
    }
    let account = account(state, &pending.file_name)
        .filter(|a| {
            a.account_id == device.account_id
                && a.api_key == device.api_key
                && a.service == device.service
        })
        .ok_or("API account changed or was removed. Start sign-in again.")?;
    let dir = state
        .cfg
        .auth_dir
        .as_ref()
        .ok_or("An auth directory must be configured.")?;
    // The filename comes from the loaded account list, never from a request path.
    let path = std::path::Path::new(dir).join(account.file_name.as_ref().unwrap());
    write_connection(&path, &device.account_id, &device.api_key, auth)?;
    logins.remove(login_id);
    Ok(())
}

fn write_connection(
    path: &std::path::Path,
    account_id: &str,
    api_key: &str,
    auth: &Credentials,
) -> Result<(), String> {
    let mut value: Value = serde_json::from_slice(
        &std::fs::read(path).map_err(|_| "API account was removed. Start sign-in again.")?,
    )
    .map_err(|_| "Could not read the saved API account.")?;
    if value["account_id"] != account_id
        || value["api_key"] != api_key
        || value["type"] != "compatible"
    {
        return Err("API account changed. Start sign-in again.".into());
    }
    value["cloud_auth"] =
        serde_json::to_value(auth).map_err(|_| "Could not save the Qwen Cloud connection.")?;
    crate::target::atomic_write_json(path, &value)
        .map_err(|_| "Could not save the Qwen Cloud connection.")?;
    Ok(())
}

pub async fn cancel(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    Json(req): Json<LoginRequest>,
) -> Response {
    if let Some(response) = crate::require_admin_session_json(&state, &headers) {
        return response;
    }
    state
        .qwen_cloud_pending
        .lock()
        .unwrap()
        .remove(&req.login_id);
    Json(json!({"ok":true,"message":"Qwen Cloud sign-in cancelled."})).into_response()
}

pub async fn disconnect(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    Json(req): Json<AccountRequest>,
) -> Response {
    if let Some(response) = crate::require_admin_session_json(&state, &headers) {
        return response;
    }
    let Some(account) = account(&state, &req.file_name) else {
        return failure("Qwen Cloud API account not found.");
    };
    let mut pending = state.qwen_cloud_pending.lock().unwrap();
    pending.retain(|_, login| login.file_name != req.file_name);
    let Some(dir) = state.cfg.auth_dir.as_ref() else {
        return failure("An auth directory must be configured.");
    };
    let path = std::path::Path::new(dir).join(account.file_name.as_ref().unwrap());
    let result = (|| -> Result<(), String> {
        let mut value: Value = serde_json::from_slice(
            &std::fs::read(&path).map_err(|_| "Could not read the API account.")?,
        )
        .map_err(|_| "Could not read the API account.")?;
        value
            .as_object_mut()
            .ok_or("Could not read the API account.")?
            .remove("cloud_auth");
        value.as_object_mut().unwrap().remove("console_cookie");
        crate::target::atomic_write_json(&path, &value)
            .map_err(|_| "Could not disconnect the Qwen Cloud account.".to_string())
    })();
    drop(pending);
    if let Err(err) = result {
        return failure(err);
    }
    super::accounts::reload_state(&state);
    Json(json!({"ok":true,"message":"Qwen Cloud disconnected. API access is still available."}))
        .into_response()
}

pub(super) async fn management_query(
    client: &reqwest::Client,
    endpoints: &Endpoints<'_>,
    access_token: &str,
    product: &str,
    action: &str,
    params: Value,
) -> Result<Value, String> {
    let mut request =
        json!({"product":product,"action":action,"region":"ap-southeast-1","params":params});
    if action == "GenerateCLIAccessToken" {
        request["version"] = json!("2026-02-10");
    }
    let response = client
        .post(format!("{}/data/v2/api.json", endpoints.api))
        .bearer_auth(access_token)
        .json(&request)
        .send()
        .await
        .map_err(|_| "Could not query Qwen Cloud. Try refreshing quota.")?;
    let raw = super::quota::body(response).await?;
    if raw["code"] != "200" || raw["successResponse"] == false {
        return Err("Qwen Cloud login required. Reconnect your account to show quota.".into());
    }
    Ok(raw["data"].clone())
}

pub(super) async fn fetch_quota(
    client: &reqwest::Client,
    endpoints: &Endpoints<'_>,
    auth: &Credentials,
    service: Service,
) -> Result<Value, String> {
    if auth.expired() {
        return Err("Qwen Cloud login expired. Reconnect your account to show quota.".into());
    }
    if service == Service::QwenPayg {
        let value = management_query(
            client,
            endpoints,
            &auth.access_token,
            "BssOpenAPI-V3",
            "GetBillingAccountAvailableAmount",
            json!({}),
        )
        .await?;
        return super::quota::payg(&value);
    }
    let derived = management_query(
        client,
        endpoints,
        &auth.access_token,
        "ModelStudioRegional",
        "GenerateCLIAccessToken",
        json!({}),
    )
    .await?;
    let token = derived["cliAccessToken"]
        .as_str()
        .filter(|s| !s.is_empty() && !s.chars().any(char::is_control))
        .ok_or("Qwen Cloud could not authorize quota access. Reconnect your account.")?;
    let query = |suffix: &'static str| async move {
        let api = format!("zeldaHttp.apikeyMgr./tokenplan/personal/api/v2/{suffix}");
        let data = if suffix == "subscription" {
            json!({"commodityCode":"sfm_tokenplansolo_public_intl"})
        } else {
            json!({})
        };
        let params = json!({"Api":api,"V":"1.0","Data":{
            "cornerstoneParam":{"protocol":"V2","console":"ONE_CONSOLE","xsp_lang":"en-US","productCode":"p_efm","switchUserType":3,"consoleSite":"QWENCLOUD"}
        }});
        let mut params = params;
        params["Data"]
            .as_object_mut()
            .unwrap()
            .extend(data.as_object().unwrap().clone());
        let response = client
            .post(format!("{}/cli/api.json", endpoints.quota))
            .query(&[
                ("action", "IntlBroadScopeAspnGateway"),
                ("product", "sfm_bailian"),
                ("api", api.as_str()),
            ])
            .bearer_auth(token)
            .form(&[
                ("region", "ap-southeast-1"),
                ("params", params.to_string().as_str()),
            ])
            .send()
            .await
            .map_err(|_| "Could not read Qwen Cloud quota.")?;
        let raw = super::quota::body(response).await?;
        if raw["code"] != "200" || raw["successResponse"] != true || raw["data"]["success"] != true
        {
            return Err(
                "Qwen Cloud quota access was rejected. Reconnect your account.".to_string(),
            );
        }
        let envelope = &raw["data"]["DataV2"];
        if !envelope["ret"][0]
            .as_str()
            .is_some_and(|s| s.starts_with("SUCCESS::"))
        {
            return Err("Qwen Cloud did not confirm the quota query.".to_string());
        }
        super::quota::payload(&envelope["data"])
    };
    let (usage, subscription, config) =
        tokio::join!(query("usage"), query("subscription"), query("quota-config"));
    Ok(super::quota::token_plan(
        &usage?,
        &subscription.unwrap_or(Value::Null),
        &config.unwrap_or(Value::Null),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        extract::{Form, Query},
        routing::post,
    };
    use std::{
        collections::HashMap,
        sync::{
            atomic::{AtomicUsize, Ordering},
            Mutex,
        },
    };

    fn credentials() -> Credentials {
        Credentials {
            access_token: "private-management-token".into(),
            expires_at: "2099-01-01T00:00:00Z".into(),
            email: "owner@example.test".into(),
            user_id: "owner-1".into(),
        }
    }

    #[tokio::test]
    async fn device_login_keeps_pkce_and_tokens_on_the_server_and_handles_pending_signals() {
        let challenge = Arc::new(Mutex::new(String::new()));
        let captured = challenge.clone();
        let polls = Arc::new(AtomicUsize::new(0));
        let recorded = polls.clone();
        let app = axum::Router::new()
            .route("/cli/device/code", post(move |Query(query): Query<HashMap<String,String>>, headers: HeaderMap| {
                let captured = captured.clone();
                async move {
                    assert!(!headers.contains_key("authorization"));
                    assert_eq!(query["code_challenge_method"], "S256");
                    assert!(!query.contains_key("code_verifier"));
                    *captured.lock().unwrap() = query["code_challenge"].clone();
                    Json(json!({"Success":true,"Data":{"Token":"private-device-token","VerificationUrl":"https://account.qwencloud.com/device?code=verify","ExpiresIn":300,"Interval":2}}))
                }
            }))
            .route("/cli/device/token", post(move |Query(query): Query<HashMap<String,String>>| {
                let captured = challenge.clone();
                let recorded = recorded.clone();
                async move {
                    assert_eq!(query["token"], "private-device-token");
                    assert_eq!(URL_SAFE_NO_PAD.encode(Sha256::digest(query["code_verifier"].as_bytes())), *captured.lock().unwrap());
                    let index = recorded.fetch_add(1, Ordering::SeqCst);
                    let (status, raw) = match index {
                        0 => (StatusCode::OK, json!({"Success":true,"Data":{"Status":"authorization_pending"}})),
                        1 => (StatusCode::BAD_REQUEST, json!({"error":"slow_down"})),
                        _ => (StatusCode::OK,json!({"Success":true,"Data":{"Status":"complete","Credentials":{
                            "AccessToken":"private-management-token","ExpireTime":"2099-01-01T00:00:00Z",
                            "User":{"Email":"owner@example.test","AliyunId":"owner-1"}}}})),
                    };
                    (status, Json(raw))
                }
            }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let endpoints = Endpoints {
            auth: &base,
            api: &base,
            quota: &base,
        };
        let client = client().unwrap();
        let account = CompatibleAccount {
            service: Service::QwenTokenPlan,
            account_id: "account-1".into(),
            api_key: "private-inference-key".into(),
            file_name: Some("plan.json".into()),
            ..Default::default()
        };
        let (pending, url, interval) = begin(&client, &endpoints, &account).await.unwrap();
        assert!(verification_url(&url));
        assert_eq!(interval, 5);
        assert!(!url.contains("private-device-token"));
        let device = pending.device.lock().await;
        assert!(matches!(
            poll(&client, &endpoints, &device).await.unwrap(),
            Poll::Pending
        ));
        assert!(matches!(
            poll(&client, &endpoints, &device).await.unwrap(),
            Poll::SlowDown
        ));
        let Poll::Complete(auth) = poll(&client, &endpoints, &device).await.unwrap() else {
            panic!("login did not complete")
        };
        assert_eq!(auth.email, "owner@example.test");
        assert_eq!(auth.access_token, "private-management-token");
        assert!(!auth.expired());
        server.abort();
    }

    #[tokio::test]
    async fn quota_transport_uses_derived_credentials_and_only_reads_plan_information() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let recorded = calls.clone();
        let app = axum::Router::new()
            .route("/data/v2/api.json", post(|headers: HeaderMap, Json(body): Json<Value>| async move {
                assert_eq!(headers["authorization"],"Bearer private-management-token");
                assert!(!headers.contains_key("cookie"));
                assert_eq!(body["product"],"ModelStudioRegional");
                assert_eq!(body["action"],"GenerateCLIAccessToken");
                assert_eq!(body["params"],json!({}));
                Json(json!({"code":"200","successResponse":true,"data":{"cliAccessToken":"private-derived-token"}}))
            }))
            .route("/cli/api.json", post(move |headers: HeaderMap, Query(query): Query<HashMap<String,String>>, Form(form): Form<HashMap<String,String>>| {
                let recorded = recorded.clone();
                async move {
                    assert_eq!(headers["authorization"],"Bearer private-derived-token");
                    assert!(!headers.contains_key("cookie"));
                    assert_eq!(query["action"],"IntlBroadScopeAspnGateway");
                    let params:Value = serde_json::from_str(&form["params"]).unwrap();
                    assert_eq!(params["Data"]["cornerstoneParam"]["consoleSite"],"QWENCLOUD");
                    let api = params["Api"].as_str().unwrap();
                    assert_eq!(query["api"],api);
                    recorded.lock().unwrap().push(api.to_string());
                    let data = if api.ends_with("/usage") {json!({"per1MonthPercentage":0.2,"per1MonthResetTime":1797091200000i64})}
                        else if api.ends_with("/subscription") {assert_eq!(params["Data"]["commodityCode"],"sfm_tokenplansolo_public_intl");json!({"specCode":"essential"})}
                        else if api.ends_with("/quota-config") {json!({"essential":{"monthly":45000}})}
                        else {panic!("unexpected provider operation")};
                    Json(json!({"code":"200","successResponse":true,"data":{"success":true,"DataV2":{"ret":["SUCCESS::success"],"data":{"success":true,"data":data}}}}))
                }
            }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let result = fetch_quota(
            &client().unwrap(),
            &Endpoints {
                auth: &base,
                api: &base,
                quota: &base,
            },
            &credentials(),
            Service::QwenTokenPlan,
        )
        .await
        .unwrap();
        assert_eq!(result["credits"]["balance"], 36000.0);
        assert_eq!(result["limits"].as_array().unwrap().len(), 1);
        assert_eq!(result["limits"][0]["label"], "Monthly");
        assert_eq!(result["limits"][0]["used_percent"], 20.0);
        assert!(result["credits"]["estimated"].as_bool().unwrap());
        assert!(!result.to_string().contains("private-"));
        assert_eq!(calls.lock().unwrap().len(), 3);
        let mut expired = credentials();
        expired.expires_at = "2000-01-01T00:00:00Z".into();
        assert!(fetch_quota(
            &client().unwrap(),
            &Endpoints {
                auth: &base,
                api: &base,
                quota: &base
            },
            &expired,
            Service::QwenTokenPlan
        )
        .await
        .is_err());
        assert_eq!(calls.lock().unwrap().len(), 3);
        server.abort();
    }

    #[test]
    fn saving_connection_preserves_inference_settings_and_refuses_deleted_or_replaced_accounts() {
        let dir = std::env::temp_dir().join(format!("iogw-qwen-auth-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("plan.json");
        let original = json!({"type":"compatible","account_id":"account-1","api_key":"private-inference-key","label":"My plan","models":["auto"],"service":"qwen_token_plan","custom_metadata":{"preserve":true}});
        crate::target::atomic_write_json(&path, &original).unwrap();
        write_connection(&path, "account-1", "private-inference-key", &credentials()).unwrap();
        let saved: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(
            saved["cloud_auth"]["access_token"],
            "private-management-token"
        );
        for key in [
            "api_key",
            "account_id",
            "models",
            "label",
            "custom_metadata",
        ] {
            assert_eq!(saved[key], original[key]);
        }
        let account: CompatibleAccount = serde_json::from_value(saved.clone()).unwrap();
        let public = serde_json::to_string(&account).unwrap();
        assert!(!public.contains("private-") && !public.contains("cloud_auth"));
        assert_eq!(connection(&account)["email"], "owner@example.test");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        assert!(write_connection(
            &path,
            "another-account",
            "private-inference-key",
            &credentials()
        )
        .is_err());
        assert!(write_connection(&path, "account-1", "a-replaced-key", &credentials()).is_err());
        assert_eq!(
            serde_json::from_slice::<Value>(&std::fs::read(&path).unwrap()).unwrap(),
            saved
        );
        std::fs::remove_file(&path).unwrap();
        assert!(
            write_connection(&path, "account-1", "private-inference-key", &credentials()).is_err()
        );
        assert!(!path.exists());
        std::fs::remove_dir(dir).unwrap();
    }

    #[test]
    fn login_links_and_expiry_are_validated_without_exposing_the_token() {
        for bad in [
            "http://account.qwencloud.com/login",
            "https://qwencloud.com.evil.test/login",
            "https://evil.test/qwencloud.com",
            "https://user:secret@account.qwencloud.com/login",
        ] {
            assert!(!verification_url(bad));
        }
        let mut auth = credentials();
        auth.expires_at = "not a date".into();
        assert!(auth.expired());
        let account = CompatibleAccount {
            cloud_auth: Some(auth),
            ..Default::default()
        };
        assert_eq!(connection(&account)["status"], "expired");
        assert!(!connection(&account)
            .to_string()
            .contains("private-management-token"));
    }
}
