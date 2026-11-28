use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::json;
use std::{collections::HashMap, time::Duration};

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
    pub project_id: String,
    pub tier_name: String,
    pub tier_id: String,
    pub tier_description: String,
    pub upgrade_text: String,
    pub groups: Vec<QuotaGroupSummary>,
    pub models: Vec<ModelQuotaSummary>,
    pub description: String,
}

#[derive(Default, Clone, Serialize)]
pub struct QuotaGroupSummary {
    pub display_name: String,
    pub description: String,
    pub five_hour: Option<QuotaBucketSummary>,
    pub weekly: Option<QuotaBucketSummary>,
}

#[derive(Default, Clone, Serialize)]
pub struct QuotaBucketSummary {
    pub used_percent: Option<f64>,
    pub remaining_percent: Option<f64>,
    pub reset_label: String,
}

#[derive(Default, Clone, Serialize)]
pub struct ModelQuotaSummary {
    pub model_id: String,
    pub display_name: String,
    pub group_display_name: String,
    pub current: Option<QuotaBucketSummary>,
    pub five_hour: Option<QuotaBucketSummary>,
    pub weekly: Option<QuotaBucketSummary>,
}

pub async fn get_quota_summaries(state: &crate::AppState) -> Vec<serde_json::Value> {
    let accounts = state.agw_accounts.lock().unwrap().clone();
    let now = std::time::Instant::now();
    let mut results = Vec::with_capacity(accounts.len());

    for account in &accounts {
        let key = cache_key(account);
        let cached = {
            let cache = state.agw_quota_cache.lock().unwrap();
            cache.get(&key).cloned()
        };

        let entry = if let Some(cached) = cached {
            if crate::quota_cache_entry_is_fresh(now, cached.fetched_at, &cached.summary) {
                cached
            } else {
                let fetched = fetch_account_quota(state, account).await;
                let mut cache = state.agw_quota_cache.lock().unwrap();
                cache.insert(key.clone(), fetched.clone());
                fetched
            }
        } else {
            let fetched = fetch_account_quota(state, account).await;
            let mut cache = state.agw_quota_cache.lock().unwrap();
            cache.insert(key.clone(), fetched.clone());
            fetched
        };

        if let Some(err) = entry.error {
            results.push(json!({
                "label": account.label,
                "email": account.email,
                "project_id": account.project_id.clone().unwrap_or_default(),
                "file_name": account.file_name.clone().unwrap_or_default(),
                "error": err
            }));
        } else {
            results.push(json!({
                "label": entry.summary.label,
                "email": entry.summary.email,
                "project_id": entry.summary.project_id,
                "file_name": account.file_name.clone().unwrap_or_default(),
                "tier_name": entry.summary.tier_name,
                "tier_id": entry.summary.tier_id,
                "tier_description": entry.summary.tier_description,
                "upgrade_text": entry.summary.upgrade_text,
                "groups": entry.summary.groups,
                "models": entry.summary.models,
                "description": entry.summary.description
            }));
        }
    }

    results
}

fn cache_key(account: &super::accounts::AntigravityAccount) -> String {
    account
        .file_name
        .clone()
        .unwrap_or_else(|| account.label.clone())
}

async fn fetch_account_quota(
    state: &crate::AppState,
    account: &super::accounts::AntigravityAccount,
) -> QuotaCacheEntry {
    let access_token = match super::auth::ensure_access_token(state, account).await {
        Ok(token) => token,
        Err(err) => {
            return QuotaCacheEntry {
                fetched_at: std::time::Instant::now(),
                summary: QuotaSummary::default(),
                error: Some(err),
            }
        }
    };

    let tier_info =
        fetch_tier_info(&state.client, &access_token, account.project_id.as_deref()).await;
    let quota_info =
        fetch_quota_summary(&state.client, &access_token, account.project_id.as_deref()).await;

    match (tier_info, quota_info) {
        (Ok(tier), Ok((groups, description, models))) => QuotaCacheEntry {
            fetched_at: std::time::Instant::now(),
            summary: QuotaSummary {
                label: account.label.clone(),
                email: account.email.clone(),
                project_id: account.project_id.clone().unwrap_or_default(),
                tier_name: tier.name,
                tier_id: tier.id,
                tier_description: tier.description,
                upgrade_text: tier.upgrade_text,
                groups,
                models,
                description,
            },
            error: None,
        },
        (Err(_tier_err), Ok((groups, description, models))) => QuotaCacheEntry {
            fetched_at: std::time::Instant::now(),
            summary: QuotaSummary {
                label: account.label.clone(),
                email: account.email.clone(),
                project_id: account.project_id.clone().unwrap_or_default(),
                groups,
                models,
                description,
                ..QuotaSummary::default()
            },
            error: None,
        },
        (Ok(_), Err(quota_err)) => QuotaCacheEntry {
            fetched_at: std::time::Instant::now(),
            summary: QuotaSummary::default(),
            error: Some(quota_err),
        },
        (Err(tier_err), Err(quota_err)) => QuotaCacheEntry {
            fetched_at: std::time::Instant::now(),
            summary: QuotaSummary::default(),
            error: Some(format!(
                "tier lookup failed: {}; quota lookup failed: {}",
                tier_err, quota_err
            )),
        },
    }
}

#[derive(Default)]
struct TierInfo {
    id: String,
    name: String,
    description: String,
    upgrade_text: String,
}

async fn fetch_tier_info(
    client: &reqwest::Client,
    access_token: &str,
    _project_id: Option<&str>,
) -> Result<TierInfo, String> {
    let body = json!({
        "metadata": {
            "ideType": "IDE_UNSPECIFIED",
            "platform": "PLATFORM_UNSPECIFIED",
            "pluginType": "GEMINI"
        }
    });
    let value =
        post_antigravity_json(client, access_token, "v1internal:loadCodeAssist", &body).await?;
    let current = value
        .get("currentTier")
        .and_then(|v| v.as_object())
        .ok_or_else(|| "loadCodeAssist missing currentTier".to_string())?;

    Ok(TierInfo {
        id: current
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string(),
        name: current
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string(),
        description: current
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string(),
        upgrade_text: current
            .get("upgradeSubscriptionText")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string(),
    })
}

async fn fetch_quota_summary(
    client: &reqwest::Client,
    access_token: &str,
    project_id: Option<&str>,
) -> Result<(Vec<QuotaGroupSummary>, String, Vec<ModelQuotaSummary>), String> {
    let body = if let Some(project_id) = project_id.filter(|s| !s.trim().is_empty()) {
        json!({ "project": project_id })
    } else {
        json!({})
    };
    let value = post_antigravity_json(
        client,
        access_token,
        "v1internal:retrieveUserQuotaSummary",
        &body,
    )
    .await?;
    let groups_value = value
        .get("groups")
        .and_then(|v| v.as_array())
        .ok_or_else(|| "retrieveUserQuotaSummary missing groups".to_string())?;
    let mut groups = Vec::new();

    for group in groups_value {
        let mut summary = QuotaGroupSummary {
            display_name: group
                .get("displayName")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string(),
            description: group
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string(),
            ..QuotaGroupSummary::default()
        };

        if let Some(buckets) = group.get("buckets").and_then(|v| v.as_array()) {
            for bucket in buckets {
                let bucket_summary = extract_bucket_summary(bucket);
                match bucket
                    .get("window")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                {
                    "5h" => summary.five_hour = Some(bucket_summary),
                    "weekly" => summary.weekly = Some(bucket_summary),
                    _ => {}
                }
            }
        }

        groups.push(summary);
    }

    let model_names = fetch_model_display_names(client, access_token)
        .await
        .unwrap_or_default();
    let models = fetch_model_quotas(client, access_token, &body, &groups, &model_names)
        .await
        .unwrap_or_default();

    let description = value
        .get("description")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();

    Ok((groups, description, models))
}

async fn fetch_model_display_names(
    client: &reqwest::Client,
    access_token: &str,
) -> Result<HashMap<String, String>, String> {
    let value = post_antigravity_json(
        client,
        access_token,
        "v1internal:fetchAvailableModels",
        &json!({}),
    )
    .await?;
    let mut names = HashMap::new();
    if let Some(models) = value.get("models").and_then(|v| v.as_object()) {
        for (model_id, model_data) in models {
            if !is_user_facing_model(model_id) {
                continue;
            }
            names.insert(
                model_id.clone(),
                model_data
                    .get("displayName")
                    .and_then(|v| v.as_str())
                    .unwrap_or(model_id)
                    .to_string(),
            );
        }
    }
    Ok(names)
}

async fn fetch_model_quotas(
    client: &reqwest::Client,
    access_token: &str,
    body: &serde_json::Value,
    groups: &[QuotaGroupSummary],
    model_names: &HashMap<String, String>,
) -> Result<Vec<ModelQuotaSummary>, String> {
    let value =
        post_antigravity_json(client, access_token, "v1internal:retrieveUserQuota", body).await?;
    let buckets = value
        .get("buckets")
        .and_then(|v| v.as_array())
        .ok_or_else(|| "retrieveUserQuota missing buckets".to_string())?;

    let mut models = Vec::new();
    for bucket in buckets {
        let Some(model_id) = bucket.get("modelId").and_then(|v| v.as_str()) else {
            continue;
        };
        if !is_user_facing_model(model_id) {
            continue;
        }

        let group = model_group_for_id(model_id, groups);
        models.push(ModelQuotaSummary {
            model_id: model_id.to_string(),
            display_name: model_names
                .get(model_id)
                .cloned()
                .unwrap_or_else(|| model_id.to_string()),
            group_display_name: group.map(|g| g.display_name.clone()).unwrap_or_default(),
            current: Some(extract_bucket_summary(bucket)),
            five_hour: group.and_then(|g| g.five_hour.clone()),
            weekly: group.and_then(|g| g.weekly.clone()),
        });
    }

    models.sort_by(|a, b| a.display_name.cmp(&b.display_name));
    Ok(models)
}

async fn post_antigravity_json(
    client: &reqwest::Client,
    access_token: &str,
    path: &str,
    body: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let mut last_error = format!("all Antigravity endpoints failed for {}", path);

    for endpoint in super::auth::ANTIGRAVITY_ENDPOINTS {
        let resp = client
            .post(format!("{}/{}", endpoint, path))
            .header("Authorization", format!("Bearer {}", access_token))
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .header("User-Agent", super::auth::antigravity_user_agent())
            .header(
                "X-Goog-Api-Client",
                "google-cloud-sdk vscode_cloudshelleditor/0.1",
            )
            .header(
                "Client-Metadata",
                r#"{"ideType":"IDE_UNSPECIFIED","platform":"PLATFORM_UNSPECIFIED","pluginType":"GEMINI"}"#,
            )
            .body(body.to_string())
            .timeout(Duration::from_secs(30))
            .send()
            .await;

        let Ok(resp) = resp else {
            last_error = format!("failed to reach Antigravity endpoint {}", path);
            continue;
        };

        let status = resp.status();
        let text = match resp.text().await {
            Ok(text) => text,
            Err(err) => {
                last_error = err.to_string();
                continue;
            }
        };
        if !status.is_success() {
            last_error = format!("{} returned {}: {}", path, status, text);
            continue;
        }

        let value = match serde_json::from_str::<serde_json::Value>(&text) {
            Ok(value) => value,
            Err(err) => {
                last_error = err.to_string();
                continue;
            }
        };
        return Ok(value);
    }

    Err(last_error)
}

fn extract_bucket_summary(bucket: &serde_json::Value) -> QuotaBucketSummary {
    let remaining_fraction = bucket.get("remainingFraction").and_then(|v| v.as_f64());
    let remaining_percent = remaining_fraction.map(|value| value.clamp(0.0, 1.0) * 100.0);
    let used_percent = remaining_fraction.map(|value| (1.0 - value.clamp(0.0, 1.0)) * 100.0);

    QuotaBucketSummary {
        used_percent,
        remaining_percent,
        reset_label: bucket
            .get("resetTime")
            .and_then(|v| v.as_str())
            .map(format_reset_time)
            .unwrap_or_default(),
    }
}

fn is_user_facing_model(model_id: &str) -> bool {
    let lower = model_id.to_ascii_lowercase();
    lower.contains("gemini") || lower.contains("claude") || lower.contains("gpt")
}

fn model_group_for_id<'a>(
    model_id: &str,
    groups: &'a [QuotaGroupSummary],
) -> Option<&'a QuotaGroupSummary> {
    let lower = model_id.to_ascii_lowercase();
    if lower.contains("gemini") {
        return groups
            .iter()
            .find(|group| group.display_name.to_ascii_lowercase().contains("gemini"));
    }
    if lower.contains("claude") || lower.contains("gpt") {
        return groups.iter().find(|group| {
            let name = group.display_name.to_ascii_lowercase();
            name.contains("claude") || name.contains("gpt")
        });
    }
    None
}

fn format_reset_time(value: &str) -> String {
    let parsed = DateTime::parse_from_rfc3339(value)
        .map(|dt| dt.with_timezone(&Utc))
        .ok();
    let Some(reset_time) = parsed else {
        return value.to_string();
    };

    let now = Utc::now();
    let delta = reset_time - now;
    let seconds = delta.num_seconds();
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
        format!("{}m", mins)
    }
}

pub fn prune_cache(
    cache: &mut HashMap<String, QuotaCacheEntry>,
    accounts: &[super::accounts::AntigravityAccount],
) {
    let active_keys = accounts
        .iter()
        .map(cache_key)
        .collect::<std::collections::HashSet<_>>();
    cache.retain(|key, _| active_keys.contains(key));
}
