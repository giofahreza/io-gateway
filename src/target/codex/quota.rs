//! Codex quota and reset-credit transport.
//!
//! The gateway owns a direct HTTP Wham adapter for its configured Codex
//! upstream. This is intentionally separate from Codex App Server's public
//! JSON-RPC account API: do not mix the two request shapes or silently fall
//! back to a different host when the configured adapter is unsupported.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::time::Duration;

const CHATGPT_BACKEND_API_BASE: &str = "https://chatgpt.com/backend-api";
const CODEX_USER_AGENT: &str = "codex_cli_rs/0.76.0 (Debian 13.0.0; x86_64) WindowsTerminal";

#[derive(Clone)]
pub struct QuotaCacheEntry {
    pub fetched_at: std::time::Instant,
    pub summary: QuotaSummary,
    pub error: Option<String>,
}

#[derive(Default, Clone, Serialize)]
pub struct QuotaSummary {
    pub label: String,
    pub account_id: String,
    pub plan_type: String,
    pub code_generation: QuotaRateSummary,
    pub code_review: QuotaRateSummary,
    pub additional_rate_limits: Vec<AdditionalRateLimitSummary>,
    pub rate_limit_reset_credits: Option<RateLimitResetCreditsSummary>,
    pub models: Vec<ModelSummary>,
}

#[derive(Default, Clone, Serialize)]
pub struct QuotaRateSummary {
    pub five_hour: Option<QuotaWindowSummary>,
    pub weekly: Option<QuotaWindowSummary>,
}

#[derive(Default, Clone, Serialize)]
pub struct QuotaWindowSummary {
    pub used_percent: Option<f64>,
    pub reset_label: String,
}

#[derive(Default, Clone, Serialize)]
pub struct AdditionalRateLimitSummary {
    pub display_name: String,
    pub five_hour: Option<QuotaWindowSummary>,
    pub weekly: Option<QuotaWindowSummary>,
}

#[derive(Default, Clone, Serialize)]
pub struct ModelSummary {
    pub model_id: String,
    pub display_name: String,
}

#[derive(Default, Clone, Serialize, Deserialize)]
pub struct RateLimitResetCreditsSummary {
    pub available_count: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credits: Option<Vec<RateLimitResetCredit>>,
}

#[derive(Default, Clone, Debug, Serialize, Deserialize)]
pub struct RateLimitResetCredit {
    #[serde(alias = "creditId")]
    pub id: String,
    #[serde(alias = "resetType")]
    pub reset_type: String,
    pub status: String,
    #[serde(alias = "grantedAt", deserialize_with = "deserialize_string_or_number")]
    pub granted_at: String,
    #[serde(
        default,
        alias = "expiresAt",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional_string_or_number"
    )]
    pub expires_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Deserialize)]
struct RateLimitResetCreditsDetails {
    #[serde(default)]
    credits: Option<Vec<RateLimitResetCredit>>,
    #[serde(alias = "availableCount")]
    available_count: i64,
}

#[derive(Default, Deserialize)]
pub struct ConsumeRateLimitResetForm {
    pub file_name: Option<String>,
    pub label: Option<String>,
    pub account_id: Option<String>,
    pub credit_id: Option<String>,
}

/// Fresh, uncached information used by the automatic reset-credit worker.
///
/// This intentionally contains only rate-limit state and opaque credit metadata.  It
/// is separate from the dashboard quota cache because an exhausted dashboard
/// snapshot can remain cached for an hour and must never authorize a credit
/// redemption.
#[derive(Clone, Debug)]
pub(crate) struct FreshRateLimitResetState {
    /// `None` means the upstream reported only a count, not safe selectable
    /// credit IDs.  Automation must fail closed in that case.
    pub credits: Option<Vec<RateLimitResetCredit>>,
    /// The documented authoritative count.  A detailed row is never enough
    /// to spend a credit when the upstream says there are none available.
    pub available_credit_count: u64,
    /// True only when the authoritative `codex` bucket explicitly reports
    /// `rate_limit_reached`. Workspace credit/spend states are not eligible
    /// for a reset credit and therefore remain false.
    pub rate_limit_reached: bool,
    /// True only when the authoritative `codex` bucket explicitly reports no
    /// reached state. A different reached state after consumption is not
    /// enough evidence to verify a reset credit was applied.
    pub rate_limit_cleared: bool,
    /// The shortest remaining natural reset among reached windows, when the
    /// upstream exposes it.  A caller may use this to avoid spending a credit
    /// just before the ordinary window would recover.
    pub natural_reset_after_seconds: Option<u64>,
}

#[derive(Clone, Debug)]
pub(crate) struct ConsumeRateLimitResetResult {
    pub outcome: String,
}

pub async fn get_quota_summaries(state: &crate::AppState) -> Vec<serde_json::Value> {
    let tokens = state.tokens.lock().unwrap().clone();
    {
        let mut cache = state.quota_cache.lock().unwrap();
        if cache.len() != tokens.len() {
            *cache = vec![None; tokens.len()];
        }
    }
    let now = std::time::Instant::now();
    let mut results = Vec::with_capacity(tokens.len());
    for (idx, token) in tokens.iter().enumerate() {
        let cached = {
            let cache = state.quota_cache.lock().unwrap();
            cache.get(idx).cloned().flatten()
        };
        let entry = if let Some(c) = cached {
            if crate::quota_cache_entry_is_fresh(now, c.fetched_at, &c.summary) {
                c
            } else {
                let fetched = fetch_codex_quota(state, token).await;
                let mut cache = state.quota_cache.lock().unwrap();
                if cache.len() <= idx {
                    cache.resize(idx + 1, None);
                }
                cache[idx] = Some(fetched.clone());
                fetched
            }
        } else {
            let fetched = fetch_codex_quota(state, token).await;
            let mut cache = state.quota_cache.lock().unwrap();
            if cache.len() <= idx {
                cache.resize(idx + 1, None);
            }
            cache[idx] = Some(fetched.clone());
            fetched
        };
        if let Some(err) = entry.error {
            results.push(serde_json::json!({
                "label": token.label,
                "account_id": token.account_id.clone().unwrap_or_default(),
                "file_name": token.file_name.clone().unwrap_or_default(),
                "error": err
            }));
        } else {
            results.push(serde_json::json!({
                "label": entry.summary.label,
                "account_id": entry.summary.account_id,
                "file_name": token.file_name.clone().unwrap_or_default(),
                "plan_type": entry.summary.plan_type,
                "code_generation": entry.summary.code_generation,
                "code_review": entry.summary.code_review,
                "additional_rate_limits": entry.summary.additional_rate_limits,
                "rate_limit_reset_credits": entry.summary.rate_limit_reset_credits,
                "models": entry.summary.models
            }));
        }
    }
    results
}

async fn fetch_codex_quota(
    state: &crate::AppState,
    token: &super::tokens::UpstreamToken,
) -> QuotaCacheEntry {
    let v = match fetch_codex_usage_value(state, token).await {
        Ok(value) => value,
        Err(error) => {
            return QuotaCacheEntry {
                fetched_at: std::time::Instant::now(),
                summary: QuotaSummary::default(),
                error: Some(error),
            }
        }
    };

    let plan_type = v
        .get("plan_type")
        .or_else(|| v.get("planType"))
        .and_then(|x| x.as_str())
        .unwrap_or_default()
        .to_string();

    let mut code_gen = extract_rate_summary(v.get("rate_limit"));
    let mut code_review = extract_rate_summary(v.get("code_review_rate_limit"));
    let additional_rate_limits = extract_additional_rate_limits(v.get("additional_rate_limits"));

    // Fallback for alternate response shape that sends usage nodes as arrays.
    if code_gen.five_hour.is_none()
        && code_gen.weekly.is_none()
        && code_review.five_hour.is_none()
        && code_review.weekly.is_none()
    {
        let usage_nodes = v
            .get("usage")
            .and_then(|x| x.as_array())
            .cloned()
            .unwrap_or_default();
        let (fallback_gen, fallback_review) = extract_from_usage_nodes(&usage_nodes);
        code_gen = fallback_gen;
        code_review = fallback_review;
    }

    let rate_limit_reset_credits = fetch_rate_limit_reset_credits(state, token)
        .await
        .ok()
        .or_else(|| extract_rate_limit_reset_credits_summary(v.get("rate_limit_reset_credits")));

    let summary = QuotaSummary {
        label: token.label.clone(),
        account_id: token.account_id.clone().unwrap_or_default(),
        plan_type,
        code_generation: code_gen,
        code_review,
        additional_rate_limits,
        rate_limit_reset_credits,
        models: fetch_codex_models(state, token).await.unwrap_or_default(),
    };
    QuotaCacheEntry {
        fetched_at: std::time::Instant::now(),
        summary,
        error: None,
    }
}

async fn fetch_codex_usage_value(
    state: &crate::AppState,
    token: &super::tokens::UpstreamToken,
) -> Result<serde_json::Value, String> {
    let url = wham_url(&state.cfg.upstream_base, "usage")?;
    let req = authenticated_codex_request(state.client.get(url), token);
    let response = req
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .map_err(|err| format!("fetch Codex usage failed: {err}"))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|err| format!("read Codex usage response failed: {err}"))?;
    if !status.is_success() {
        return Err(format!(
            "Codex usage returned {}: {}",
            status.as_u16(),
            body
        ));
    }
    serde_json::from_str(&body).map_err(|_| "failed to parse Codex usage response".to_string())
}

/// Reads reset-credit eligibility directly from Codex without consulting or
/// populating the dashboard cache.  The automatic worker calls this both
/// before and after a redemption so a stale quota view cannot consume a
/// credit or falsely verify one.
pub(crate) async fn fresh_rate_limit_reset_state(
    state: &crate::AppState,
    account_key: &str,
) -> Result<FreshRateLimitResetState, String> {
    // Reset credits are a documented Codex App Server account operation.
    // Do not use the dashboard's legacy direct-HTTP quota cache to make a
    // spending decision: it can be stale and is not the supported contract
    // for `account/rateLimitResetCredit/consume`.
    let rate_limits = super::app_server::read_rate_limits(state.cfg.as_ref(), account_key).await?;
    let credits_value = rate_limits.get("rateLimitResetCredits").ok_or_else(|| {
        "Codex App Server did not provide reset-credit details; refusing to select a credit"
            .to_string()
    })?;
    let credits: RateLimitResetCreditsDetails = serde_json::from_value(credits_value.clone())
        .map_err(|_| {
            "Codex App Server returned invalid reset-credit details; refusing to select a credit"
                .to_string()
        })?;
    let available_credit_count = checked_available_credit_count(credits.available_count)?;
    let mut inspection = RateLimitInspection::default();
    inspect_rate_limits(&rate_limits, &mut inspection);
    if !inspection.state_known {
        return Err(
            "Codex App Server did not provide an authoritative rate-limit state; refusing reset-credit automation"
                .to_string(),
        );
    }
    Ok(FreshRateLimitResetState {
        credits: credits.credits,
        available_credit_count,
        rate_limit_reached: inspection.reached,
        rate_limit_cleared: inspection.cleared,
        natural_reset_after_seconds: inspection.natural_reset_after_seconds,
    })
}

async fn fetch_codex_models(
    state: &crate::AppState,
    token: &super::tokens::UpstreamToken,
) -> Result<Vec<ModelSummary>, String> {
    let req = authenticated_codex_request(
        state.client.get(super::gateway::build_upstream_url(
            &state.cfg.upstream_base,
            "models",
            Some("client_version=1.0.0"),
        )),
        token,
    );

    let resp = req
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .map_err(|err| err.to_string())?;
    let status = resp.status();
    let body = resp.text().await.map_err(|err| err.to_string())?;
    if !status.is_success() {
        return Err(format!("status {}: {}", status.as_u16(), body));
    }
    let value: serde_json::Value =
        serde_json::from_str(&body).map_err(|_| "failed to parse models response".to_string())?;
    Ok(parse_models_response(&value))
}

/// Sends one documented reset-credit redemption for a selected upstream
/// credential through the local Codex App Server.  The durable coordinator
/// always supplies a concrete opaque credit ID and its own idempotency key.
pub(crate) async fn consume_rate_limit_reset_credit_with_token(
    state: &crate::AppState,
    account_key: &str,
    credit_id: &str,
    idempotency_key: &str,
) -> Result<ConsumeRateLimitResetResult, String> {
    let outcome = super::app_server::consume_rate_limit_reset_credit(
        state.cfg.as_ref(),
        account_key,
        credit_id,
        idempotency_key,
    )
    .await?;
    Ok(ConsumeRateLimitResetResult {
        outcome: reset_outcome(&outcome).to_string(),
    })
}

async fn fetch_rate_limit_reset_credits(
    state: &crate::AppState,
    token: &super::tokens::UpstreamToken,
) -> Result<RateLimitResetCreditsSummary, String> {
    let url = wham_url(&state.cfg.upstream_base, "rate-limit-reset-credits")?;
    let req = authenticated_codex_request(state.client.get(&url), token);
    let details: RateLimitResetCreditsDetails = execute_json(req, "fetch reset credits").await?;
    Ok(RateLimitResetCreditsSummary {
        available_count: details.available_count,
        credits: details.credits,
    })
}

async fn execute_json<T: for<'de> Deserialize<'de>>(
    req: reqwest::RequestBuilder,
    context: &str,
) -> Result<T, String> {
    let resp = req
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .map_err(|err| format!("{} request failed: {}", context, err))?;
    let status = resp.status();
    let body = resp
        .text()
        .await
        .map_err(|err| format!("{} body read failed: {}", context, err))?;
    if !status.is_success() {
        return Err(format!(
            "{} returned {}: {}",
            context,
            status.as_u16(),
            body
        ));
    }
    serde_json::from_str(&body).map_err(|err| format!("{} JSON parse failed: {}", context, err))
}

#[derive(Default)]
struct RateLimitInspection {
    reached: bool,
    cleared: bool,
    natural_reset_after_seconds: Option<u64>,
    /// `false` means the App Server response did not contain a structurally
    /// recognizable authoritative quota state.  A missing/renamed field must
    /// never be interpreted as an un-reached limit after a credit POST.
    state_known: bool,
}

/// Reads only the documented `codex` App Server bucket. In particular,
/// workspace-credit/spend-limit states and random nested percentages must not
/// authorize spending an earned rate-limit reset.
fn inspect_rate_limits(value: &serde_json::Value, inspection: &mut RateLimitInspection) {
    // `rateLimits` is only the legacy single-bucket compatibility view.  If
    // App Server supplied the multi-bucket map, never fall back to a possibly
    // unrelated compatibility bucket merely because it does not contain the
    // Codex bucket we need.  That could spend a Codex reset credit for a
    // workspace/spend limit or a future non-Codex meter.
    let bucket = match value.get("rateLimitsByLimitId") {
        Some(serde_json::Value::Object(by_limit_id)) => by_limit_id.get("codex").filter(|bucket| {
            bucket.get("limitId").and_then(serde_json::Value::as_str) == Some("codex")
        }),
        Some(serde_json::Value::Null) | None => value.get("rateLimits").filter(|bucket| {
            bucket
                .get("limitId")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|limit_id| limit_id == "codex")
        }),
        // A malformed multi-bucket field is not a legacy response.  Refuse
        // to infer an eligible limit from a second, ambiguous field.
        Some(_) => None,
    };
    let Some(bucket) = bucket.and_then(serde_json::Value::as_object) else {
        return;
    };
    let Some(marker) = bucket.get("rateLimitReachedType") else {
        return;
    };
    inspection.state_known = true;
    match marker {
        serde_json::Value::Null => inspection.cleared = true,
        serde_json::Value::String(marker) if marker == "rate_limit_reached" => {
            inspection.reached = true;
            let mut saw_window = false;
            let mut all_present_windows_have_reset = true;
            for name in ["primary", "secondary"] {
                if let Some(window) = bucket.get(name).and_then(serde_json::Value::as_object) {
                    saw_window = true;
                    if let Some(reset_after) = reset_after_seconds(window) {
                        record_natural_reset(inspection, Some(reset_after));
                    } else {
                        // The reached marker does not identify which window
                        // caused the block. If a disclosed candidate window
                        // has no usable reset, a second window's timestamp is
                        // not proof that the natural reset is safely distant.
                        all_present_windows_have_reset = false;
                    }
                }
            }
            if !saw_window || !all_present_windows_have_reset {
                inspection.natural_reset_after_seconds = None;
            }
        }
        // All other server-classified states are known but never treated as
        // either a resettable rate limit or evidence that one was cleared.
        _ => {}
    }
}

fn record_natural_reset(inspection: &mut RateLimitInspection, after: Option<u64>) {
    if let Some(after) = after {
        inspection.natural_reset_after_seconds = Some(
            inspection
                .natural_reset_after_seconds
                .map_or(after, |current| current.min(after)),
        );
    }
}

fn json_number(value: &serde_json::Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str()?.trim().parse().ok())
}

fn reset_after_seconds(object: &serde_json::Map<String, serde_json::Value>) -> Option<u64> {
    for key in ["reset_after_seconds", "resetAfterSeconds"] {
        if let Some(value) = object.get(key).and_then(json_number) {
            if value.is_finite() && value >= 0.0 {
                return Some(value as u64);
            }
        }
    }
    for key in ["resets_at", "resetsAt", "reset_at", "resetAt"] {
        let Some(value) = object.get(key) else {
            continue;
        };
        if let Some(seconds) = value.as_i64().or_else(|| {
            value
                .as_str()
                .and_then(|value| value.trim().parse::<i64>().ok())
        }) {
            return Some(seconds.saturating_sub(Utc::now().timestamp()).max(0) as u64);
        }
        if let Some(text) = value.as_str() {
            if let Ok(timestamp) = DateTime::parse_from_rfc3339(text) {
                return Some(
                    timestamp
                        .with_timezone(&Utc)
                        .timestamp()
                        .saturating_sub(Utc::now().timestamp())
                        .max(0) as u64,
                );
            }
        }
    }
    None
}

fn deserialize_string_or_number<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserialize_optional_string_or_number(deserializer)?.ok_or_else(|| {
        serde::de::Error::custom("expected a reset-credit timestamp string or number")
    })
}

fn deserialize_optional_string_or_number<'de, D>(
    deserializer: D,
) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    match value {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::String(value)) => Ok(Some(value)),
        Some(serde_json::Value::Number(value)) => Ok(Some(value.to_string())),
        _ => Err(serde::de::Error::custom(
            "expected a reset-credit timestamp string or number",
        )),
    }
}

pub(crate) fn select_token_for_reset(
    state: &crate::AppState,
    form: &ConsumeRateLimitResetForm,
) -> Result<super::tokens::UpstreamToken, String> {
    let tokens = state.tokens.lock().unwrap().clone();
    let file_name = form
        .file_name
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let account_id = form
        .account_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let label = form
        .label
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());

    let mut matches = tokens.into_iter().filter(|token| {
        if let Some(file_name) = file_name {
            return token.file_name.as_deref() == Some(file_name);
        }
        if let Some(account_id) = account_id {
            return token.account_id.as_deref() == Some(account_id);
        }
        if let Some(label) = label {
            return token.label == label;
        }
        false
    });

    let token = matches
        .next()
        .ok_or_else(|| "matching Codex account was not found".to_string())?;
    if matches.next().is_some() {
        return Err("multiple Codex accounts matched; include file_name or account_id".to_string());
    }
    Ok(token)
}

fn authenticated_codex_request(
    req: reqwest::RequestBuilder,
    token: &super::tokens::UpstreamToken,
) -> reqwest::RequestBuilder {
    let mut req = req
        .header("Authorization", format!("Bearer {}", token.token))
        .header("Content-Type", "application/json")
        .header("User-Agent", CODEX_USER_AGENT);
    if let Some(account_id) = token.account_id.as_ref() {
        if !account_id.trim().is_empty() {
            req = req.header("Chatgpt-Account-Id", account_id);
        }
    }
    req
}

fn wham_url(upstream_base: &str, path: &str) -> Result<String, String> {
    let base = chatgpt_backend_base(upstream_base)?;
    Ok(format!("{}/wham/{}", base, path.trim_start_matches('/')))
}

fn chatgpt_backend_base(upstream_base: &str) -> Result<String, String> {
    let trimmed = upstream_base.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return Ok(CHATGPT_BACKEND_API_BASE.to_string());
    }
    if let Some(base) = trimmed.strip_suffix("/codex") {
        // `upstream_base` names the ordinary Codex HTTP prefix.  Its parent
        // is the matching Wham prefix, including for a local/mock or a
        // self-hosted compatible backend.  Never substitute another host.
        return Ok(base.to_string());
    }
    if trimmed.ends_with("/backend-api") {
        return Ok(trimmed.to_string());
    }
    Err(
        "Codex reset-credit requests require upstream_base ending in /codex or /backend-api; refusing to redirect credentials to a different host"
            .to_string(),
    )
}

fn checked_available_credit_count(value: i64) -> Result<u64, String> {
    u64::try_from(value).map_err(|_| "Codex reset-credit availability count is invalid".to_string())
}

fn reset_outcome(code: &str) -> &'static str {
    // These are documented App Server enum values. Do not normalize unknown
    // punctuation/case variants into a successful spend result: a future
    // server state must reach the coordinator as `unknown` and stop safely.
    match code {
        "reset" => "reset",
        "nothingToReset" => "nothing_to_reset",
        "noCredit" => "no_credit",
        "alreadyRedeemed" => "already_redeemed",
        _ => "unknown",
    }
}

fn parse_models_response(value: &serde_json::Value) -> Vec<ModelSummary> {
    value
        .get("models")
        .and_then(|value| value.as_array())
        .into_iter()
        .flatten()
        .filter_map(|model| {
            let model_id = model
                .get("slug")
                .or_else(|| model.get("id"))
                .or_else(|| model.get("model_id"))
                .and_then(|value| value.as_str())?
                .trim();
            if model_id.is_empty() {
                return None;
            }
            let display_name = model
                .get("display_name")
                .or_else(|| model.get("name"))
                .and_then(|value| value.as_str())
                .unwrap_or(model_id)
                .to_string();
            Some(ModelSummary {
                model_id: model_id.to_string(),
                display_name,
            })
        })
        .collect()
}

fn extract_rate_limit_reset_credits_summary(
    value: Option<&serde_json::Value>,
) -> Option<RateLimitResetCreditsSummary> {
    let value = value?;
    let available_count = value
        .get("available_count")
        .or_else(|| value.get("availableCount"))
        .and_then(|value| value.as_i64())?;
    Some(RateLimitResetCreditsSummary {
        available_count,
        credits: None,
    })
}

fn extract_rate_summary(rate_limit: Option<&serde_json::Value>) -> QuotaRateSummary {
    let Some(serde_json::Value::Object(obj)) = rate_limit else {
        return QuotaRateSummary::default();
    };
    let five_hour = obj
        .get("primary_window")
        .and_then(|window| extract_window_summary(window, Some("5h")));
    let weekly = obj
        .get("secondary_window")
        .and_then(|window| extract_window_summary(window, Some("weekly")));
    QuotaRateSummary { five_hour, weekly }
}

fn extract_additional_rate_limits(
    value: Option<&serde_json::Value>,
) -> Vec<AdditionalRateLimitSummary> {
    let Some(items) = value.and_then(|value| value.as_array()) else {
        return Vec::new();
    };

    items
        .iter()
        .filter_map(|item| {
            let display_name = item
                .get("display_name")
                .or_else(|| item.get("displayName"))
                .or_else(|| item.get("limit_name"))
                .or_else(|| item.get("limitName"))
                .or_else(|| item.get("name"))
                .and_then(|value| value.as_str())
                .map(str::trim)
                .filter(|value| !value.is_empty())?
                .to_string();
            let rate_limit = item.get("rate_limit").or_else(|| item.get("rateLimit"))?;
            let summary = extract_rate_summary(Some(rate_limit));
            Some(AdditionalRateLimitSummary {
                display_name,
                five_hour: summary.five_hour,
                weekly: summary.weekly,
            })
        })
        .collect()
}

fn extract_window_summary(
    window: &serde_json::Value,
    default_bucket: Option<&str>,
) -> Option<QuotaWindowSummary> {
    if !window.is_object() {
        return None;
    }

    let used_percent = window
        .get("used_percent")
        .or_else(|| window.get("usedPercent"))
        .and_then(|x| x.as_f64())
        .or_else(|| {
            let used = window.get("used").and_then(|x| x.as_f64())?;
            let limit = window.get("limit").and_then(|x| x.as_f64())?;
            if limit > 0.0 {
                Some((used / limit) * 100.0)
            } else {
                None
            }
        });

    let reset_label = window
        .get("reset_label")
        .or_else(|| window.get("resetAtLabel"))
        .and_then(|x| x.as_str())
        .map(|s| s.to_string())
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            let seconds = window
                .get("reset_after_seconds")
                .or_else(|| window.get("resetAfterSeconds"))
                .and_then(|x| x.as_i64())?;
            Some(format_reset_after(seconds, default_bucket))
        })
        .unwrap_or_default();

    Some(QuotaWindowSummary {
        used_percent,
        reset_label,
    })
}

fn format_reset_after(seconds: i64, bucket: Option<&str>) -> String {
    if seconds <= 0 {
        return "reset now".to_string();
    }
    let d = Duration::from_secs(seconds as u64);
    let days = d.as_secs() / 86_400;
    let hours = (d.as_secs() % 86_400) / 3_600;
    let mins = (d.as_secs() % 3_600) / 60;
    match bucket {
        Some("weekly") => {
            if days > 0 {
                format!("resets in {}d {}h", days, hours)
            } else if hours > 0 {
                format!("resets in {}h {}m", hours, mins)
            } else {
                format!("resets in {}m", mins)
            }
        }
        _ => {
            if days > 0 {
                format!("resets in {}d {}h", days, hours)
            } else if hours > 0 {
                format!("resets in {}h {}m", hours, mins)
            } else {
                format!("resets in {}m", mins)
            }
        }
    }
}

fn extract_from_usage_nodes(nodes: &[serde_json::Value]) -> (QuotaRateSummary, QuotaRateSummary) {
    let mut code_gen = QuotaRateSummary::default();
    let mut code_review = QuotaRateSummary::default();
    for node in nodes {
        let cat = node
            .get("category")
            .or_else(|| node.get("name"))
            .and_then(|x| x.as_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        let period = node
            .get("period")
            .or_else(|| node.get("window"))
            .and_then(|x| x.as_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        let window = extract_window_summary(node, None).unwrap_or_default();
        let is_weekly = period.contains("week");
        let is_code_review = cat.contains("review");
        let is_code_gen = cat.contains("generation") || cat.contains("gen");

        if is_code_gen || (!is_code_review && !is_code_gen) {
            if is_weekly {
                code_gen.weekly = Some(window.clone());
            } else {
                code_gen.five_hour = Some(window.clone());
            }
        }
        if is_code_review {
            if is_weekly {
                code_review.weekly = Some(window.clone());
            } else {
                code_review.five_hour = Some(window);
            }
        }
    }
    (code_gen, code_review)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn extracts_additional_codex_spark_rate_limit() {
        let limits = extract_additional_rate_limits(Some(&json!([
            {
                "limit_name": "GPT-5.3-Codex-Spark",
                "rate_limit": {
                    "primary_window": {
                        "used_percent": 12.5,
                        "reset_after_seconds": 1800
                    },
                    "secondary_window": {
                        "used_percent": 34.0,
                        "reset_after_seconds": 86400
                    }
                }
            }
        ])));

        assert_eq!(limits.len(), 1);
        assert_eq!(limits[0].display_name, "GPT-5.3-Codex-Spark");
        assert_eq!(
            limits[0]
                .five_hour
                .as_ref()
                .and_then(|window| window.used_percent),
            Some(12.5)
        );
        assert_eq!(
            limits[0]
                .weekly
                .as_ref()
                .and_then(|window| window.used_percent),
            Some(34.0)
        );
    }

    #[test]
    fn primary_window_stays_in_five_hour_when_reset_exceeds_five_hours() {
        let summary = extract_rate_summary(Some(&json!({
            "primary_window": {
                "used_percent": 46.0,
                "limit_window_seconds": 604800,
                "reset_after_seconds": 579081
            },
            "secondary_window": null
        })));

        assert_eq!(
            summary
                .five_hour
                .as_ref()
                .and_then(|window| window.used_percent),
            Some(46.0)
        );
        assert_eq!(
            summary
                .five_hour
                .as_ref()
                .map(|window| window.reset_label.as_str()),
            Some("resets in 6d 16h")
        );
        assert!(summary.weekly.is_none());
    }

    #[test]
    fn keeps_primary_and_secondary_slot_mapping() {
        let summary = extract_rate_summary(Some(&json!({
            "primary_window": {
                "used_percent": 20.0,
                "limit_window_seconds": 604800,
                "reset_after_seconds": 500000
            },
            "secondary_window": {
                "used_percent": 70.0,
                "limit_window_seconds": 18000,
                "reset_after_seconds": 7200
            }
        })));

        assert_eq!(
            summary
                .five_hour
                .as_ref()
                .and_then(|window| window.used_percent),
            Some(20.0)
        );
        assert_eq!(
            summary
                .weekly
                .as_ref()
                .and_then(|window| window.used_percent),
            Some(70.0)
        );
    }

    #[test]
    fn builds_wham_reset_credit_urls_from_codex_upstream_base() {
        assert_eq!(
            wham_url(
                "https://chatgpt.com/backend-api/codex",
                "rate-limit-reset-credits/consume"
            )
            .unwrap(),
            "https://chatgpt.com/backend-api/wham/rate-limit-reset-credits/consume"
        );
        assert_eq!(
            wham_url(
                "https://chatgpt.com/backend-api",
                "rate-limit-reset-credits"
            )
            .unwrap(),
            "https://chatgpt.com/backend-api/wham/rate-limit-reset-credits"
        );
    }

    #[test]
    fn wham_reset_credit_urls_keep_a_configured_local_backend_host() {
        assert_eq!(
            wham_url("http://127.0.0.1:49152/codex", "rate-limit-reset-credits").unwrap(),
            "http://127.0.0.1:49152/wham/rate-limit-reset-credits"
        );
        assert!(wham_url("http://127.0.0.1:49152/not-codex", "usage")
            .unwrap_err()
            .contains("refusing to redirect credentials"));
    }

    #[test]
    fn documented_codex_marker_uses_its_window_reset_time() {
        let now = Utc::now().timestamp();
        let value = json!({
            "rateLimitsByLimitId": {
                "codex": {
                    "limitId": "codex",
                    "rateLimitReachedType": "rate_limit_reached",
                    "primary": {"usedPercent": 25, "resetsAt": now + 60},
                    "secondary": {"usedPercent": 25, "resetsAt": now + 3600}
                }
            }
        });
        let mut inspection = RateLimitInspection::default();
        inspect_rate_limits(&value, &mut inspection);
        assert!(inspection.reached);
        assert!(matches!(
            inspection.natural_reset_after_seconds,
            Some(seconds) if seconds <= 60
        ));
    }

    #[test]
    fn reached_bucket_with_any_unreadable_disclosed_window_has_no_safe_natural_reset_time() {
        let now = Utc::now().timestamp();
        let value = json!({
            "rateLimitsByLimitId": {
                "codex": {
                    "limitId": "codex",
                    "rateLimitReachedType": "rate_limit_reached",
                    "primary": {"resetsAt": now + 3600},
                    "secondary": {"usedPercent": 100}
                }
            }
        });
        let mut inspection = RateLimitInspection::default();
        inspect_rate_limits(&value, &mut inspection);
        assert!(inspection.state_known);
        assert!(inspection.reached);
        assert!(inspection.natural_reset_after_seconds.is_none());
    }

    #[test]
    fn only_a_null_codex_marker_proves_a_cleared_limit() {
        let cleared = json!({
            "rateLimitsByLimitId": {
                "codex": {
                    "limitId": "codex",
                    "rateLimitReachedType": null
                }
            }
        });
        let mut inspection = RateLimitInspection::default();
        inspect_rate_limits(&cleared, &mut inspection);
        assert!(inspection.state_known);
        assert!(!inspection.reached);
        assert!(inspection.cleared);
    }

    #[test]
    fn workspace_or_spend_markers_never_authorize_or_verify_reset_credit_use() {
        for marker in [
            "workspace_credit_reached",
            "spend_limit_reached",
            "None",
            " rate_limit_reached",
            "rate_limit_reached ",
        ] {
            let value = json!({
                "rateLimitsByLimitId": {
                    "codex": {
                        "limitId": "codex",
                        "rateLimitReachedType": marker,
                        "primary": {"usedPercent": 100, "resetsAt": 9_999_999_999i64}
                    }
                }
            });
            let mut inspection = RateLimitInspection::default();
            inspect_rate_limits(&value, &mut inspection);
            assert!(inspection.state_known);
            assert!(!inspection.reached);
            assert!(!inspection.cleared);
            assert!(inspection.natural_reset_after_seconds.is_none());
        }
    }

    #[test]
    fn multi_bucket_response_never_falls_back_to_a_non_codex_legacy_bucket() {
        let value = json!({
            "rateLimits": {
                "limitId": "codex",
                "rateLimitReachedType": "rate_limit_reached",
                "primary": {"resetsAt": 9_999_999_999i64}
            },
            "rateLimitsByLimitId": {
                "workspace": {
                    "limitId": "workspace",
                    "rateLimitReachedType": "rate_limit_reached"
                }
            }
        });
        let mut inspection = RateLimitInspection::default();
        inspect_rate_limits(&value, &mut inspection);
        assert!(!inspection.state_known);
        assert!(!inspection.reached);
        assert!(!inspection.cleared);
    }

    #[test]
    fn legacy_single_bucket_requires_an_explicit_codex_limit_id() {
        let codex = json!({
            "rateLimits": {
                "limitId": "codex",
                "rateLimitReachedType": "rate_limit_reached",
                "primary": {"resetsAt": 9_999_999_999i64}
            }
        });
        let non_codex = json!({
            "rateLimits": {
                "limitId": "workspace",
                "rateLimitReachedType": "rate_limit_reached",
                "primary": {"resetsAt": 9_999_999_999i64}
            }
        });
        let mut inspection = RateLimitInspection::default();
        inspect_rate_limits(&codex, &mut inspection);
        assert!(inspection.state_known);
        assert!(inspection.reached);

        let mut inspection = RateLimitInspection::default();
        inspect_rate_limits(&non_codex, &mut inspection);
        assert!(!inspection.state_known);
        assert!(!inspection.reached);
    }

    #[test]
    fn documented_credit_details_accept_camel_case_numeric_timestamps() {
        let details: RateLimitResetCreditsDetails = serde_json::from_value(json!({
            "availableCount": 1,
            "credits": [{
                "creditId": "RateLimitResetCredit_1",
                "resetType": "codexRateLimits",
                "status": "available",
                "grantedAt": 1_781_654_400i64,
                "expiresAt": 1_784_246_400i64
            }]
        }))
        .unwrap();
        assert_eq!(details.available_count, 1);
        let credit = details.credits.unwrap().pop().unwrap();
        assert_eq!(credit.id, "RateLimitResetCredit_1");
        assert_eq!(credit.reset_type, "codexRateLimits");
        assert_eq!(credit.expires_at.as_deref(), Some("1784246400"));
    }

    #[test]
    fn negative_available_credit_count_is_rejected_before_automation_can_use_it() {
        assert_eq!(checked_available_credit_count(0).unwrap(), 0);
        assert_eq!(checked_available_credit_count(1).unwrap(), 1);
        assert!(checked_available_credit_count(-1).is_err());
    }

    #[test]
    fn reset_outcome_accepts_only_documented_app_server_enums() {
        assert_eq!(reset_outcome("reset"), "reset");
        assert_eq!(reset_outcome("alreadyRedeemed"), "already_redeemed");
        assert_eq!(reset_outcome("nothingToReset"), "nothing_to_reset");
        assert_eq!(reset_outcome("noCredit"), "no_credit");
        for unexpected in [
            "already_redeemed",
            "AlreadyRedeemed",
            "nothing-to-reset",
            "no credit",
            "reset!",
            " reset",
            "reset ",
        ] {
            assert_eq!(reset_outcome(unexpected), "unknown", "{unexpected}");
        }
    }

    #[test]
    fn extracts_reset_credit_summary_from_usage_payload() {
        let summary = extract_rate_limit_reset_credits_summary(Some(&json!({
            "available_count": 3
        })))
        .expect("reset summary");

        assert_eq!(summary.available_count, 3);
        assert!(summary.credits.is_none());
    }
}
