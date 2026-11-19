use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};

#[derive(Deserialize)]
pub struct SaveRequest {
    #[serde(default)]
    pub file_name: Option<String>,
    pub label: String,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default, alias = "openai_base_url")]
    pub base_url: Option<String>,
    #[serde(default)]
    pub anthropic_base_url: Option<String>,
    #[serde(default)]
    pub models: Vec<String>,
}

pub fn normalize_url(value: Option<&str>) -> Result<Option<String>, String> {
    let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    let url = url::Url::parse(value).map_err(|_| "Enter an absolute HTTP or HTTPS base URL")?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(
            "Base URLs must use HTTP or HTTPS, without credentials, query, or fragment".into(),
        );
    }
    Ok(Some(value.trim_end_matches('/').to_string()))
}

pub fn snapshot(state: &crate::AppState) -> Value {
    let accounts = state.compatible_accounts.lock().unwrap().clone();
    let stats = state.stats.lock().unwrap();
    json!({"accounts":accounts.iter().map(|account| {
        let key = crate::compatible_stats_key(account);
        let usage = stats.compatible_accounts.iter().find(|usage| usage.key == key).cloned().unwrap_or_default();
        json!({"account_id":account.account_id,"label":account.label,"file_name":account.file_name,
            "enabled":account.enabled,"base_url":account.base_url,"anthropic_base_url":account.anthropic_base_url,
            "models":account.models,"has_api_key":!account.api_key.is_empty(),"key":key,
            "runtime":crate::router_account_runtime_json(state,"compatible",&key,account.enabled),
            "requests":usage.requests,"errors":usage.errors,"input_tokens":usage.input_tokens,
            "output_tokens":usage.output_tokens,"total_tokens":usage.total_tokens,
            "last_success_at":usage.last_success_at,"last_error_message":usage.last_error_message})
    }).collect::<Vec<_>>()})
}

pub async fn accounts(State(state): State<crate::AppState>, headers: HeaderMap) -> Response {
    if let Some(response) = crate::require_admin_session_json(&state, &headers) {
        return response;
    }
    Json(snapshot(&state)).into_response()
}

fn failure(message: impl Into<String>) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({"ok":false,"message":message.into()})),
    )
        .into_response()
}

pub async fn save(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    Json(payload): Json<SaveRequest>,
) -> Response {
    if let Some(response) = crate::require_admin_session_json(&state, &headers) {
        return response;
    }
    let label = payload.label.trim();
    if label.is_empty() {
        return failure("Name is required");
    }
    let base_url = match normalize_url(payload.base_url.as_deref()) {
        Ok(url) => url,
        Err(err) => return failure(err),
    };
    let anthropic_base_url = match normalize_url(payload.anthropic_base_url.as_deref()) {
        Ok(url) => url,
        Err(err) => return failure(err),
    };
    if base_url.is_none() && anthropic_base_url.is_none() {
        return failure("Enter an OpenAI or Anthropic base URL");
    }
    let mut models: Vec<_> = payload
        .models
        .iter()
        .map(|m| m.trim().to_string())
        .filter(|m| !m.is_empty())
        .collect();
    models.sort();
    models.dedup();
    if models.is_empty()
        || models
            .iter()
            .any(|m| m.starts_with("cmp:") || m.chars().any(char::is_whitespace))
    {
        return failure(
            "Enter at least one upstream model ID, without the gateway cmp: prefix or whitespace",
        );
    }
    let existing = if let Some(file) = payload.file_name.as_deref() {
        let account = state
            .compatible_accounts
            .lock()
            .unwrap()
            .iter()
            .find(|a| a.file_name.as_deref() == Some(file))
            .cloned();
        match account {
            Some(a) => Some(a),
            None => return failure("Compatible account not found"),
        }
    } else {
        None
    };
    let api_key = payload
        .api_key
        .as_deref()
        .map(str::trim)
        .filter(|k| !k.is_empty())
        .map(str::to_string)
        .or_else(|| existing.as_ref().map(|a| a.api_key.clone()))
        .unwrap_or_default();
    if api_key.is_empty() {
        return failure("API key is required");
    }
    let id = existing
        .as_ref()
        .map(|a| a.account_id.clone())
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let file = existing
        .as_ref()
        .and_then(|a| a.file_name.clone())
        .unwrap_or_else(|| format!("compatible-{id}.json"));
    let Some(dir) = state.cfg.auth_dir.as_deref() else {
        return failure("An auth directory must be configured");
    };
    let value = json!({"type":"compatible","account_id":id,"label":label,"api_key":api_key,
        "base_url":base_url,"anthropic_base_url":anthropic_base_url,"models":models});
    if let Err(_) = crate::target::atomic_write_json(&std::path::Path::new(dir).join(&file), &value)
    {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"ok":false,"message":"Could not save compatible account"})),
        )
            .into_response();
    }
    super::accounts::reload_state(&state);
    crate::invalidate_model_caches().await;
    Json(json!({"ok":true,"file_name":file,"message":"Compatible account saved"})).into_response()
}

pub async fn discover(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    Json(payload): Json<SaveRequest>,
) -> Response {
    if let Some(response) = crate::require_admin_session_json(&state, &headers) {
        return response;
    }
    let existing = payload.file_name.as_deref().and_then(|file| {
        state
            .compatible_accounts
            .lock()
            .unwrap()
            .iter()
            .find(|a| a.file_name.as_deref() == Some(file))
            .cloned()
    });
    let key = payload
        .api_key
        .as_deref()
        .map(str::trim)
        .filter(|k| !k.is_empty())
        .map(str::to_string)
        .or_else(|| existing.map(|a| a.api_key))
        .unwrap_or_default();
    if key.is_empty() {
        return failure("API key is required");
    }
    let openai = match normalize_url(payload.base_url.as_deref()) {
        Ok(u) => u,
        Err(e) => return failure(e),
    };
    let anthropic = match normalize_url(payload.anthropic_base_url.as_deref()) {
        Ok(u) => u,
        Err(e) => return failure(e),
    };
    let mut models = Vec::new();
    for (base, protocol) in [(openai, "openai"), (anthropic, "anthropic")] {
        let Some(base) = base else { continue };
        let url = super::api::endpoint(&base, protocol, "models");
        let mut request = state
            .client
            .get(url)
            .bearer_auth(&key)
            .timeout(std::time::Duration::from_secs(15));
        if protocol == "anthropic" {
            request = request
                .header("x-api-key", &key)
                .header("anthropic-version", "2023-06-01");
        }
        if let Ok(response) = request.send().await {
            if response.status().is_success() {
                if let Ok(value) = response.json::<Value>().await {
                    if let Some(data) = value["data"].as_array() {
                        models.extend(
                            data.iter()
                                .filter_map(|m| m["id"].as_str().map(str::to_string)),
                        );
                    }
                }
            }
        }
    }
    models.sort();
    models.dedup();
    if models.is_empty() {
        return failure("No models discovered. Check the URL and key, or enter model IDs manually if the API has no model list.");
    }
    Json(json!({"ok":true,"models":models})).into_response()
}
