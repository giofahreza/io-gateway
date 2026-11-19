//! Claude OAuth usage/quota fetcher.
//!
//! Claude Code OAuth accounts expose subscription quota through
//! `GET /api/oauth/usage` with the `oauth-2025-04-20` beta header. Normal
//! Anthropic responses can also include `anthropic-ratelimit-*` headers; those
//! are observed passively so quota remains useful if the active probe fails.

use reqwest::header::HeaderMap;
use serde::Serialize;
use serde_json::{json, Value};
use std::time::Duration;

const REQUEST_TIMEOUT_SECS: u64 = 20;
const USAGE_PATH: &str = "/api/oauth/usage";
const OAUTH_USAGE_BETA: &str = "oauth-2025-04-20";
const CLAUDE_CODE_USER_AGENT: &str = super::CLAUDE_CODE_CLI_USER_AGENT;

#[derive(Clone, Debug)]
pub struct QuotaCacheEntry {
    pub fetched_at: std::time::Instant,
    pub summary: QuotaSummary,
    pub error: Option<String>,
}

#[derive(Default, Clone, Debug, Serialize)]
pub struct QuotaSummary {
    pub label: String,
    pub email: Option<String>,
    pub organization_uuid: String,
    pub account_id: String,
    pub file_name: String,
    pub is_available: bool,
    pub source: String,
    pub status_msg: String,
    pub current_window: Option<QuotaBucketSummary>,
    pub weekly: Option<QuotaBucketSummary>,
    pub additional_rate_limits: Vec<AdditionalRateLimitSummary>,
    pub limits: Vec<RateLimitSummary>,
    pub models: Vec<ModelSummary>,
    pub rate_limit_headers: Vec<HeaderSummary>,
    pub credits: Option<Value>,
    pub rate_limit_reset_credits: Option<Value>,
    pub raw_usage: Value,
}

#[derive(Default, Clone, Debug, Serialize)]
pub struct QuotaBucketSummary {
    pub used_percent: Option<f64>,
    pub remaining_percent: Option<f64>,
    pub reset_label: String,
    pub reset_at: Option<String>,
}

#[derive(Default, Clone, Debug, Serialize)]
pub struct AdditionalRateLimitSummary {
    pub display_name: String,
    pub weekly: Option<QuotaBucketSummary>,
}

#[derive(Default, Clone, Debug, Serialize)]
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

#[derive(Default, Clone, Debug, Serialize)]
pub struct ModelSummary {
    pub model_id: String,
    pub display_name: String,
}

#[derive(Default, Clone, Debug, Serialize)]
pub struct HeaderSummary {
    pub name: String,
    pub value: String,
}

pub async fn get_quota_summaries(state: &crate::AppState) -> Vec<Value> {
    let accounts = state.claude_accounts.lock().unwrap().clone();
    let now = std::time::Instant::now();
    let mut results = Vec::with_capacity(accounts.len());

    for account in &accounts {
        let key = cache_key(account);
        let cached = {
            let cache = state.claude_quota_cache.lock().unwrap();
            cache.get(&key).cloned()
        };

        let entry = if let Some(cached) = cached {
            if crate::quota_cache_entry_is_fresh(now, cached.fetched_at, &cached.summary) {
                cached
            } else {
                let fetched = fetch_account_quota(state, account).await;
                let entry = merge_fetch_with_stale(cached, fetched);
                let mut cache = state.claude_quota_cache.lock().unwrap();
                cache.insert(key.clone(), entry.clone());
                entry
            }
        } else {
            let fetched = fetch_account_quota(state, account).await;
            let mut cache = state.claude_quota_cache.lock().unwrap();
            cache.insert(key.clone(), fetched.clone());
            fetched
        };

        let mut payload = summary_json(account, &entry.summary);
        payload["reset_automation"] = super::reset_automation::account_policy(&state.cfg, account);
        if let Some(action) = super::resets::last_action(&state.cfg, account) {
            payload["last_reset_action"] = action;
        }
        if let Some(err) = entry.error {
            if let Some(obj) = payload.as_object_mut() {
                obj.insert("error".to_string(), Value::String(err.clone()));
                if obj
                    .get("status_msg")
                    .and_then(|value| value.as_str())
                    .unwrap_or_default()
                    .is_empty()
                {
                    obj.insert("status_msg".to_string(), Value::String(err));
                }
            }
        }
        results.push(payload);
    }

    results
}

pub fn observe_response_headers(
    state: &crate::AppState,
    account: &super::accounts::ClaudeAccount,
    headers: &HeaderMap,
) {
    if !headers
        .iter()
        .any(|(name, _)| name.as_str().starts_with("anthropic-ratelimit-"))
    {
        return;
    }

    let key = cache_key(account);
    let header_summary = summary_from_headers(account, headers);
    let mut cache = state.claude_quota_cache.lock().unwrap();
    let merged = if let Some(existing) = cache.get(&key).cloned() {
        QuotaCacheEntry {
            fetched_at: std::time::Instant::now(),
            summary: merge_summaries(existing.summary, header_summary),
            error: existing.error,
        }
    } else {
        QuotaCacheEntry {
            fetched_at: std::time::Instant::now(),
            summary: header_summary,
            error: None,
        }
    };
    cache.insert(key, merged);
}

fn merge_fetch_with_stale(stale: QuotaCacheEntry, fetched: QuotaCacheEntry) -> QuotaCacheEntry {
    if fetched.error.is_none() {
        return fetched;
    }
    if !has_any_quota(&stale.summary) {
        return fetched;
    }
    let mut summary = stale.summary;
    summary.status_msg = format!(
        "{} Last live Claude usage probe failed: {}",
        summary.status_msg,
        fetched.error.clone().unwrap_or_default()
    )
    .trim()
    .to_string();
    QuotaCacheEntry {
        fetched_at: std::time::Instant::now(),
        summary,
        error: fetched.error,
    }
}

fn merge_summaries(mut base: QuotaSummary, incoming: QuotaSummary) -> QuotaSummary {
    if incoming.current_window.is_some() {
        base.current_window = incoming.current_window;
    }
    if incoming.weekly.is_some() {
        base.weekly = incoming.weekly;
    }
    if !incoming.additional_rate_limits.is_empty() {
        base.additional_rate_limits = incoming.additional_rate_limits;
    }
    if !incoming.limits.is_empty() {
        base.limits = incoming.limits;
    }
    if !incoming.rate_limit_headers.is_empty() {
        base.rate_limit_headers = incoming.rate_limit_headers;
    }
    if !incoming.source.is_empty() {
        base.source = incoming.source;
    }
    if !incoming.status_msg.is_empty() {
        base.status_msg = incoming.status_msg;
    }
    base.is_available = base.is_available || incoming.is_available;
    base
}

fn summary_json(account: &super::accounts::ClaudeAccount, summary: &QuotaSummary) -> Value {
    json!({
        "label": if summary.label.is_empty() { account.label.clone() } else { summary.label.clone() },
        "email": summary.email.clone().or_else(|| account.email.clone()),
        "organization_uuid": if summary.organization_uuid.is_empty() { account.organization_uuid.clone() } else { summary.organization_uuid.clone() },
        "account_id": if summary.account_id.is_empty() { account.account_id.clone() } else { summary.account_id.clone() },
        "file_name": if summary.file_name.is_empty() { account.file_name.clone().unwrap_or_default() } else { summary.file_name.clone() },
        "is_available": account.enabled && summary.is_available,
        "source": summary.source,
        "status_msg": summary.status_msg,
        "current_window": summary.current_window,
        "weekly": summary.weekly,
        "additional_rate_limits": summary.additional_rate_limits,
        "limits": summary.limits,
        "models": summary.models,
        "rate_limit_headers": summary.rate_limit_headers,
        "credits": summary.credits,
        "rate_limit_reset_credits": summary.rate_limit_reset_credits,
        "raw_usage": summary.raw_usage,
    })
}

fn cache_key(account: &super::accounts::ClaudeAccount) -> String {
    account
        .file_name
        .clone()
        .unwrap_or_else(|| account.label.clone())
}

async fn fetch_account_quota(
    state: &crate::AppState,
    account: &super::accounts::ClaudeAccount,
) -> QuotaCacheEntry {
    let mut summary = base_summary(account);
    if !account.enabled {
        summary.status_msg = "Claude account is disabled.".to_string();
        return QuotaCacheEntry {
            fetched_at: std::time::Instant::now(),
            summary,
            error: None,
        };
    }

    let access_token = match super::auth::ensure_access_token(state, account).await {
        Ok(token) => token,
        Err(err) => {
            summary.status_msg = err.clone();
            return QuotaCacheEntry {
                fetched_at: std::time::Instant::now(),
                summary,
                error: Some(err),
            };
        }
    };

    match fetch_oauth_usage(&state.client, account, &access_token).await {
        Ok(raw) => {
            summary = summary_from_usage(account, raw);
            QuotaCacheEntry {
                fetched_at: std::time::Instant::now(),
                summary,
                error: None,
            }
        }
        Err(err) => {
            summary.status_msg = format!(
                "Claude usage probe failed. Passive rate-limit headers will populate after this account handles traffic. {}",
                err
            );
            QuotaCacheEntry {
                fetched_at: std::time::Instant::now(),
                summary,
                error: Some(err),
            }
        }
    }
}

pub(super) async fn fetch_oauth_usage(
    client: &reqwest::Client,
    account: &super::accounts::ClaudeAccount,
    access_token: &str,
) -> Result<Value, String> {
    let base = super::auth::api_base_url(account.api_base_url.as_deref());
    let resp = client
        .get(format!(
            "{}{}?cedar_ember=1&at_wall=1",
            base.trim_end_matches('/'),
            USAGE_PATH
        ))
        .header("Authorization", format!("Bearer {}", access_token.trim()))
        .header("Accept", "application/json")
        .header("Content-Type", "application/json")
        .header("User-Agent", CLAUDE_CODE_USER_AGENT)
        .header("anthropic-beta", OAUTH_USAGE_BETA)
        .timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS))
        .send()
        .await
        .map_err(|err| format!("Claude usage request failed: {}", err))?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(format!("Claude usage returned {}", status));
    }
    serde_json::from_str(&text).map_err(|err| format!("Claude usage JSON parse failed: {}", err))
}

fn base_summary(account: &super::accounts::ClaudeAccount) -> QuotaSummary {
    QuotaSummary {
        label: account.label.clone(),
        email: account.email.clone(),
        organization_uuid: account.organization_uuid.clone(),
        account_id: account.account_id.clone(),
        file_name: account.file_name.clone().unwrap_or_default(),
        is_available: account.enabled,
        models: model_summaries(&account.models),
        raw_usage: Value::Null,
        ..Default::default()
    }
}

fn summary_from_usage(account: &super::accounts::ClaudeAccount, raw: Value) -> QuotaSummary {
    let mut summary = base_summary(account);
    summary.source = "oauth_usage".to_string();
    summary.current_window = parse_oauth_bucket(raw.get("five_hour"));
    summary.weekly = parse_oauth_bucket(raw.get("seven_day"));
    summary.additional_rate_limits = [
        ("seven_day_sonnet", "Sonnet"),
        ("seven_day_opus", "Opus"),
        ("seven_day_omelette", "Fable"),
        ("seven_day_oauth_apps", "OAuth apps"),
        ("seven_day_cowork", "Claude Code teams"),
    ]
    .into_iter()
    .filter_map(|(key, label)| {
        let weekly = parse_oauth_bucket(raw.get(key))?;
        Some(AdditionalRateLimitSummary {
            display_name: label.to_string(),
            weekly: Some(weekly),
        })
    })
    .collect();
    summary.limits = usage_limit_summaries(raw.get("limits"));
    if let Some(extra_usage) = extra_usage_limit(raw.get("extra_usage")) {
        summary.limits.push(extra_usage);
    }
    summary.credits = usage_credits(&raw);
    summary.rate_limit_reset_credits = super::resets::summarize(&raw);
    summary.raw_usage = raw;
    summary.is_available = true;
    summary.status_msg = if has_any_quota(&summary) {
        "Claude usage loaded from /api/oauth/usage.".to_string()
    } else {
        "Claude usage endpoint responded but did not include quota buckets for this account."
            .to_string()
    };
    summary
}

/// Keep monetary credits separate from a spend cap and from reset grants.
/// Current Claude money objects carry integer minor units and an exponent.
fn usage_credits(raw: &Value) -> Option<Value> {
    let spend = raw.get("spend").filter(|v| v.is_object());
    let extra = raw.get("extra_usage").filter(|v| v.is_object());
    if spend.is_none() && extra.is_none() {
        return None;
    }
    let balance = spend.and_then(|s| money(s.get("balance")));
    let used = spend.and_then(|s| money(s.get("used")));
    let limit = spend.and_then(|s| money(s.get("limit")));
    let enabled = spend
        .and_then(|s| s.get("enabled").and_then(Value::as_bool))
        .or_else(|| extra.and_then(|e| e.get("is_enabled").and_then(Value::as_bool)));
    Some(json!({
        "enabled": enabled,
        "balance": balance.as_ref().map(|m| &m.0),
        "currency": balance.as_ref().or(used.as_ref()).or(limit.as_ref()).map(|m| &m.1),
        "used": used.as_ref().map(|m| &m.0),
        "monthly_limit": limit.as_ref().map(|m| &m.0),
        "note": if balance.is_none() { "Prepaid balance not reported; a spend limit is not a credit balance." } else { "" }
    }))
}

fn money(value: Option<&Value>) -> Option<(String, String)> {
    let value = value?.as_object()?;
    let amount = value.get("amount_minor")?.as_i64()?;
    let exponent = value.get("exponent")?.as_u64()?;
    if exponent > 9 {
        return None;
    }
    let currency = value.get("currency")?.as_str()?;
    if currency.len() != 3 || !currency.bytes().all(|b| b.is_ascii_uppercase()) {
        return None;
    }
    let scale = 10_u64.pow(exponent as u32);
    let abs = amount.unsigned_abs();
    let sign = if amount < 0 { "-" } else { "" };
    let text = if exponent == 0 {
        amount.to_string()
    } else {
        format!(
            "{sign}{}.{:0width$}",
            abs / scale,
            abs % scale,
            width = exponent as usize
        )
    };
    Some((text, currency.to_string()))
}

fn summary_from_headers(
    account: &super::accounts::ClaudeAccount,
    headers: &HeaderMap,
) -> QuotaSummary {
    let mut summary = base_summary(account);
    summary.source = "response_headers".to_string();
    summary.current_window = parse_header_bucket(
        headers,
        &[
            "anthropic-ratelimit-unified-5h-utilization",
            "anthropic-ratelimit-unified-five-hour-utilization",
            "anthropic-ratelimit-unified-five_hour-utilization",
        ],
        &[
            "anthropic-ratelimit-unified-5h-reset",
            "anthropic-ratelimit-unified-five-hour-reset",
            "anthropic-ratelimit-unified-five_hour-reset",
        ],
    );
    summary.weekly = parse_header_bucket(
        headers,
        &[
            "anthropic-ratelimit-unified-7d-utilization",
            "anthropic-ratelimit-unified-seven-day-utilization",
            "anthropic-ratelimit-unified-seven_day-utilization",
        ],
        &[
            "anthropic-ratelimit-unified-7d-reset",
            "anthropic-ratelimit-unified-seven-day-reset",
            "anthropic-ratelimit-unified-seven_day-reset",
        ],
    );
    summary.additional_rate_limits = [
        (
            "Sonnet",
            &[
                "anthropic-ratelimit-unified-7d-sonnet-utilization",
                "anthropic-ratelimit-unified-seven-day-sonnet-utilization",
                "anthropic-ratelimit-unified-seven_day_sonnet-utilization",
                "anthropic-ratelimit-unified-seven_day-sonnet-utilization",
            ][..],
            &[
                "anthropic-ratelimit-unified-7d-sonnet-reset",
                "anthropic-ratelimit-unified-seven-day-sonnet-reset",
                "anthropic-ratelimit-unified-seven_day_sonnet-reset",
                "anthropic-ratelimit-unified-seven_day-sonnet-reset",
            ][..],
        ),
        (
            "Opus",
            &[
                "anthropic-ratelimit-unified-7d-opus-utilization",
                "anthropic-ratelimit-unified-seven-day-opus-utilization",
                "anthropic-ratelimit-unified-seven_day_opus-utilization",
                "anthropic-ratelimit-unified-seven_day-opus-utilization",
            ][..],
            &[
                "anthropic-ratelimit-unified-7d-opus-reset",
                "anthropic-ratelimit-unified-seven-day-opus-reset",
                "anthropic-ratelimit-unified-seven_day_opus-reset",
                "anthropic-ratelimit-unified-seven_day-opus-reset",
            ][..],
        ),
    ]
    .into_iter()
    .filter_map(|(label, usage_keys, reset_keys)| {
        let weekly = parse_header_bucket(headers, usage_keys, reset_keys)?;
        Some(AdditionalRateLimitSummary {
            display_name: label.to_string(),
            weekly: Some(weekly),
        })
    })
    .collect();
    summary.limits = ["requests", "tokens"]
        .into_iter()
        .filter_map(|scope| parse_standard_limit(headers, scope))
        .collect();
    summary.rate_limit_headers = extract_rate_limit_headers(headers);
    summary.is_available = true;
    let status = header_value(headers, "anthropic-ratelimit-unified-status").unwrap_or_default();
    summary.status_msg = if status.is_empty() {
        "Claude quota observed from anthropic-ratelimit response headers.".to_string()
    } else {
        format!("Claude quota observed from anthropic-ratelimit response headers ({status}).")
    };
    summary
}

fn model_summaries(models: &[super::accounts::ClaudeModelInfo]) -> Vec<ModelSummary> {
    models
        .iter()
        .map(|model| ModelSummary {
            model_id: model.id.clone(),
            display_name: model
                .display_name
                .clone()
                .unwrap_or_else(|| model.id.clone()),
        })
        .collect()
}

fn parse_oauth_bucket(value: Option<&Value>) -> Option<QuotaBucketSummary> {
    let bucket = value?.as_object()?;
    let used_percent = bucket
        .get("used_percentage")
        .or_else(|| bucket.get("usedPercentage"))
        .or_else(|| bucket.get("utilization"))
        .and_then(number_value);
    let reset_at = bucket
        .get("resets_at")
        .or_else(|| bucket.get("resetsAt"))
        .or_else(|| bucket.get("reset_at"))
        .or_else(|| bucket.get("resetAt"))
        .and_then(parse_reset_value);
    Some(QuotaBucketSummary {
        used_percent: used_percent.map(|value| value.clamp(0.0, 100.0)),
        remaining_percent: used_percent.map(|value| (100.0 - value).clamp(0.0, 100.0)),
        reset_label: reset_at
            .as_ref()
            .map(format_reset_label)
            .unwrap_or_default(),
        reset_at: reset_at.map(|dt| dt.to_rfc3339()),
    })
}

fn parse_header_bucket(
    headers: &HeaderMap,
    usage_keys: &[&str],
    reset_keys: &[&str],
) -> Option<QuotaBucketSummary> {
    let used_percent = usage_keys
        .iter()
        .find_map(|key| header_value(headers, key))
        .and_then(|value| parse_number(&value))
        .map(|value| {
            let percent = if value <= 1.0 { value * 100.0 } else { value };
            percent.clamp(0.0, 100.0)
        });
    let reset_at = reset_keys
        .iter()
        .find_map(|key| header_value(headers, key))
        .and_then(|value| parse_reset_string(&value));

    if used_percent.is_none() && reset_at.is_none() {
        return None;
    }
    Some(QuotaBucketSummary {
        used_percent,
        remaining_percent: used_percent.map(|value| (100.0 - value).clamp(0.0, 100.0)),
        reset_label: reset_at
            .as_ref()
            .map(format_reset_label)
            .unwrap_or_default(),
        reset_at: reset_at.map(|dt| dt.to_rfc3339()),
    })
}

fn parse_standard_limit(headers: &HeaderMap, scope: &str) -> Option<RateLimitSummary> {
    let limit = header_value(headers, &format!("anthropic-ratelimit-{scope}-limit"))
        .and_then(|value| parse_number(&value));
    let remaining = header_value(headers, &format!("anthropic-ratelimit-{scope}-remaining"))
        .and_then(|value| parse_number(&value));
    let reset_at = header_value(headers, &format!("anthropic-ratelimit-{scope}-reset"))
        .and_then(|value| parse_reset_string(&value));

    if limit.is_none() && remaining.is_none() && reset_at.is_none() {
        return None;
    }
    let used = match (limit, remaining) {
        (Some(limit), Some(remaining)) => Some((limit - remaining).max(0.0)),
        _ => None,
    };
    let used_percent = match (used, limit) {
        (Some(used), Some(limit)) if limit > 0.0 => {
            Some(((used / limit) * 100.0).clamp(0.0, 100.0))
        }
        _ => None,
    };
    let remaining_percent = match (remaining, limit) {
        (Some(remaining), Some(limit)) if limit > 0.0 => {
            Some(((remaining / limit) * 100.0).clamp(0.0, 100.0))
        }
        _ => None,
    };
    Some(RateLimitSummary {
        label: match scope {
            "requests" => "Requests".to_string(),
            "tokens" => "Tokens".to_string(),
            _ => scope.to_string(),
        },
        scope: scope.to_string(),
        limit,
        remaining,
        used,
        used_percent,
        remaining_percent,
        limit_text: compact_number(limit),
        remaining_text: compact_number(remaining),
        used_text: compact_number(used),
        reset_label: reset_at
            .as_ref()
            .map(format_reset_label)
            .unwrap_or_default(),
    })
}

fn extra_usage_limit(value: Option<&Value>) -> Option<RateLimitSummary> {
    let extra = value?.as_object()?;
    let enabled = extra
        .get("is_enabled")
        .or_else(|| extra.get("enabled"))
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    if !enabled {
        return None;
    }
    let limit = ["monthly_limit", "monthlyLimit", "limit", "quota"]
        .into_iter()
        .find_map(|key| extra.get(key).and_then(number_value));
    let used = [
        "used_credits",
        "used",
        "usage",
        "current_usage",
        "currentUsage",
        "amount_used",
        "amountUsed",
    ]
    .into_iter()
    .find_map(|key| extra.get(key).and_then(number_value));
    if limit.is_none() && used.is_none() {
        return None;
    }
    let remaining = match (limit, used) {
        (Some(limit), Some(used)) => Some((limit - used).max(0.0)),
        _ => None,
    };
    let used_percent = match (used, limit) {
        (Some(used), Some(limit)) if limit > 0.0 => {
            Some(((used / limit) * 100.0).clamp(0.0, 100.0))
        }
        _ => None,
    };
    Some(RateLimitSummary {
        label: "Extra usage".to_string(),
        scope: "extra_usage".to_string(),
        limit,
        remaining,
        used,
        used_percent,
        remaining_percent: match (remaining, limit) {
            (Some(remaining), Some(limit)) if limit > 0.0 => {
                Some(((remaining / limit) * 100.0).clamp(0.0, 100.0))
            }
            _ => None,
        },
        limit_text: compact_number(limit),
        remaining_text: compact_number(remaining),
        used_text: compact_number(used),
        reset_label: String::new(),
    })
}

fn usage_limit_summaries(value: Option<&Value>) -> Vec<RateLimitSummary> {
    value
        .and_then(Value::as_array)
        .map(|limits| {
            limits
                .iter()
                .filter_map(usage_limit_summary)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default()
}

fn usage_limit_summary(value: &Value) -> Option<RateLimitSummary> {
    let obj = value.as_object()?;
    let kind = obj.get("kind").and_then(Value::as_str).unwrap_or_default();
    let group = obj.get("group").and_then(Value::as_str).unwrap_or_default();
    let percent = obj
        .get("percent")
        .or_else(|| obj.get("utilization"))
        .or_else(|| obj.get("used_percentage"))
        .and_then(number_value)
        .map(|value| value.clamp(0.0, 100.0));
    let reset_at = obj
        .get("resets_at")
        .or_else(|| obj.get("reset_at"))
        .or_else(|| obj.get("resetsAt"))
        .or_else(|| obj.get("resetAt"))
        .and_then(parse_reset_value);

    if percent.is_none() && reset_at.is_none() {
        return None;
    }

    let label = usage_limit_label(obj, kind, group);
    let remaining = percent.map(|value| (100.0 - value).clamp(0.0, 100.0));
    Some(RateLimitSummary {
        label,
        scope: if kind.is_empty() {
            group.to_string()
        } else {
            kind.to_string()
        },
        limit: Some(100.0),
        remaining,
        used: percent,
        used_percent: percent,
        remaining_percent: remaining,
        limit_text: "100%".to_string(),
        remaining_text: percent_text(remaining),
        used_text: percent_text(percent),
        reset_label: reset_at
            .as_ref()
            .map(format_reset_label)
            .unwrap_or_default(),
    })
}

fn usage_limit_label(obj: &serde_json::Map<String, Value>, kind: &str, group: &str) -> String {
    let scope = obj.get("scope").and_then(Value::as_object);
    let model_name = scope
        .and_then(|scope| scope.get("model"))
        .and_then(Value::as_object)
        .and_then(|model| {
            model
                .get("display_name")
                .or_else(|| model.get("id"))
                .and_then(Value::as_str)
        })
        .map(str::trim)
        .filter(|value| !value.is_empty());

    if let Some(model_name) = model_name {
        let suffix = if kind.contains("weekly") || group.eq_ignore_ascii_case("weekly") {
            "weekly"
        } else if kind.contains("session") || group.eq_ignore_ascii_case("session") {
            "session"
        } else {
            kind
        };
        return if suffix.is_empty() {
            model_name.to_string()
        } else {
            format!("{} {}", model_name, humanize_limit_name(suffix))
        };
    }

    match kind {
        "weekly_all" => "Weekly all".to_string(),
        "weekly_scoped" => "Weekly scoped".to_string(),
        "session" => "Session".to_string(),
        _ if !kind.is_empty() => humanize_limit_name(kind),
        _ if !group.is_empty() => humanize_limit_name(group),
        _ => "Usage limit".to_string(),
    }
}

fn humanize_limit_name(value: &str) -> String {
    value
        .split(['_', '-'])
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(first) => {
                    let mut out = first.to_uppercase().collect::<String>();
                    out.push_str(chars.as_str());
                    out
                }
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn percent_text(value: Option<f64>) -> String {
    value
        .map(|value| {
            if (value.fract()).abs() < f64::EPSILON {
                format!("{}%", value as i64)
            } else {
                format!("{:.1}%", value)
            }
        })
        .unwrap_or_default()
}

fn extract_rate_limit_headers(headers: &HeaderMap) -> Vec<HeaderSummary> {
    let mut out = headers
        .iter()
        .filter_map(|(name, value)| {
            let name = name.as_str().to_ascii_lowercase();
            if !name.starts_with("anthropic-ratelimit-") && name != "retry-after" {
                return None;
            }
            Some(HeaderSummary {
                name,
                value: value.to_str().unwrap_or_default().to_string(),
            })
        })
        .collect::<Vec<_>>();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

fn header_value(headers: &HeaderMap, key: &str) -> Option<String> {
    headers
        .get(key)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| value.to_string())
}

fn number_value(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str().and_then(parse_number))
}

fn parse_number(value: &str) -> Option<f64> {
    value.trim().parse::<f64>().ok()
}

fn parse_reset_value(value: &Value) -> Option<chrono::DateTime<chrono::Utc>> {
    value
        .as_i64()
        .and_then(epoch_to_datetime)
        .or_else(|| value.as_str().and_then(parse_reset_string))
}

fn parse_reset_string(value: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Ok(epoch) = trimmed.parse::<i64>() {
        return epoch_to_datetime(epoch);
    }
    chrono::DateTime::parse_from_rfc3339(trimmed)
        .ok()
        .map(|value| value.with_timezone(&chrono::Utc))
}

fn epoch_to_datetime(epoch: i64) -> Option<chrono::DateTime<chrono::Utc>> {
    let seconds = if epoch > 1_000_000_000_000 {
        epoch / 1000
    } else {
        epoch
    };
    chrono::DateTime::<chrono::Utc>::from_timestamp(seconds, 0)
}

fn format_reset_label(reset_at: &chrono::DateTime<chrono::Utc>) -> String {
    let now = chrono::Utc::now();
    if *reset_at <= now {
        return "reset now".to_string();
    }
    let seconds = (*reset_at - now).num_seconds().max(0) as u64;
    let d = Duration::from_secs(seconds);
    let days = d.as_secs() / 86_400;
    let hours = (d.as_secs() % 86_400) / 3_600;
    let mins = (d.as_secs() % 3_600) / 60;
    if days > 0 {
        format!("resets in {}d {}h", days, hours)
    } else if hours > 0 {
        format!("resets in {}h {}m", hours, mins)
    } else {
        format!("resets in {}m", mins.max(1))
    }
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

fn has_any_quota(summary: &QuotaSummary) -> bool {
    summary.current_window.is_some()
        || summary.weekly.is_some()
        || !summary.additional_rate_limits.is_empty()
        || !summary.limits.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monetary_credits_preserve_minor_units_and_do_not_invent_balance_from_cap() {
        let raw = json!({"spend":{"enabled":false,"balance":null,"limit":{"amount_minor":10000,"currency":"USD","exponent":2},"used":{"amount_minor":125,"currency":"USD","exponent":2}}});
        let credits = usage_credits(&raw).unwrap();
        assert_eq!(credits["used"], "1.25");
        assert_eq!(credits["monthly_limit"], "100.00");
        assert!(credits["balance"].is_null());
        assert_eq!(credits["enabled"], false);
        assert_eq!(
            money(Some(
                &json!({"amount_minor":0,"currency":"JPY","exponent":0})
            )),
            Some(("0".into(), "JPY".into()))
        );
        assert!(money(Some(
            &json!({"amount_minor":1,"currency":"USD","exponent":99})
        ))
        .is_none());
    }

    #[test]
    fn oauth_usage_bucket_keeps_percent_scale() {
        let value = json!({
            "utilization": 42.5,
            "resets_at": 1_700_000_000
        });
        let bucket = parse_oauth_bucket(Some(&value)).unwrap();
        assert_eq!(bucket.used_percent, Some(42.5));
        assert_eq!(bucket.remaining_percent, Some(57.5));
        assert_eq!(
            bucket.reset_at.as_deref(),
            Some("2023-11-14T22:13:20+00:00")
        );
    }

    #[test]
    fn header_usage_bucket_converts_fraction_to_percent() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "anthropic-ratelimit-unified-5h-utilization",
            "0.75".parse().unwrap(),
        );
        let bucket = parse_header_bucket(
            &headers,
            &["anthropic-ratelimit-unified-5h-utilization"],
            &[],
        )
        .unwrap();
        assert_eq!(bucket.used_percent, Some(75.0));
    }

    #[test]
    fn standard_limit_computes_usage_from_remaining() {
        let mut headers = HeaderMap::new();
        headers.insert("anthropic-ratelimit-tokens-limit", "1000".parse().unwrap());
        headers.insert(
            "anthropic-ratelimit-tokens-remaining",
            "250".parse().unwrap(),
        );
        let limit = parse_standard_limit(&headers, "tokens").unwrap();
        assert_eq!(limit.used, Some(750.0));
        assert_eq!(limit.used_percent, Some(75.0));
    }

    #[test]
    fn oauth_usage_limits_include_scoped_model_display_name() {
        let limits = json!([
            {
                "group": "weekly",
                "is_active": false,
                "kind": "weekly_scoped",
                "percent": 12.5,
                "resets_at": 1_700_000_000,
                "scope": {
                    "model": {
                        "display_name": "Fable",
                        "id": null
                    },
                    "surface": null
                },
                "severity": "normal"
            }
        ]);

        let parsed = usage_limit_summaries(Some(&limits));
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].label, "Fable Weekly");
        assert_eq!(parsed[0].scope, "weekly_scoped");
        assert_eq!(parsed[0].used_percent, Some(12.5));
        assert_eq!(parsed[0].remaining_percent, Some(87.5));
        assert_eq!(parsed[0].used_text, "12.5%");
        assert_eq!(parsed[0].limit_text, "100%");
        assert_eq!(
            parsed[0].reset_label,
            format_reset_label(
                &chrono::DateTime::<chrono::Utc>::from_timestamp(1_700_000_000, 0).unwrap()
            )
        );
    }

    #[test]
    fn oauth_usage_maps_omelette_bucket_to_fable() {
        let account = super::super::accounts::ClaudeAccount {
            label: "claude".to_string(),
            email: None,
            scopes: Vec::new(),
            organization_uuid: String::new(),
            account_id: String::new(),
            file_name: None,
            access_token: String::new(),
            refresh_token: None,
            token_type: String::new(),
            expires_at: None,
            api_base_url: None,
            enabled: true,
            models: Vec::new(),
        };
        let summary = summary_from_usage(
            &account,
            json!({
                "seven_day_omelette": {
                    "utilization": 10.0,
                    "resets_at": 1_700_000_000
                }
            }),
        );

        assert_eq!(summary.additional_rate_limits.len(), 1);
        assert_eq!(summary.additional_rate_limits[0].display_name, "Fable");
        assert_eq!(
            summary.additional_rate_limits[0]
                .weekly
                .as_ref()
                .and_then(|bucket| bucket.used_percent),
            Some(10.0)
        );
    }
}
