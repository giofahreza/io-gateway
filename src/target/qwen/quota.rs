use base64::Engine;
use chrono::{DateTime, Utc};
use reqwest::header::HeaderMap;
use serde::Serialize;
use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet},
    time::Duration,
};

const TIER_NAME_FIELDS: &[&str] = &[
    "tier_name",
    "tier",
    "plan_name",
    "plan",
    "subscription_name",
    "subscription",
    "package_name",
    "package",
    "membership_name",
    "membership",
    "license_name",
    "license",
    "sku_name",
    "sku",
];

const TIER_ID_FIELDS: &[&str] = &[
    "tier_id",
    "plan_id",
    "subscription_id",
    "package_id",
    "membership_id",
    "license_id",
    "sku_id",
];

const ACCOUNT_TYPE_FIELDS: &[&str] = &[
    "account_type",
    "accounttype",
    "user_type",
    "usertype",
    "edition",
];

const CONTEXT_WINDOW_FIELDS: &[&str] = &[
    "context_window",
    "contextwindow",
    "context_length",
    "contextlength",
    "max_context_length",
    "maxcontextlength",
    "max_input_tokens",
    "maxinputtokens",
    "input_token_limit",
    "inputtokenlimit",
];

const OUTPUT_TOKEN_FIELDS: &[&str] = &[
    "max_output_tokens",
    "maxoutputtokens",
    "max_generation_length",
    "maxgenerationlength",
    "max_summary_generation_length",
    "maxsummarygenerationlength",
    "max_thinking_generation_length",
    "maxthinkinggenerationlength",
    "output_token_limit",
    "outputtokenlimit",
];

const QWEN_CHAT_API_BASE_URL: &str = "https://chat.qwen.ai/api";

#[derive(Clone)]
pub struct QuotaCacheEntry {
    pub fetched_at: std::time::Instant,
    pub summary: QuotaSummary,
    pub error: Option<String>,
}

#[derive(Default, Clone, Serialize)]
pub struct QuotaSummary {
    pub label: String,
    pub email: String,
    pub resource_url: String,
    pub tier_name: String,
    pub tier_id: String,
    pub tier_description: String,
    pub description: String,
    pub notes: Vec<String>,
    pub limits: Vec<RateLimitSummary>,
    pub models: Vec<ModelQuotaSummary>,
    pub raw_headers: Vec<HeaderSummary>,
}

#[derive(Default, Clone, Serialize)]
pub struct RateLimitSummary {
    pub label: String,
    pub scope: String,
    pub limit: Option<f64>,
    pub remaining: Option<f64>,
    pub used: Option<f64>,
    pub used_percent: Option<f64>,
    pub remaining_percent: Option<f64>,
    pub limit_text: String,
    pub remaining_text: String,
    pub used_text: String,
    pub reset_label: String,
}

#[derive(Default, Clone, Serialize)]
pub struct ModelQuotaSummary {
    pub model_id: String,
    pub display_name: String,
    pub owned_by: String,
    pub context_window: Option<u64>,
    pub max_output_tokens: Option<u64>,
    pub capabilities: Vec<String>,
    pub description: String,
}

#[derive(Default, Clone, Serialize)]
pub struct HeaderSummary {
    pub name: String,
    pub value: String,
}

pub async fn get_quota_summaries(state: &crate::AppState) -> Vec<Value> {
    let accounts = state.qwen_accounts.lock().unwrap().clone();
    let now = std::time::Instant::now();
    let mut results = Vec::with_capacity(accounts.len());

    for account in &accounts {
        let key = cache_key(account);
        let cached = {
            let cache = state.qwen_quota_cache.lock().unwrap();
            cache.get(&key).cloned()
        };

        let entry = if let Some(cached) = cached {
            if crate::quota_cache_entry_is_fresh(now, cached.fetched_at, &cached.summary) {
                cached
            } else {
                let fetched = fetch_account_quota(state, account).await;
                let mut cache = state.qwen_quota_cache.lock().unwrap();
                cache.insert(key.clone(), fetched.clone());
                fetched
            }
        } else {
            let fetched = fetch_account_quota(state, account).await;
            let mut cache = state.qwen_quota_cache.lock().unwrap();
            cache.insert(key.clone(), fetched.clone());
            fetched
        };

        results.push(json!({
            "label": entry.summary.label,
            "email": entry.summary.email,
            "resource_url": entry.summary.resource_url,
            "file_name": account.file_name.clone().unwrap_or_default(),
            "tier_name": entry.summary.tier_name,
            "tier_id": entry.summary.tier_id,
            "tier_description": entry.summary.tier_description,
            "description": entry.summary.description,
            "notes": entry.summary.notes,
            "limits": entry.summary.limits,
            "models": entry.summary.models,
            "raw_headers": entry.summary.raw_headers,
            "error": entry.error
        }));
    }

    results
}

fn cache_key(account: &super::accounts::QwenAccount) -> String {
    account
        .file_name
        .clone()
        .unwrap_or_else(|| account.label.clone())
}

async fn fetch_account_quota(
    state: &crate::AppState,
    account: &super::accounts::QwenAccount,
) -> QuotaCacheEntry {
    let resource_url = super::auth::base_url(state, account);
    let auth_value = load_auth_value(state, account.file_name.as_deref());

    let access_token = match super::auth::ensure_access_token(state, account).await {
        Ok(token) => token,
        Err(err) => {
            return QuotaCacheEntry {
                fetched_at: std::time::Instant::now(),
                summary: build_summary(
                    account,
                    &resource_url,
                    auth_value.as_ref(),
                    None,
                    None,
                    vec![],
                    vec![],
                    vec![],
                    Some(err.as_str()),
                ),
                error: Some(err),
            };
        }
    };

    let claims = parse_access_token_claims(&access_token);
    let config_value = fetch_config_metadata(&state.client, &access_token)
        .await
        .ok();
    match fetch_models_metadata(&state.client, &access_token).await {
        Ok((models, limits, raw_headers)) => QuotaCacheEntry {
            fetched_at: std::time::Instant::now(),
            summary: build_summary(
                account,
                &resource_url,
                auth_value.as_ref(),
                config_value.as_ref(),
                claims.as_ref(),
                limits,
                models,
                raw_headers,
                None,
            ),
            error: None,
        },
        Err(err) => QuotaCacheEntry {
            fetched_at: std::time::Instant::now(),
            summary: build_summary(
                account,
                &resource_url,
                auth_value.as_ref(),
                config_value.as_ref(),
                claims.as_ref(),
                vec![],
                vec![],
                vec![],
                Some(&err),
            ),
            error: Some(err),
        },
    }
}

fn build_summary(
    account: &super::accounts::QwenAccount,
    resource_url: &str,
    auth_value: Option<&Value>,
    config_value: Option<&Value>,
    claims: Option<&Value>,
    limits: Vec<RateLimitSummary>,
    models: Vec<ModelQuotaSummary>,
    raw_headers: Vec<HeaderSummary>,
    lookup_error: Option<&str>,
) -> QuotaSummary {
    let tier = infer_tier_info(auth_value, claims);
    let mut notes = Vec::new();
    notes.push(format!("Resource: {}", resource_url));
    notes.push(format!("Live metadata source: {}", QWEN_CHAT_API_BASE_URL));

    if let Some(subject) = account.subject.clone().or_else(|| {
        if !account.account_id.trim().is_empty() {
            Some(account.account_id.clone())
        } else {
            claims
                .and_then(|value| value.get("sub"))
                .and_then(value_to_string)
        }
    }) {
        if !subject.trim().is_empty() {
            notes.push(format!("Subject: {}", subject));
        }
    }

    if let Some(issuer) = claims
        .and_then(|value| value.get("iss"))
        .and_then(value_to_string)
    {
        if !issuer.trim().is_empty() {
            notes.push(format!("Issuer: {}", issuer));
        }
    }

    let audiences = claims
        .and_then(|value| find_string_list_field(value, &["aud"]))
        .unwrap_or_default();
    if !audiences.is_empty() {
        notes.push(format!("Audience: {}", audiences.join(", ")));
    }

    let scopes = claims
        .and_then(|value| find_string_list_field(value, &["scope", "scp"]))
        .unwrap_or_default();
    if !scopes.is_empty() {
        notes.push(format!("Scopes: {}", scopes.join(", ")));
    }

    if limits.is_empty() {
        notes.push(
            "No live Qwen rate-limit headers were returned by /api/models, so this view falls back to account metadata and accessible models."
                .to_string(),
        );
    } else {
        notes.push("Limit data comes from live headers returned by Qwen /api/models.".to_string());
    }

    if !models.is_empty() {
        notes.push(format!(
            "Fetched {} accessible model(s) from the live /api/models response.",
            models.len()
        ));
    }

    let config_has_payment_and_quota =
        config_flag(config_value, &["features", "enable_payment_and_quota"]);
    let config_has_subscription = config_flag(config_value, &["features", "enable_subscription"]);
    if let (Some(payment_and_quota), Some(subscription)) =
        (config_has_payment_and_quota, config_has_subscription)
    {
        notes.push(format!(
            "Upstream config reports enable_payment_and_quota={} and enable_subscription={}.",
            payment_and_quota, subscription
        ));
    }

    if let Some(err) = lookup_error.filter(|value| !value.trim().is_empty()) {
        notes.push(format!("Last live lookup failed: {}", err));
    }

    let description = if lookup_error.is_some() {
        "Qwen live metadata now comes from chat.qwen.ai/api/models and /api/config. No dedicated remaining-quota endpoint was found during research, so this view falls back to tier/config metadata plus accessible models when live lookups fail.".to_string()
    } else if config_has_payment_and_quota == Some(false) {
        "Qwen live metadata comes from chat.qwen.ai/api/models and /api/config. The current upstream config reports enable_payment_and_quota=false, so this view shows tier/config metadata and accessible models instead of a remaining-usage counter.".to_string()
    } else if limits.is_empty() {
        "Qwen live metadata comes from chat.qwen.ai/api/models and /api/config. The current upstream did not return standard rate-limit headers for this account, so this view shows tier/config metadata and accessible models.".to_string()
    } else {
        "Qwen live metadata comes from chat.qwen.ai/api/models and /api/config. Tier is inferred from saved auth metadata or token claims, and any live limits come from /api/models response headers.".to_string()
    };

    QuotaSummary {
        label: account.label.clone(),
        email: account.email.clone(),
        resource_url: resource_url.to_string(),
        tier_name: tier.name,
        tier_id: tier.id,
        tier_description: tier.description,
        description,
        notes,
        limits,
        models,
        raw_headers,
    }
}

#[derive(Default)]
struct TierInfo {
    name: String,
    id: String,
    description: String,
}

fn infer_tier_info(auth_value: Option<&Value>, claims: Option<&Value>) -> TierInfo {
    let auth_name = auth_value.and_then(|value| find_string_field(value, TIER_NAME_FIELDS));
    let claim_name = claims.and_then(|value| find_string_field(value, TIER_NAME_FIELDS));
    let auth_type = auth_value.and_then(|value| find_string_field(value, ACCOUNT_TYPE_FIELDS));
    let claim_type = claims.and_then(|value| find_string_field(value, ACCOUNT_TYPE_FIELDS));

    let (name, source) = if let Some(value) = auth_name {
        (value, "saved credential metadata")
    } else if let Some(value) = claim_name {
        (value, "access token claims")
    } else if let Some(value) = auth_type {
        (value, "saved credential metadata")
    } else if let Some(value) = claim_type {
        (value, "access token claims")
    } else {
        ("Unknown".to_string(), "no explicit tier field")
    };

    let id = auth_value
        .and_then(|value| find_string_field(value, TIER_ID_FIELDS))
        .or_else(|| claims.and_then(|value| find_string_field(value, TIER_ID_FIELDS)))
        .unwrap_or_default();

    let description = if source == "no explicit tier field" {
        "No explicit Qwen tier or subscription field was found in the saved auth file or access token.".to_string()
    } else if id.trim().is_empty() {
        format!("Tier inferred from {}.", source)
    } else {
        format!("Tier inferred from {}. Tier ID: {}.", source, id)
    };

    TierInfo {
        name,
        id,
        description,
    }
}

async fn fetch_models_metadata(
    client: &reqwest::Client,
    access_token: &str,
) -> Result<
    (
        Vec<ModelQuotaSummary>,
        Vec<RateLimitSummary>,
        Vec<HeaderSummary>,
    ),
    String,
> {
    let request = client
        .get(format!("{}/models", QWEN_CHAT_API_BASE_URL))
        .header("Accept", "application/json")
        .timeout(Duration::from_secs(30));
    let resp = super::auth::qwen_headers(request, access_token)
        .send()
        .await
        .map_err(|err| err.to_string())?;

    let status = resp.status();
    let headers = resp.headers().clone();
    let text = resp.text().await.map_err(|err| err.to_string())?;
    if !status.is_success() {
        return Err(format!(
            "Qwen models endpoint returned {}: {}",
            status, text
        ));
    }

    let value: Value = serde_json::from_str(&text).map_err(|err| err.to_string())?;
    let mut models = extract_models_from_response(&value);
    models.sort_by(|a, b| a.display_name.cmp(&b.display_name));
    let limits = extract_limit_summaries(&headers);
    let raw_headers = extract_rate_limit_headers(&headers);
    Ok((models, limits, raw_headers))
}

async fn fetch_config_metadata(
    client: &reqwest::Client,
    access_token: &str,
) -> Result<Value, String> {
    let request = client
        .get(format!("{}/config", QWEN_CHAT_API_BASE_URL))
        .header("Accept", "application/json")
        .timeout(Duration::from_secs(30));
    let resp = super::auth::qwen_headers(request, access_token)
        .send()
        .await
        .map_err(|err| err.to_string())?;

    let status = resp.status();
    let text = resp.text().await.map_err(|err| err.to_string())?;
    if !status.is_success() {
        return Err(format!(
            "Qwen config endpoint returned {}: {}",
            status, text
        ));
    }

    serde_json::from_str(&text).map_err(|err| err.to_string())
}

fn extract_models_from_response(value: &Value) -> Vec<ModelQuotaSummary> {
    let items = value
        .get("data")
        .and_then(|models| models.as_array())
        .or_else(|| value.get("models").and_then(|models| models.as_array()))
        .cloned()
        .unwrap_or_default();

    items
        .iter()
        .filter_map(|model| {
            let model_id = model
                .get("id")
                .and_then(|value| value.as_str())?
                .to_string();
            let info = model.get("info");
            let meta = info.and_then(|value| value.get("meta"));
            let display_name = model
                .get("display_name")
                .or_else(|| model.get("displayName"))
                .or_else(|| model.get("name"))
                .or_else(|| meta.and_then(|value| value.get("display_name")))
                .or_else(|| meta.and_then(|value| value.get("displayName")))
                .or_else(|| meta.and_then(|value| value.get("name")))
                .and_then(value_to_string)
                .unwrap_or_else(|| model_id.clone());

            Some(ModelQuotaSummary {
                model_id: model_id.clone(),
                display_name,
                owned_by: model
                    .get("owned_by")
                    .or_else(|| model.get("ownedBy"))
                    .or_else(|| model.get("provider"))
                    .and_then(value_to_string)
                    .unwrap_or_default(),
                context_window: find_u64_field(model, CONTEXT_WINDOW_FIELDS),
                max_output_tokens: find_u64_field(model, OUTPUT_TOKEN_FIELDS),
                capabilities: extract_model_capabilities(model),
                description: model
                    .get("description")
                    .or_else(|| meta.and_then(|value| value.get("description")))
                    .and_then(value_to_string)
                    .unwrap_or_default(),
            })
        })
        .collect()
}

fn extract_model_capabilities(model: &Value) -> Vec<String> {
    let mut values = Vec::new();

    for source in [
        Some(model),
        model.get("info"),
        model.get("info").and_then(|value| value.get("meta")),
    ]
    .into_iter()
    .flatten()
    {
        if let Some(items) = source
            .get("modalities")
            .or_else(|| source.get("modality"))
            .and_then(|value| value.as_array())
        {
            for item in items {
                if let Some(value) = item.as_str() {
                    values.push(value.to_string());
                }
            }
        }

        if let Some(map) = source
            .get("capabilities")
            .or_else(|| source.get("abilities"))
            .and_then(|value| value.as_object())
        {
            for (key, value) in map {
                let include = value
                    .as_bool()
                    .or_else(|| value.as_i64().map(|number| number > 0))
                    .unwrap_or_else(|| !value.is_null());
                if include {
                    values.push(key.to_string());
                }
            }
        }
    }

    values.sort();
    values.dedup();
    values
}

fn config_flag(value: Option<&Value>, path: &[&str]) -> Option<bool> {
    let mut current = value?;
    for segment in path {
        current = current.get(*segment)?;
    }
    current.as_bool()
}

fn extract_limit_summaries(headers: &HeaderMap) -> Vec<RateLimitSummary> {
    let mut limits = Vec::new();
    for (label, scope) in [
        ("Requests", "requests"),
        ("Tokens", "tokens"),
        ("Input Tokens", "input-tokens"),
        ("Output Tokens", "output-tokens"),
    ] {
        if let Some(summary) = extract_limit_summary(headers, label, scope) {
            limits.push(summary);
        }
    }
    limits
}

fn extract_limit_summary(
    headers: &HeaderMap,
    label: &str,
    scope: &str,
) -> Option<RateLimitSummary> {
    let limit_text = header_value(headers, &format!("x-ratelimit-limit-{}", scope))
        .or_else(|| header_value(headers, &format!("ratelimit-limit-{}", scope)))
        .unwrap_or_default();
    let remaining_text = header_value(headers, &format!("x-ratelimit-remaining-{}", scope))
        .or_else(|| header_value(headers, &format!("ratelimit-remaining-{}", scope)))
        .unwrap_or_default();
    let used_text = header_value(headers, &format!("x-ratelimit-used-{}", scope))
        .or_else(|| header_value(headers, &format!("ratelimit-used-{}", scope)))
        .unwrap_or_default();
    let reset_raw = header_value(headers, &format!("x-ratelimit-reset-{}", scope))
        .or_else(|| header_value(headers, &format!("ratelimit-reset-{}", scope)))
        .or_else(|| header_value(headers, "retry-after"))
        .unwrap_or_default();

    if limit_text.is_empty()
        && remaining_text.is_empty()
        && used_text.is_empty()
        && reset_raw.is_empty()
    {
        return None;
    }

    let limit = parse_numeric(&limit_text);
    let remaining = parse_numeric(&remaining_text);
    let used = parse_numeric(&used_text).or_else(|| match (limit, remaining) {
        (Some(limit), Some(remaining)) if limit >= remaining => Some(limit - remaining),
        _ => None,
    });
    let used_percent = match (used, limit) {
        (Some(used), Some(limit)) if limit > 0.0 => Some((used / limit) * 100.0),
        _ => None,
    };
    let remaining_percent = match (remaining, limit) {
        (Some(remaining), Some(limit)) if limit > 0.0 => Some((remaining / limit) * 100.0),
        _ => None,
    };

    Some(RateLimitSummary {
        label: label.to_string(),
        scope: scope.to_string(),
        limit,
        remaining,
        used,
        used_percent,
        remaining_percent,
        limit_text,
        remaining_text,
        used_text: if used_text.is_empty() {
            used.map(format_metric).unwrap_or_default()
        } else {
            used_text
        },
        reset_label: format_reset_value(&reset_raw),
    })
}

fn extract_rate_limit_headers(headers: &HeaderMap) -> Vec<HeaderSummary> {
    let mut values = headers
        .iter()
        .filter_map(|(name, value)| {
            let header_name = name.as_str().to_ascii_lowercase();
            if !header_name.starts_with("x-ratelimit-") && header_name != "retry-after" {
                return None;
            }
            Some(HeaderSummary {
                name: header_name,
                value: value.to_str().unwrap_or_default().to_string(),
            })
        })
        .collect::<Vec<_>>();
    values.sort_by(|a, b| a.name.cmp(&b.name));
    values
}

fn load_auth_value(state: &crate::AppState, file_name: Option<&str>) -> Option<Value> {
    let file_name = file_name?.trim();
    if file_name.is_empty() {
        return None;
    }

    let auth_dir = state
        .cfg
        .auth_dir
        .clone()
        .unwrap_or_else(|| "/root/dev/yow/io-gateway/auths".to_string());
    let path = std::path::Path::new(&auth_dir).join(file_name);
    let data = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&data).ok()
}

fn parse_access_token_claims(access_token: &str) -> Option<Value> {
    let mut parts = access_token.split('.');
    let _header = parts.next()?;
    let payload = parts.next()?;
    let _sig = parts.next()?;
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    serde_json::from_slice(&decoded).ok()
}

fn find_string_field(value: &Value, candidates: &[&str]) -> Option<String> {
    let candidate_keys = candidates
        .iter()
        .map(|value| normalize_key(value))
        .collect::<HashSet<_>>();
    find_string_field_inner(value, &candidate_keys)
}

fn find_string_field_inner(value: &Value, candidates: &HashSet<String>) -> Option<String> {
    match value {
        Value::Object(map) => {
            for (key, value) in map {
                if candidates.contains(&normalize_key(key)) {
                    if let Some(value) = value_to_string(value) {
                        let value = value.trim().to_string();
                        if !value.is_empty() {
                            return Some(value);
                        }
                    }
                }
            }
            for value in map.values() {
                if let Some(value) = find_string_field_inner(value, candidates) {
                    return Some(value);
                }
            }
            None
        }
        Value::Array(values) => values
            .iter()
            .find_map(|value| find_string_field_inner(value, candidates)),
        _ => None,
    }
}

fn find_string_list_field(value: &Value, candidates: &[&str]) -> Option<Vec<String>> {
    let candidate_keys = candidates
        .iter()
        .map(|value| normalize_key(value))
        .collect::<HashSet<_>>();
    find_string_list_field_inner(value, &candidate_keys)
}

fn find_string_list_field_inner(
    value: &Value,
    candidates: &HashSet<String>,
) -> Option<Vec<String>> {
    match value {
        Value::Object(map) => {
            for (key, value) in map {
                if candidates.contains(&normalize_key(key)) {
                    let items = match value {
                        Value::String(value) => split_string_list(value),
                        Value::Array(values) => values
                            .iter()
                            .filter_map(value_to_string)
                            .map(|value| value.trim().to_string())
                            .filter(|value| !value.is_empty())
                            .collect::<Vec<_>>(),
                        _ => Vec::new(),
                    };
                    if !items.is_empty() {
                        return Some(items);
                    }
                }
            }
            for value in map.values() {
                if let Some(values) = find_string_list_field_inner(value, candidates) {
                    return Some(values);
                }
            }
            None
        }
        Value::Array(values) => values
            .iter()
            .find_map(|value| find_string_list_field_inner(value, candidates)),
        _ => None,
    }
}

fn find_u64_field(value: &Value, candidates: &[&str]) -> Option<u64> {
    let candidate_keys = candidates
        .iter()
        .map(|value| normalize_key(value))
        .collect::<HashSet<_>>();
    find_u64_field_inner(value, &candidate_keys)
}

fn find_u64_field_inner(value: &Value, candidates: &HashSet<String>) -> Option<u64> {
    match value {
        Value::Object(map) => {
            for (key, value) in map {
                if candidates.contains(&normalize_key(key)) {
                    if let Some(number) = value
                        .as_u64()
                        .or_else(|| value.as_i64().and_then(|number| u64::try_from(number).ok()))
                        .or_else(|| {
                            value
                                .as_str()
                                .and_then(|number| number.trim().parse::<u64>().ok())
                        })
                    {
                        return Some(number);
                    }
                }
            }
            for value in map.values() {
                if let Some(value) = find_u64_field_inner(value, candidates) {
                    return Some(value);
                }
            }
            None
        }
        Value::Array(values) => values
            .iter()
            .find_map(|value| find_u64_field_inner(value, candidates)),
        _ => None,
    }
}

fn value_to_string(value: &Value) -> Option<String> {
    match value {
        Value::String(value) => Some(value.to_string()),
        Value::Number(value) => Some(value.to_string()),
        Value::Bool(value) => Some(value.to_string()),
        _ => None,
    }
}

fn split_string_list(value: &str) -> Vec<String> {
    value
        .split(|ch: char| ch.is_whitespace() || ch == ',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| value.to_string())
        .collect()
}

fn normalize_key(value: &str) -> String {
    value
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .flat_map(|ch| ch.to_lowercase())
        .collect()
}

fn header_value(headers: &HeaderMap, name: &str) -> Option<String> {
    headers.iter().find_map(|(key, value)| {
        if key.as_str().eq_ignore_ascii_case(name) {
            value.to_str().ok().map(|value| value.to_string())
        } else {
            None
        }
    })
}

fn parse_numeric(value: &str) -> Option<f64> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    trimmed.parse::<f64>().ok()
}

fn format_metric(value: f64) -> String {
    if (value.fract()).abs() < f64::EPSILON {
        format!("{:.0}", value)
    } else {
        format!("{:.2}", value)
    }
}

fn format_reset_value(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return String::new();
    }

    if let Some(seconds) = parse_seconds_value(trimmed) {
        return format_remaining_time(seconds);
    }

    if let Ok(parsed) = DateTime::parse_from_rfc3339(trimmed) {
        let delta = parsed.with_timezone(&Utc) - Utc::now();
        return format_remaining_time(delta.num_seconds());
    }

    trimmed.to_string()
}

fn parse_seconds_value(value: &str) -> Option<i64> {
    if let Ok(seconds) = value.parse::<f64>() {
        return Some(seconds.round() as i64);
    }

    if let Some(stripped) = value.strip_suffix("ms") {
        if let Ok(milliseconds) = stripped.trim().parse::<f64>() {
            return Some((milliseconds / 1000.0).round() as i64);
        }
    }

    let mut total_seconds = 0.0;
    let mut current = String::new();
    let mut saw_unit = false;
    for ch in value.chars() {
        if ch.is_ascii_digit() || ch == '.' {
            current.push(ch);
            continue;
        }
        if ch.is_whitespace() || ch == ',' {
            continue;
        }

        let number = current.parse::<f64>().ok()?;
        current.clear();
        let multiplier = match ch {
            's' | 'S' => 1.0,
            'm' | 'M' => 60.0,
            'h' | 'H' => 3600.0,
            'd' | 'D' => 86_400.0,
            _ => return None,
        };
        total_seconds += number * multiplier;
        saw_unit = true;
    }

    if saw_unit && current.is_empty() {
        Some(total_seconds.round() as i64)
    } else {
        None
    }
}

fn format_remaining_time(seconds: i64) -> String {
    if seconds <= 0 {
        return "reset now".to_string();
    }

    let days = seconds / 86_400;
    let hours = (seconds % 86_400) / 3_600;
    let mins = (seconds % 3_600) / 60;
    if days > 0 {
        format!("{}d {}h", days, hours)
    } else if hours > 0 {
        format!("{}h {}m", hours, mins)
    } else {
        format!("{}m", mins.max(1))
    }
}

pub fn prune_cache(
    cache: &mut HashMap<String, QuotaCacheEntry>,
    accounts: &[super::accounts::QwenAccount],
) {
    let active_keys = accounts.iter().map(cache_key).collect::<HashSet<_>>();
    cache.retain(|key, _| active_keys.contains(key));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_models_from_response_reads_nested_qwen_api_shape() {
        let value = json!({
            "data": [
                {
                    "id": "qwen3.7-plus",
                    "name": "Qwen3.7-Plus",
                    "owned_by": "qwen",
                    "info": {
                        "meta": {
                            "description": "nested description",
                            "max_context_length": 1000000,
                            "max_summary_generation_length": 65536,
                            "capabilities": {
                                "vision": true,
                                "search": true
                            },
                            "modality": ["text", "image"]
                        }
                    }
                }
            ]
        });

        let models = extract_models_from_response(&value);
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].model_id, "qwen3.7-plus");
        assert_eq!(models[0].display_name, "Qwen3.7-Plus");
        assert_eq!(models[0].description, "nested description");
        assert_eq!(models[0].context_window, Some(1_000_000));
        assert_eq!(models[0].max_output_tokens, Some(65_536));
        assert!(models[0].capabilities.contains(&"vision".to_string()));
        assert!(models[0].capabilities.contains(&"search".to_string()));
        assert!(models[0].capabilities.contains(&"text".to_string()));
        assert!(models[0].capabilities.contains(&"image".to_string()));
    }

    #[test]
    fn config_flag_reads_nested_boolean() {
        let value = json!({
            "features": {
                "enable_payment_and_quota": false,
                "enable_subscription": true
            }
        });

        assert_eq!(
            config_flag(Some(&value), &["features", "enable_payment_and_quota"]),
            Some(false)
        );
        assert_eq!(
            config_flag(Some(&value), &["features", "enable_subscription"]),
            Some(true)
        );
        assert_eq!(config_flag(Some(&value), &["features", "missing"]), None);
    }
}
