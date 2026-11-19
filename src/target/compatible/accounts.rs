use serde::{Deserialize, Serialize};
use std::collections::HashSet;

#[derive(Clone, Default, Serialize, Deserialize)]
pub struct CompatibleAccount {
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
    *state.compatible_accounts.lock().unwrap() = load_accounts(&state.cfg, &disabled);
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
