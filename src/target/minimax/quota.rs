//! MiniMax usage/quota fetcher.
//!
//! The MiniMax "coding plan" quota API returns per-model usage windows:
//! a 5-hour rolling window and a weekly window, each with percent
//! remaining and a time-to-reset. The dashboard reads this so the
//! admin can see the same numbers that appear on
//! <https://platform.minimax.io/console/usage>.
//!
//! Endpoints we try, in order:
//! - `{base}/v1/api/openplatform/coding_plan/remains`  (most common)
//! - `https://api.minimaxi.chat/v1/api/openplatform/coding_plan/remains`
//!   (fallback to the official platform host if the account's
//!   `base_url` is on a different domain like `api.minimax.io`).
//!
//! Auth: the same `api_key` that the account uses for chat, sent as a
//! Bearer token.

use serde::Serialize;
use serde_json::{json, Value};
use std::time::Duration;

const REQUEST_TIMEOUT_SECS: u64 = 20;

const PLATFORM_HOST: &str = "https://api.minimaxi.chat";

#[derive(Clone, Debug)]
pub struct QuotaCacheEntry {
    pub fetched_at: std::time::Instant,
    pub summary: QuotaSummary,
    pub error: Option<String>,
}

#[derive(Default, Clone, Debug, Serialize)]
pub struct QuotaSummary {
    pub label: String,
    pub file_name: String,
    pub is_available: bool,
    /// Free-form status message (e.g. MiniMax "base_resp.status_msg").
    pub status_msg: String,
    /// 5-hour rolling window quota summary (shared across the model group).
    pub current_window: Option<WindowSummary>,
    /// Weekly quota summary (shared across the model group).
    pub weekly: Option<WindowSummary>,
    /// Per-model breakdown as MiniMax reports it.
    pub models: Vec<ModelQuota>,
    /// Live model catalog from /v1/models.
    pub available_models: Vec<ModelInfo>,
    /// The raw response, useful for debugging.
    pub raw: Value,
}

#[derive(Default, Clone, Debug, Serialize)]
pub struct WindowSummary {
    pub total_count: i64,
    pub usage_count: i64,
    pub remaining_percent: Option<f64>,
    /// Used percent = 100 - remaining_percent. Exposed for the
    /// dashboard so the renderer does not have to know about
    /// MiniMax's "remaining" convention.
    pub used_percent: Option<f64>,
    pub start_time: i64,
    pub end_time: i64,
    pub remains_time: i64,
    pub status: Option<i64>,
    /// `remains_time` formatted as a human label like "5h 12m" or "3d".
    pub reset_label: String,
}

#[derive(Default, Clone, Debug, Serialize)]
pub struct ModelQuota {
    pub model_name: String,
    pub current_window: Option<WindowSummary>,
    pub weekly: Option<WindowSummary>,
}

#[derive(Default, Clone, Debug, Serialize)]
pub struct ModelInfo {
    pub model_id: String,
    pub display_name: String,
    pub owned_by: String,
}

pub async fn get_quota_summaries(state: &crate::AppState) -> Vec<Value> {
    let accounts = state.minimax_accounts.lock().unwrap().clone();
    let now = std::time::Instant::now();
    let mut results = Vec::with_capacity(accounts.len());

    for account in &accounts {
        let key = cache_key(account);
        let cached = {
            let cache = state.minimax_quota_cache.lock().unwrap();
            cache.get(&key).cloned()
        };

        let entry = if let Some(cached) = cached {
            if crate::quota_cache_entry_is_fresh(now, cached.fetched_at, &cached.summary) {
                cached
            } else {
                let fetched = fetch_account_quota(&state.client, account).await;
                let mut cache = state.minimax_quota_cache.lock().unwrap();
                cache.insert(key.clone(), fetched.clone());
                fetched
            }
        } else {
            let fetched = fetch_account_quota(&state.client, account).await;
            let mut cache = state.minimax_quota_cache.lock().unwrap();
            cache.insert(key.clone(), fetched.clone());
            fetched
        };

        if let Some(err) = entry.error {
            results.push(json!({
                "label": account.label,
                "file_name": account.file_name.clone().unwrap_or_default(),
                "error": err
            }));
        } else {
            // Adapt the rich WindowSummary to the dashboard's
            // {used_percent, reset_label, ...} shape so renderQuotaBars
            // can consume it without special cases.
            let adapt = |w: &Option<WindowSummary>| -> Option<Value> {
                w.as_ref().map(|s| {
                    json!({
                        "used_percent": s.used_percent,
                        "remaining_percent": s.remaining_percent,
                        "reset_label": s.reset_label,
                        "remains_time": s.remains_time,
                        "total_count": s.total_count,
                        "usage_count": s.usage_count,
                        "start_time": s.start_time,
                        "end_time": s.end_time,
                        "status": s.status,
                    })
                })
            };
            let adapted_models: Vec<Value> = entry
                .summary
                .models
                .iter()
                .map(|m| {
                    json!({
                        "model_id": m.model_name,
                        "display_name": m.model_name,
                        "current": adapt(&m.current_window),
                        "weekly": adapt(&m.weekly),
                    })
                })
                .collect();
            results.push(json!({
                "label": entry.summary.label,
                "file_name": account.file_name.clone().unwrap_or_default(),
                "is_available": entry.summary.is_available,
                "status_msg": entry.summary.status_msg,
                "current_window": adapt(&entry.summary.current_window),
                "weekly": adapt(&entry.summary.weekly),
                "available_models": entry.summary.available_models,
                "models": adapted_models,
            }));
        }
    }

    results
}

fn cache_key(account: &super::accounts::MiniMaxAccount) -> String {
    account
        .file_name
        .clone()
        .unwrap_or_else(|| account.label.clone())
}

async fn fetch_account_quota(
    client: &reqwest::Client,
    account: &super::accounts::MiniMaxAccount,
) -> QuotaCacheEntry {
    let mut summary = QuotaSummary {
        label: account.label.clone(),
        file_name: account.file_name.clone().unwrap_or_default(),
        ..Default::default()
    };
    match fetch_quota(client, account).await {
        Ok((is_available, status_msg, models)) => {
            // Aggregate the per-model numbers into shared 5h and weekly
            // windows. The platform displays per-model numbers but the
            // most prominent "usage" bar uses the 5h percent-remaining
            // for the first non-empty model. To stay simple, we report
            // the FIRST model in the response as the headline numbers
            // and keep the rest under `models`.
            summary.is_available = is_available;
            summary.status_msg = status_msg;
            if let Some(first) = models.first() {
                summary.current_window = first.current_window.clone();
                summary.weekly = first.weekly.clone();
            }
            summary.models = models;
            summary.available_models = fetch_available_models(client, account)
                .await
                .unwrap_or_default();
            QuotaCacheEntry {
                fetched_at: std::time::Instant::now(),
                summary,
                error: None,
            }
        }
        Err(err) => QuotaCacheEntry {
            fetched_at: std::time::Instant::now(),
            summary,
            error: Some(err),
        },
    }
}

async fn fetch_available_models(
    client: &reqwest::Client,
    account: &super::accounts::MiniMaxAccount,
) -> Result<Vec<ModelInfo>, String> {
    let base = super::api::normalize_base_url(account.base_url.as_deref());
    let url = if base.ends_with("/models") {
        base
    } else if base.ends_with("/v1") {
        format!("{}/models", base)
    } else {
        format!("{}/v1/models", base)
    };
    let resp = client
        .get(&url)
        .header(
            "Authorization",
            format!("Bearer {}", account.api_key.trim()),
        )
        .header("Accept", "application/json")
        .timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS))
        .send()
        .await
        .map_err(|e| format!("MiniMax models request to {} failed: {}", url, e))?;
    let status = resp.status();
    let text = resp
        .text()
        .await
        .map_err(|e| format!("MiniMax models body read failed: {}", e))?;
    if !status.is_success() {
        return Err(format!(
            "MiniMax models at {} returned {}: {}",
            url, status, text
        ));
    }
    let value: Value = serde_json::from_str(&text)
        .map_err(|e| format!("MiniMax models JSON parse failed: {}", e))?;
    Ok(parse_models_response(&value))
}

fn parse_models_response(value: &Value) -> Vec<ModelInfo> {
    value
        .get("data")
        .and_then(|v| v.as_array())
        .or_else(|| value.as_array())
        .into_iter()
        .flatten()
        .filter_map(|item| {
            let model_id = item
                .get("id")
                .and_then(|v| v.as_str())
                .or_else(|| item.get("model").and_then(|v| v.as_str()))?
                .trim();
            if model_id.is_empty() {
                return None;
            }
            Some(ModelInfo {
                model_id: model_id.to_string(),
                display_name: item
                    .get("display_name")
                    .and_then(|v| v.as_str())
                    .unwrap_or(model_id)
                    .to_string(),
                owned_by: item
                    .get("owned_by")
                    .and_then(|v| v.as_str())
                    .unwrap_or("minimax")
                    .to_string(),
            })
        })
        .collect()
}

async fn fetch_quota(
    client: &reqwest::Client,
    account: &super::accounts::MiniMaxAccount,
) -> Result<(bool, String, Vec<ModelQuota>), String> {
    let base = super::api::normalize_base_url(account.base_url.as_deref());
    let candidates = quota_url_candidates(&base);

    let mut last_err: Option<String> = None;
    for url in candidates {
        match fetch_quota_from_url(client, account, &url).await {
            Ok(result) => return Ok(result),
            Err(err) => last_err = Some(err),
        }
    }
    Err(last_err.unwrap_or_else(|| "no MiniMax quota URL responded".to_string()))
}

/// Build the candidate list of quota API URLs we should try, in order:
/// 1. The account's configured base_url (most likely correct).
/// 2. The platform's official host `api.minimaxi.chat` (covers accounts
///    that point their chat traffic at a different domain like
///    `api.minimax.io` but still have a MiniMax platform account).
fn quota_url_candidates(base: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let trimmed = base.trim_end_matches('/');
    // Strip a trailing /v1 or /chat/completions if present.
    let chat_root = trimmed
        .trim_end_matches("/v1/chat/completions")
        .trim_end_matches("/chat/completions")
        .trim_end_matches("/v1");
    let path = "/v1/api/openplatform/coding_plan/remains";
    out.push(format!("{}{}", chat_root, path));
    if !trimmed.starts_with(PLATFORM_HOST) {
        out.push(format!("{}{}", PLATFORM_HOST, path));
    }
    out
}

async fn fetch_quota_from_url(
    client: &reqwest::Client,
    account: &super::accounts::MiniMaxAccount,
    url: &str,
) -> Result<(bool, String, Vec<ModelQuota>), String> {
    let resp = client
        .get(url)
        .header(
            "Authorization",
            format!("Bearer {}", account.api_key.trim()),
        )
        .header("Accept", "application/json")
        .timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS))
        .send()
        .await
        .map_err(|e| format!("MiniMax quota request to {} failed: {}", url, e))?;
    let status = resp.status();
    let text = resp
        .text()
        .await
        .map_err(|e| format!("MiniMax quota body read failed: {}", e))?;
    if !status.is_success() {
        return Err(format!(
            "MiniMax quota at {} returned {}: {}",
            url, status, text
        ));
    }
    let value: Value = serde_json::from_str(&text)
        .map_err(|e| format!("MiniMax quota JSON parse failed: {}", e))?;
    parse_quota_response(&value)
}

fn parse_quota_response(value: &Value) -> Result<(bool, String, Vec<ModelQuota>), String> {
    // The base_resp block carries the platform status; treat anything
    // other than status_code == 0 as an error.
    let base_resp = value.get("base_resp");
    let status_code = base_resp
        .and_then(|b| b.get("status_code"))
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let status_msg = base_resp
        .and_then(|b| b.get("status_msg"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if status_code != 0 {
        return Err(format!(
            "MiniMax quota API returned status_code={} ({})",
            status_code, status_msg
        ));
    }

    let is_available = value
        .get("is_available")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);

    let mut models: Vec<ModelQuota> = Vec::new();
    if let Some(arr) = value.get("model_remains").and_then(|v| v.as_array()) {
        for item in arr {
            let model_name = item
                .get("model_name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            models.push(ModelQuota {
                model_name: model_name.clone(),
                current_window: parse_window(item, "current_interval_"),
                weekly: parse_window(item, "current_weekly_"),
            });
            let _ = model_name;
        }
    }

    Ok((is_available, status_msg, models))
}

fn parse_window(item: &Value, prefix: &str) -> Option<WindowSummary> {
    let get_i64 = |suffix: &str| -> i64 { get_i64_key(item, &format!("{}{}", prefix, suffix)) };
    let get_opt_f64 = |suffix: &str| -> Option<f64> {
        item.get(format!("{}{}", prefix, suffix))
            .and_then(|v| v.as_f64())
    };
    let total = get_i64("total_count");
    let usage = get_i64("usage_count");
    // A window with no quota is not interesting.
    if total == 0 && usage == 0 && get_opt_f64("remaining_percent").is_none() {
        return None;
    }
    let remaining = get_opt_f64("remaining_percent");
    let used = remaining.map(|r| (100.0 - r).max(0.0).min(100.0));
    let (start_time_raw, end_time_raw, remains_time_raw) = match prefix {
        "current_interval_" => (
            get_i64_first(item, &["current_interval_start_time", "start_time"]),
            get_i64_first(item, &["current_interval_end_time", "end_time"]),
            get_i64_first(item, &["current_interval_remains_time", "remains_time"]),
        ),
        "current_weekly_" => (
            get_i64_first(item, &["current_weekly_start_time", "weekly_start_time"]),
            get_i64_first(item, &["current_weekly_end_time", "weekly_end_time"]),
            get_i64_first(
                item,
                &["current_weekly_remains_time", "weekly_remains_time"],
            ),
        ),
        _ => (
            get_i64("start_time"),
            get_i64("end_time"),
            get_i64("remains_time"),
        ),
    };
    let start_time = normalize_epoch_seconds(start_time_raw);
    let end_time = normalize_epoch_seconds(end_time_raw);
    let reset_from_end_time = reset_seconds_from_end_time(end_time);
    let remains_time = normalize_duration_seconds(remains_time_raw, reset_from_end_time)
        .or(reset_from_end_time)
        .unwrap_or(0);
    Some(WindowSummary {
        total_count: total,
        usage_count: usage,
        remaining_percent: remaining,
        used_percent: used,
        start_time,
        end_time,
        remains_time,
        status: item
            .get(format!("{}status", prefix))
            .and_then(|v| v.as_i64()),
        reset_label: humanize_seconds(remains_time),
    })
}

fn get_i64_key(item: &Value, key: &str) -> i64 {
    item.get(key).and_then(|v| v.as_i64()).unwrap_or(0)
}

fn get_i64_first(item: &Value, keys: &[&str]) -> i64 {
    keys.iter()
        .find_map(|key| item.get(*key).and_then(|v| v.as_i64()))
        .unwrap_or(0)
}

fn normalize_epoch_seconds(value: i64) -> i64 {
    if value > 100_000_000_000 {
        value / 1000
    } else {
        value
    }
}

fn normalize_duration_seconds(value: i64, reset_from_end_time: Option<i64>) -> Option<i64> {
    if value <= 0 {
        return None;
    }
    let as_seconds = value;
    let as_milliseconds = ((value + 999) / 1000).max(1);
    if let Some(reference) = reset_from_end_time {
        if absolute_diff(as_milliseconds, reference) < absolute_diff(as_seconds, reference) {
            return Some(as_milliseconds);
        }
        return Some(as_seconds);
    }
    // MiniMax's live coding-plan endpoint currently returns remains_time
    // in milliseconds, while older examples used seconds.
    if value > 31 * 86_400 {
        Some(as_milliseconds)
    } else {
        Some(as_seconds)
    }
}

fn reset_seconds_from_end_time(end_time: i64) -> Option<i64> {
    if end_time <= 0 {
        return None;
    }
    let remaining = end_time.saturating_sub(chrono::Utc::now().timestamp());
    (remaining > 0).then_some(remaining)
}

fn absolute_diff(left: i64, right: i64) -> i64 {
    if left >= right {
        left - right
    } else {
        right - left
    }
}

fn humanize_seconds(secs: i64) -> String {
    if secs <= 0 {
        return String::new();
    }
    let days = secs / 86400;
    let hours = (secs % 86400) / 3600;
    let minutes = (secs % 3600) / 60;
    if days > 0 {
        format!("{}d {}h", days, hours)
    } else if hours > 0 {
        format!("{}h {}m", hours, minutes)
    } else if minutes > 0 {
        format!("{}m", minutes)
    } else {
        format!("{}s", secs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_two_model_response() {
        let raw = json!({
            "model_remains": [
                {
                    "model_name": "general",
                    "current_interval_total_count": 100,
                    "current_interval_usage_count": 25,
                    "current_interval_status": 1,
                    "current_interval_remaining_percent": 75.0,
                    "current_interval_start_time": 1700000000,
                    "current_interval_end_time": 1700018000,
                    "current_interval_remains_time": 1500,
                    "current_weekly_total_count": 1000,
                    "current_weekly_usage_count": 100,
                    "current_weekly_status": 1,
                    "current_weekly_remaining_percent": 90.0,
                    "current_weekly_start_time": 1699000000,
                    "current_weekly_end_time": 1699600000,
                    "current_weekly_remains_time": 500000
                },
                {
                    "model_name": "video",
                    "current_interval_total_count": 0,
                    "current_interval_usage_count": 0,
                    "current_interval_status": 3,
                    "current_interval_remaining_percent": 100.0,
                    "current_weekly_total_count": 0,
                    "current_weekly_usage_count": 0,
                    "current_weekly_status": 3,
                    "current_weekly_remaining_percent": 100.0
                }
            ],
            "base_resp": { "status_code": 0, "status_msg": "success" }
        });
        let (is_available, status_msg, models) = parse_quota_response(&raw).unwrap();
        assert!(is_available);
        assert_eq!(status_msg, "success");
        assert_eq!(models.len(), 2);
        let general = &models[0];
        assert_eq!(general.model_name, "general");
        let cur = general.current_window.as_ref().unwrap();
        assert_eq!(cur.total_count, 100);
        assert_eq!(cur.usage_count, 25);
        assert_eq!(cur.remaining_percent, Some(75.0));
        let wk = general.weekly.as_ref().unwrap();
        assert_eq!(wk.total_count, 1000);
        assert_eq!(wk.usage_count, 100);
        assert_eq!(wk.remaining_percent, Some(90.0));
        // video has zero counts but a remaining_percent set, so the
        // window is reported (with zero usage). The parse_window helper
        // only drops windows when BOTH counters and remaining_percent
        // are missing.
        let video = &models[1];
        let v_cur = video.current_window.as_ref().unwrap();
        assert_eq!(v_cur.total_count, 0);
        assert_eq!(v_cur.usage_count, 0);
        assert_eq!(v_cur.remaining_percent, Some(100.0));
        assert_eq!(v_cur.status, Some(3));
    }

    #[test]
    fn returns_error_on_non_zero_status() {
        let raw = json!({
            "base_resp": { "status_code": 1004, "status_msg": "invalid api key" }
        });
        let err = parse_quota_response(&raw).unwrap_err();
        assert!(
            err.contains("1004"),
            "expected status code in error, got: {}",
            err
        );
    }

    #[test]
    fn missing_model_remains_returns_empty_list() {
        let raw = json!({
            "base_resp": { "status_code": 0, "status_msg": "success" }
        });
        let (_, _, models) = parse_quota_response(&raw).unwrap();
        assert!(models.is_empty());
    }

    #[test]
    fn parses_live_minimax_reset_aliases_and_millisecond_durations() {
        let raw = json!({
            "model_remains": [
                {
                    "model_name": "general",
                    "current_interval_total_count": 100,
                    "current_interval_usage_count": 55,
                    "current_interval_status": 1,
                    "current_interval_remaining_percent": 45.0,
                    "start_time": 1782622800000i64,
                    "end_time": 1782640800000i64,
                    "remains_time": 15032113i64,
                    "current_weekly_total_count": 1000,
                    "current_weekly_usage_count": 260,
                    "current_weekly_status": 1,
                    "current_weekly_remaining_percent": 74.0,
                    "weekly_start_time": 1782086400000i64,
                    "weekly_end_time": 1782691200000i64,
                    "weekly_remains_time": 65432113i64
                }
            ],
            "base_resp": { "status_code": 0, "status_msg": "success" }
        });

        let (_, _, models) = parse_quota_response(&raw).unwrap();
        let general = &models[0];
        let current = general.current_window.as_ref().unwrap();
        assert_eq!(current.start_time, 1782622800);
        assert_eq!(current.end_time, 1782640800);
        assert_eq!(current.remains_time, 15033);
        assert_eq!(current.reset_label, "4h 10m");

        let weekly = general.weekly.as_ref().unwrap();
        assert_eq!(weekly.start_time, 1782086400);
        assert_eq!(weekly.end_time, 1782691200);
        assert_eq!(weekly.remains_time, 65433);
        assert_eq!(weekly.reset_label, "18h 10m");
    }

    #[test]
    fn falls_back_to_end_time_when_remains_time_is_missing() {
        let end_time = chrono::Utc::now().timestamp() + 90 * 60;
        let item = json!({
            "model_name": "general",
            "current_interval_total_count": 100,
            "current_interval_usage_count": 20,
            "current_interval_remaining_percent": 80.0,
            "current_interval_end_time": end_time
        });

        let current = parse_window(&item, "current_interval_").unwrap();
        assert!(current.remains_time >= (89 * 60));
        assert!(current.remains_time <= (90 * 60));
        assert!(current.reset_label.starts_with("1h "));
    }

    #[test]
    fn uses_end_time_to_disambiguate_small_millisecond_durations() {
        let end_time = (chrono::Utc::now().timestamp() + 5 * 60) * 1000;
        let item = json!({
            "model_name": "general",
            "current_interval_total_count": 100,
            "current_interval_usage_count": 20,
            "current_interval_remaining_percent": 80.0,
            "end_time": end_time,
            "remains_time": 300000
        });

        let current = parse_window(&item, "current_interval_").unwrap();
        assert!(current.remains_time >= 299);
        assert!(current.remains_time <= 300);
        assert_eq!(current.reset_label, "5m");
    }

    #[test]
    fn quota_url_candidates_includes_account_base() {
        let urls = quota_url_candidates("https://api.minimax.io/v1");
        assert!(urls[0].contains("api.minimax.io"));
        assert!(urls.iter().any(|u| u.starts_with(PLATFORM_HOST)));
    }

    #[test]
    fn quota_url_candidates_skips_duplicate_platform_host() {
        let urls = quota_url_candidates(PLATFORM_HOST);
        // When the configured base is already the platform host, the
        // fallback candidate is omitted to avoid duplicate requests.
        assert_eq!(urls.len(), 1);
    }

    #[test]
    fn quota_url_candidates_strips_chat_completions() {
        let urls = quota_url_candidates("https://api.minimaxi.chat/v1/chat/completions");
        assert_eq!(
            urls[0],
            format!("{}/v1/api/openplatform/coding_plan/remains", PLATFORM_HOST)
        );
    }

    #[test]
    fn humanize_seconds_formats_hours_and_days() {
        assert_eq!(humanize_seconds(0), "");
        assert_eq!(humanize_seconds(45), "45s");
        assert_eq!(humanize_seconds(60), "1m");
        assert_eq!(humanize_seconds(90 * 60), "1h 30m");
        assert_eq!(humanize_seconds(2 * 86400 + 3 * 3600 + 15 * 60), "2d 3h");
    }
}
