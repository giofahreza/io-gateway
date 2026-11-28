use axum::{
    body::Bytes,
    extract::{Form, State},
    http::{Method, StatusCode},
    response::{Html, IntoResponse},
};
use serde::Deserialize;
use std::{
    collections::HashMap,
    sync::{Mutex, OnceLock},
};

#[derive(Clone)]
struct CopilotQuotaCacheEntry {
    fetched_at: std::time::Instant,
    payload: serde_json::Value,
}

static COPILOT_QUOTA_CACHE: OnceLock<Mutex<HashMap<String, CopilotQuotaCacheEntry>>> =
    OnceLock::new();

fn copilot_quota_cache() -> &'static Mutex<HashMap<String, CopilotQuotaCacheEntry>> {
    COPILOT_QUOTA_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn invalidate_quota_cache(file_name: Option<&str>) {
    let mut cache = copilot_quota_cache().lock().unwrap();
    if let Some(file_name) = file_name {
        cache.remove(file_name);
    } else {
        cache.clear();
    }
}

#[derive(Deserialize)]
struct LoginStartRequest {
    github_token: Option<String>,
    label: Option<String>,
    account_type: Option<String>,
}

#[derive(Deserialize)]
pub struct LoginSubmitForm {
    pub device_code: String,
    pub label: Option<String>,
    pub account_type: Option<String>,
}

pub async fn accounts_json(State(state): State<crate::AppState>) -> impl IntoResponse {
    let usage_by_key = {
        let stats = state.stats.lock().unwrap();
        stats
            .copilot_accounts
            .iter()
            .map(|usage| (usage.key.clone(), usage.clone()))
            .collect::<std::collections::HashMap<_, _>>()
    };

    let accounts = state
        .copilot_accounts
        .lock()
        .unwrap()
        .iter()
        .map(|account| {
            let stats_key = crate::copilot_stats_key(account);
            let usage = usage_by_key.get(&stats_key).cloned().unwrap_or_default();
            let runtime =
                crate::router_account_runtime_json(&state, "copilot", &stats_key, account.enabled);
            serde_json::json!({
                "account_id": account.account_id,
                "label": account.label,
                "login": account.login,
                "file_name": account.file_name,
                "enabled": account.enabled,
                "runtime": runtime,
                "account_type": account.account_type,
                "copilot_expires_at": unix_to_rfc3339(account.copilot_expires_at),
                "models": account.models.iter().filter(|model| super::accounts::is_app_accessible_model(&model.id)).map(|model| serde_json::json!({
                    "model_id": format!("cop:{}", model.id),
                    "display_name": model.name.as_deref().unwrap_or(&model.id),
                    "upstream_model": model.id,
                    "vendor": model.vendor,
                    "preview": model.preview,
                    "model_picker_category": model.model_picker_category,
                    "policy_state": model.policy_state,
                    "billing_tier": super::accounts::model_billing_tier(&model.id, model.model_picker_category.as_deref()),
                    "premium": super::accounts::model_is_premium(&model.id, model.model_picker_category.as_deref()),
                    "utility_model": super::accounts::is_utility_model(&model.id),
                    "app_accessible": super::accounts::is_app_accessible_model(&model.id)
                })).collect::<Vec<_>>(),
                "requests": usage.requests,
                "errors": usage.errors,
                "prompt_total": usage.prompt_total,
                "prompt_error_total": usage.prompt_error_total,
                "input_tokens": usage.input_tokens,
                "output_tokens": usage.output_tokens,
                "total_tokens": usage.total_tokens,
                "cache_tokens": usage.cache_tokens,
                "reasoning_tokens": usage.reasoning_tokens,
                "last_success_at": usage.last_success_at,
                "last_error_at": usage.last_error_at,
                "last_error_message": usage.last_error_message
            })
        })
        .collect::<Vec<_>>();

    axum::Json(serde_json::json!({ "accounts": accounts }))
}

pub async fn quota_accounts(state: &crate::AppState) -> Vec<serde_json::Value> {
    let accounts = state.copilot_accounts.lock().unwrap().clone();
    let now = std::time::Instant::now();
    let mut out = Vec::with_capacity(accounts.len());
    for account in accounts {
        let key = copilot_quota_cache_key(&account);
        if account.enabled {
            let cached = {
                let cache = copilot_quota_cache().lock().unwrap();
                cache.get(&key).cloned()
            };
            if let Some(cached) = cached {
                if crate::quota_cache_entry_is_fresh(now, cached.fetched_at, &cached.payload) {
                    out.push(cached.payload);
                    continue;
                }
            }
        }

        let mut status = Vec::new();
        let mut models = model_entries(&account.models);
        let mut limits = Vec::new();
        let mut raw_usage = serde_json::Value::Null;
        let mut chat_enabled = serde_json::Value::Null;
        let mut copilot_plan = serde_json::Value::Null;
        let mut access_type_sku = serde_json::Value::Null;
        let mut quota_reset_date = serde_json::Value::Null;

        if account.enabled {
            match super::auth::ensure_copilot_token(&state, &account).await {
                Ok(copilot_token) => {
                    match super::api::fetch_models(
                        &state.client,
                        &account.account_type,
                        &copilot_token,
                    )
                    .await
                    {
                        Ok(live_models) if !live_models.is_empty() => {
                            models = model_entries(&live_models);
                        }
                        Ok(_) => {
                            status.push("Copilot model endpoint returned no models".to_string())
                        }
                        Err(err) => status.push(err),
                    }
                    match super::auth::fetch_copilot_user(&state.client, &account.github_token)
                        .await
                    {
                        Ok(usage) => {
                            chat_enabled = usage
                                .get("chat_enabled")
                                .cloned()
                                .unwrap_or(serde_json::Value::Null);
                            copilot_plan = usage
                                .get("copilot_plan")
                                .cloned()
                                .unwrap_or(serde_json::Value::Null);
                            access_type_sku = usage
                                .get("access_type_sku")
                                .cloned()
                                .unwrap_or(serde_json::Value::Null);
                            quota_reset_date = usage
                                .get("quota_reset_date")
                                .cloned()
                                .unwrap_or(serde_json::Value::Null);
                            limits = quota_limits_from_usage(&usage);
                            raw_usage = usage;
                        }
                        Err(err) => status.push(err),
                    }
                }
                Err(err) => status.push(err),
            }
        } else {
            status.push("Account is disabled".to_string());
        }

        let payload = serde_json::json!({
            "label": account.label,
            "login": account.login,
            "file_name": account.file_name,
            "account_type": account.account_type,
            "is_available": account.enabled && status.is_empty(),
            "status_msg": if status.is_empty() {
                "GitHub Copilot quota and models loaded from live account metadata.".to_string()
            } else {
                status.join(" | ")
            },
            "available_models": models,
            "limits": limits,
            "chat_enabled": chat_enabled,
            "copilot_plan": copilot_plan,
            "access_type_sku": access_type_sku,
            "quota_reset_date": quota_reset_date,
            "raw_usage": raw_usage
        });
        if account.enabled {
            copilot_quota_cache().lock().unwrap().insert(
                key,
                CopilotQuotaCacheEntry {
                    fetched_at: std::time::Instant::now(),
                    payload: payload.clone(),
                },
            );
        }
        out.push(payload);
    }
    out
}

fn copilot_quota_cache_key(account: &super::accounts::CopilotAccount) -> String {
    account
        .file_name
        .clone()
        .unwrap_or_else(|| account.label.clone())
}

fn model_entries(models: &[super::accounts::CopilotModelInfo]) -> Vec<serde_json::Value> {
    models
        .iter()
        .filter(|model| super::accounts::is_app_accessible_model(&model.id))
        .map(|model| {
            serde_json::json!({
                "model_id": format!("cop:{}", model.id),
                "display_name": model.name.as_deref().unwrap_or(&model.id),
                "upstream_model": model.id,
                "vendor": model.vendor,
                "preview": model.preview,
                "model_picker_category": model.model_picker_category,
                "policy_state": model.policy_state,
                "billing_tier": super::accounts::model_billing_tier(&model.id, model.model_picker_category.as_deref()),
                "premium": super::accounts::model_is_premium(&model.id, model.model_picker_category.as_deref()),
                "utility_model": super::accounts::is_utility_model(&model.id),
                "app_accessible": super::accounts::is_app_accessible_model(&model.id)
            })
        })
        .collect()
}

fn quota_limits_from_usage(usage: &serde_json::Value) -> Vec<serde_json::Value> {
    let reset_label = usage
        .get("quota_reset_date")
        .and_then(|value| value.as_str())
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_default();
    let Some(snapshots) = usage
        .get("quota_snapshots")
        .and_then(|value| value.as_object())
    else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for key in ["chat", "completions", "premium_interactions"] {
        let Some(snapshot) = snapshots.get(key) else {
            continue;
        };
        out.push(quota_limit_from_snapshot(key, snapshot, &reset_label));
    }
    out
}

fn quota_limit_from_snapshot(
    key: &str,
    snapshot: &serde_json::Value,
    reset_label: &str,
) -> serde_json::Value {
    let label = match key {
        "chat" => "Chat",
        "completions" => "Completions",
        "premium_interactions" => "Premium interactions",
        _ => key,
    };
    let unlimited = snapshot
        .get("unlimited")
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    let remaining = number_field(snapshot, &["remaining", "quota_remaining"]);
    let entitlement = number_field(snapshot, &["entitlement", "overage_entitlement"]);
    let percent_remaining = number_field(snapshot, &["percent_remaining"]);
    let used_percent = if unlimited {
        0.0
    } else {
        percent_remaining
            .map(|value| (100.0 - value).clamp(0.0, 100.0))
            .unwrap_or_else(|| {
                entitlement
                    .filter(|value| *value > 0.0)
                    .map(|limit| {
                        let used = limit - remaining.unwrap_or(limit);
                        ((used.max(0.0) / limit) * 100.0).clamp(0.0, 100.0)
                    })
                    .unwrap_or(0.0)
            })
    };
    let used = entitlement
        .zip(remaining)
        .map(|(limit, rem)| (limit - rem).max(0.0));

    serde_json::json!({
        "label": label,
        "scope": key,
        "used_percent": used_percent,
        "used": used,
        "limit": entitlement,
        "remaining": remaining,
        "used_text": if unlimited { "unlimited".to_string() } else { compact_number(used) },
        "limit_text": if unlimited { "unlimited".to_string() } else { compact_number(entitlement) },
        "remaining_text": if unlimited { "unlimited".to_string() } else { compact_number(remaining) },
        "reset_label": reset_label,
        "unlimited": unlimited
    })
}

fn number_field(value: &serde_json::Value, keys: &[&str]) -> Option<f64> {
    keys.iter()
        .find_map(|key| value.get(*key).and_then(|value| value.as_f64()))
}

fn compact_number(value: Option<f64>) -> String {
    let Some(value) = value else {
        return String::new();
    };
    if (value.fract()).abs() < f64::EPSILON {
        format!("{}", value as i64)
    } else {
        format!("{:.1}", value)
    }
}

fn unix_to_rfc3339(value: Option<i64>) -> Option<String> {
    value.and_then(|timestamp| {
        chrono::DateTime::<chrono::Utc>::from_timestamp(timestamp, 0)
            .map(|datetime| datetime.to_rfc3339())
    })
}

pub async fn login_start(
    State(state): State<crate::AppState>,
    method: Method,
    body: Bytes,
) -> impl IntoResponse {
    match method {
        Method::GET => Html(helper_html()).into_response(),
        Method::POST => start_or_save(&state, &body).await,
        _ => (
            StatusCode::METHOD_NOT_ALLOWED,
            "Copilot setup only supports GET for instructions or POST for JSON",
        )
            .into_response(),
    }
}

pub async fn login_submit(
    State(state): State<crate::AppState>,
    Form(form): Form<LoginSubmitForm>,
) -> impl IntoResponse {
    let device_code = form.device_code.trim();
    if device_code.is_empty() {
        return axum::Json(serde_json::json!({
            "ok": false,
            "message": "device_code is required"
        }))
        .into_response();
    }

    let pending = {
        let pending = state.copilot_oauth_pending.lock().unwrap();
        pending.get(device_code).cloned()
    };
    let Some(pending) = pending else {
        return axum::Json(serde_json::json!({
            "ok": false,
            "message": "unknown or expired device_code; start login again"
        }))
        .into_response();
    };
    if pending.expires_at_unix <= chrono::Utc::now().timestamp() {
        state
            .copilot_oauth_pending
            .lock()
            .unwrap()
            .remove(device_code);
        return axum::Json(serde_json::json!({
            "ok": false,
            "message": "device_code expired; start login again"
        }))
        .into_response();
    }

    let github_token = match super::auth::poll_access_token_once(&state.client, device_code).await {
        Ok(token) => token,
        Err(err) => {
            return axum::Json(serde_json::json!({
                "ok": false,
                "message": err
            }))
            .into_response()
        }
    };
    let account_type = form
        .account_type
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(&pending.account_type)
        .to_string();
    match save_from_github_token(&state, &github_token, &account_type, form.label.as_deref()).await
    {
        Ok(saved_path) => {
            state
                .copilot_oauth_pending
                .lock()
                .unwrap()
                .remove(device_code);
            axum::Json(serde_json::json!({
                "ok": true,
                "message": format!("saved Copilot credentials to {}", saved_path),
                "saved_path": saved_path
            }))
            .into_response()
        }
        Err(err) => axum::Json(serde_json::json!({
            "ok": false,
            "message": err
        }))
        .into_response(),
    }
}

async fn start_or_save(state: &crate::AppState, body: &Bytes) -> axum::response::Response {
    let payload: LoginStartRequest = if body.is_empty() {
        LoginStartRequest {
            github_token: None,
            label: None,
            account_type: None,
        }
    } else {
        match serde_json::from_slice(body) {
            Ok(payload) => payload,
            Err(_) => {
                return axum::Json(serde_json::json!({
                    "ok": false,
                    "message": "Submit JSON: {\"github_token\":\"optional\",\"label\":\"optional\",\"account_type\":\"individual|business|enterprise\"}"
                }))
                .into_response()
            }
        }
    };

    let account_type = super::auth::normalize_account_type(payload.account_type.as_deref());
    if let Some(github_token) = payload
        .github_token
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return match save_from_github_token(
            state,
            github_token,
            &account_type,
            payload.label.as_deref(),
        )
        .await
        {
            Ok(saved_path) => axum::Json(serde_json::json!({
                "ok": true,
                "message": format!("saved Copilot credentials to {}", saved_path),
                "saved_path": saved_path
            }))
            .into_response(),
            Err(err) => axum::Json(serde_json::json!({
                "ok": false,
                "message": err
            }))
            .into_response(),
        };
    }

    let device = match super::auth::start_device_flow(&state.client).await {
        Ok(device) => device,
        Err(err) => {
            return axum::Json(serde_json::json!({
                "ok": false,
                "message": err
            }))
            .into_response()
        }
    };
    let expires_at_unix = chrono::Utc::now()
        .timestamp()
        .saturating_add(device.expires_in);
    let pending = super::auth::PendingDevice {
        device_code: device.device_code.clone(),
        user_code: device.user_code.clone(),
        verification_uri: device.verification_uri.clone(),
        expires_at_unix,
        interval: device.interval,
        account_type,
    };
    state
        .copilot_oauth_pending
        .lock()
        .unwrap()
        .insert(device.device_code.clone(), pending);

    axum::Json(serde_json::json!({
        "ok": true,
        "mode": "device_code",
        "device_code": device.device_code,
        "user_code": device.user_code,
        "verification_uri": device.verification_uri,
        "expires_in": device.expires_in,
        "interval": device.interval,
        "message": format!("Open {} and enter code {}", device.verification_uri, device.user_code)
    }))
    .into_response()
}

async fn save_from_github_token(
    state: &crate::AppState,
    github_token: &str,
    account_type: &str,
    label: Option<&str>,
) -> Result<String, String> {
    let user = super::auth::get_github_user(&state.client, github_token).await?;
    let copilot_token = super::auth::fetch_copilot_token(&state.client, github_token).await?;
    let models = super::api::fetch_models(&state.client, account_type, &copilot_token.token)
        .await
        .unwrap_or_default();
    super::auth::save_auth(
        state,
        github_token,
        account_type,
        label,
        &user,
        &copilot_token,
        &models,
    )
}

fn helper_html() -> String {
    r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>GitHub Copilot Provider Setup</title>
</head>
<body>
  <h1>GitHub Copilot Provider Setup</h1>
  <p>Start device-code login:</p>
  <pre>curl -X POST /login/copilot/start -H 'Content-Type: application/json' -d '{"account_type":"individual"}'</pre>
  <p>Open the returned verification URL, enter the user code, then submit the returned device code:</p>
  <pre>curl -X POST /login/copilot/submit -d 'device_code=DEVICE_CODE'</pre>
  <p>You can also paste a GitHub token directly:</p>
  <pre>{"github_token":"YOUR_GITHUB_TOKEN","label":"optional","account_type":"individual"}</pre>
  <p>Use model ids with the <code>cop:</code> prefix, for example <code>cop:gpt-5.1</code> for Codex CLI or <code>cop:claude-sonnet-4.5</code> for Claude Code.</p>
</body>
</html>"#
        .to_string()
}
