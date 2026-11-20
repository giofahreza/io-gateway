use serde::{Deserialize, Serialize};
use std::collections::HashSet;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Service {
    #[default]
    Custom,
    QwenTokenPlan,
    QwenPayg,
}

impl Service {
    pub fn detect(base_url: Option<&str>, anthropic_base_url: Option<&str>) -> Self {
        for value in [base_url, anthropic_base_url].into_iter().flatten() {
            let Some(host) = url::Url::parse(value)
                .ok()
                .and_then(|url| url.host_str().map(str::to_owned))
            else {
                continue;
            };
            if host == "token-plan.maas.qwencloudapi.com" {
                return Self::QwenTokenPlan;
            }
            if host == "maas.qwencloudapi.com" || host.ends_with(".maas.qwencloudapi.com") {
                return Self::QwenPayg;
            }
        }
        Self::Custom
    }

    pub fn display_provider(self) -> &'static str {
        if self == Self::Custom {
            "compatible"
        } else {
            "qwen"
        }
    }

    pub fn default_urls(self) -> (Option<String>, Option<String>) {
        let host = match self {
            Self::Custom => return (None, None),
            Self::QwenTokenPlan => "token-plan",
            Self::QwenPayg => "ap-southeast-1",
        };
        (
            Some(format!(
                "https://{host}.maas.qwencloudapi.com/compatible-mode/v1"
            )),
            Some(format!(
                "https://{host}.maas.qwencloudapi.com/apps/anthropic"
            )),
        )
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Custom => "Compatible API",
            Self::QwenTokenPlan => "Qwen Cloud Token Plan",
            Self::QwenPayg => "Qwen Cloud Pay-as-you-go",
        }
    }
}

#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompatibleAccount {
    #[serde(default)]
    pub service: Service,
    #[serde(default, skip_serializing)]
    pub console_cookie: Option<String>,
    #[serde(default, skip_serializing)]
    pub cloud_auth: Option<super::cloud_auth::Credentials>,
    pub account_id: String,
    pub label: String,
    // Credentials are only written explicitly by admin::save; never serialize them to clients.
    #[serde(skip_serializing)]
    pub api_key: String,
    pub base_url: Option<String>,
    pub anthropic_base_url: Option<String>,
    #[serde(default)]
    pub models: Vec<String>,
    #[serde(skip)]
    pub file_name: Option<String>,
    #[serde(skip)]
    pub enabled: bool,
}

pub fn load_accounts(cfg: &crate::Config, disabled: &HashSet<String>) -> Vec<CompatibleAccount> {
    let Some(dir) = cfg.auth_dir.as_ref() else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut paths: Vec<_> = entries.flatten().map(|entry| entry.path()).collect();
    paths.sort();
    paths
        .into_iter()
        .filter_map(|path| {
            if path.extension()?.to_str()? != "json" {
                return None;
            }
            let value: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&path).ok()?).ok()?;
            if value.get("type")?.as_str()? != "compatible" {
                return None;
            }
            let mut account: CompatibleAccount = serde_json::from_value(value).ok()?;
            // Existing Qwen compatible accounts keep their IDs and routing.
            // Older editors wrote service:custom for known Qwen endpoints too.
            if account.service == Service::Custom {
                account.service = Service::detect(
                    account.base_url.as_deref(),
                    account.anthropic_base_url.as_deref(),
                );
            }
            if account.base_url.is_none() && account.anthropic_base_url.is_none() {
                (account.base_url, account.anthropic_base_url) = account.service.default_urls();
            }
            if account.api_key.trim().is_empty()
                || account.account_id.is_empty()
                || account.models.is_empty()
            {
                return None;
            }
            if account.base_url.is_none() && account.anthropic_base_url.is_none() {
                return None;
            }
            let file_name = path.file_name()?.to_str()?.to_string();
            account.enabled = !disabled.contains(&file_name);
            account.file_name = Some(file_name);
            Some(account)
        })
        .collect()
}

pub fn reload_state(state: &crate::AppState) {
    let disabled = state.disabled.lock().unwrap().clone();
    let loaded = load_accounts(&state.cfg, &disabled);
    let mut accounts = state.compatible_accounts.lock().unwrap();
    *accounts = loaded;
    state.quota_snapshots.lock().unwrap().remove("compatible");
    drop(accounts);
    crate::sync_usage_stats(state);
}

pub fn candidate_accounts(state: &crate::AppState, model: &str) -> Vec<CompatibleAccount> {
    let accounts: Vec<_> = state
        .compatible_accounts
        .lock()
        .unwrap()
        .iter()
        .filter(|account| account.models.iter().any(|id| id == model))
        .cloned()
        .collect();
    let mut rr = state.compatible_rr.lock().unwrap();
    let priority = crate::routing_priority_accounts_for_provider(state, "compatible");
    let picked = crate::select_ordered_account_indices_with_priority(
        accounts.len(),
        *rr,
        |idx| {
            accounts[idx].enabled
                && crate::router_account_eligible(
                    state,
                    "compatible",
                    &crate::compatible_stats_key(&accounts[idx]),
                )
        },
        |idx| priority.contains(&crate::compatible_stats_key(&accounts[idx])),
        |idx| crate::compatible_account_selection_score(state, &accounts[idx]),
    );
    if let Some(idx) = picked.first() {
        *rr = (idx + 1) % accounts.len();
        crate::router_reserve_account(
            state,
            "compatible",
            &crate::compatible_stats_key(&accounts[*idx]),
        );
    }
    picked
        .into_iter()
        .map(|idx| accounts[idx].clone())
        .collect()
}

pub fn model_entries(state: &crate::AppState) -> Vec<serde_json::Value> {
    let mut seen = HashSet::new();
    state
        .compatible_accounts
        .lock()
        .unwrap()
        .iter()
        .filter(|a| a.enabled)
        .flat_map(|a| a.models.iter())
        .filter(|id| seen.insert((*id).clone()))
        .map(|id| {
            serde_json::json!({"id":format!("cmp:{id}"),"object":"model","created":0,
            "owned_by":"compatible","provider_prefix":"cmp","upstream_model":id})
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn detects_both_qwen_protocols_without_matching_lookalike_hosts() {
        assert_eq!(
            Service::detect(
                None,
                Some("https://token-plan.maas.qwencloudapi.com/apps/anthropic")
            ),
            Service::QwenTokenPlan
        );
        assert_eq!(
            Service::detect(
                Some("https://ap-southeast-1.maas.qwencloudapi.com/compatible-mode/v1"),
                None
            ),
            Service::QwenPayg
        );
        assert_eq!(
            Service::detect(
                Some("https://token-plan.maas.qwencloudapi.com.example.org/v1"),
                None
            ),
            Service::Custom
        );
        assert_eq!(
            Service::detect(Some("https://example.org/v1"), None),
            Service::Custom
        );
    }
    #[test]
    fn known_qwen_profiles_are_detected_without_rewriting_credentials() {
        use serde_json::json;
        let dir = std::env::temp_dir().join(format!("iogw-qwen-profile-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let cfg: crate::Config = serde_json::from_value(
            json!({"listen":"127.0.0.1:0","upstream_base":"http://127.0.0.1:1",
            "proxy_api_key":"test","tokens":[],"auth_dir":dir}),
        )
        .unwrap();
        let path = dir.join("existing.json");
        let original = json!({"type":"compatible","account_id":"stable-id","label":"Saved account","api_key":"sk-sp-synthetic",
            "base_url":"https://token-plan.maas.qwencloudapi.com/compatible-mode/v1","models":["auto"]});
        for (service, expected) in [
            (None, Service::QwenTokenPlan),
            (Some("custom"), Service::QwenTokenPlan),
            (Some("qwen_payg"), Service::QwenPayg),
        ] {
            let mut value = original.clone();
            if let Some(service) = service {
                value["service"] = json!(service);
            }
            std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
            let loaded = load_accounts(&cfg, &HashSet::new());
            assert_eq!(loaded.len(), 1);
            assert_eq!(loaded[0].account_id, "stable-id");
            assert_eq!(loaded[0].service, expected);
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&std::fs::read(&path).unwrap())
                    .unwrap(),
                value
            );
        }
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn serialized_accounts_never_include_inference_or_console_credentials() {
        let account = CompatibleAccount {
            api_key: "private-api-key".into(),
            console_cookie: Some("session=private-cookie".into()),
            service: Service::QwenTokenPlan,
            ..Default::default()
        };
        let value = serde_json::to_value(account).unwrap();
        assert_eq!(value["service"], "qwen_token_plan");
        assert!(value.get("api_key").is_none());
        assert!(value.get("console_cookie").is_none());
    }
}
