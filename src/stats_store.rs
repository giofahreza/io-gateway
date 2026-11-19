use serde::{Deserialize, Serialize};
use std::{collections::HashMap, path::PathBuf};

#[derive(Clone, Copy)]
pub enum Provider {
    Codex,
    Antigravity,
    Gemini,
    Qwen,
    DeepSeek,
    Grok,
    MiniMax,
    Copilot,
    Claude,
    Glm,
    Compatible,
}

#[derive(Default, Clone, Serialize, Deserialize)]
pub struct StoredAccountUsage {
    pub label: String,
    pub account_id: String,
    pub requests: u64,
    pub errors: u64,
    #[serde(default)]
    pub prompt_total: u64,
    #[serde(default)]
    pub prompt_error_total: u64,
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub total_tokens: u64,
    #[serde(default)]
    pub cache_tokens: u64,
    #[serde(default)]
    pub reasoning_tokens: u64,
    #[serde(default)]
    pub first_seen_at: Option<String>,
    #[serde(default)]
    pub last_seen_at: Option<String>,
    #[serde(default)]
    pub last_success_at: Option<String>,
    #[serde(default)]
    pub last_error_at: Option<String>,
    #[serde(default)]
    pub last_error_message: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LatestErrorMessage {
    pub recorded_at: String,
    pub error_message: String,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct StatsStore {
    #[serde(default = "default_store_type", rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub total_requests: u64,
    #[serde(default)]
    pub total_errors: u64,
    #[serde(default)]
    pub total_prompt_total: u64,
    #[serde(default)]
    pub total_prompt_error_total: u64,
    #[serde(default)]
    pub total_input_tokens: u64,
    #[serde(default)]
    pub total_output_tokens: u64,
    #[serde(default)]
    pub total_tokens_used: u64,
    #[serde(default)]
    pub total_cache_tokens: u64,
    #[serde(default)]
    pub total_reasoning_tokens: u64,
    #[serde(default)]
    pub first_recorded_at: Option<String>,
    #[serde(default)]
    pub last_recorded_at: Option<String>,
    #[serde(default)]
    pub codex: HashMap<String, StoredAccountUsage>,
    #[serde(default)]
    pub antigravity: HashMap<String, StoredAccountUsage>,
    #[serde(default)]
    pub gemini: HashMap<String, StoredAccountUsage>,
    #[serde(default)]
    pub qwen: HashMap<String, StoredAccountUsage>,
    #[serde(default)]
    pub deepseek: HashMap<String, StoredAccountUsage>,
    #[serde(default)]
    pub grok: HashMap<String, StoredAccountUsage>,
    #[serde(default)]
    pub minimax: HashMap<String, StoredAccountUsage>,
    #[serde(default)]
    pub copilot: HashMap<String, StoredAccountUsage>,
    #[serde(default)]
    pub claude: HashMap<String, StoredAccountUsage>,
    #[serde(default)]
    pub glm: HashMap<String, StoredAccountUsage>,
    #[serde(default)]
    pub compatible: HashMap<String, StoredAccountUsage>,
}

impl Default for StatsStore {
    fn default() -> Self {
        Self {
            kind: default_store_type(),
            total_requests: 0,
            total_errors: 0,
            total_prompt_total: 0,
            total_prompt_error_total: 0,
            total_input_tokens: 0,
            total_output_tokens: 0,
            total_tokens_used: 0,
            total_cache_tokens: 0,
            total_reasoning_tokens: 0,
            first_recorded_at: None,
            last_recorded_at: None,
            codex: HashMap::new(),
            antigravity: HashMap::new(),
            gemini: HashMap::new(),
            qwen: HashMap::new(),
            deepseek: HashMap::new(),
            grok: HashMap::new(),
            minimax: HashMap::new(),
            copilot: HashMap::new(),
            claude: HashMap::new(),
            glm: HashMap::new(),
            compatible: HashMap::new(),
        }
    }
}

impl StatsStore {
    pub fn account_usage(&self, provider: Provider, key: &str) -> Option<&StoredAccountUsage> {
        match provider {
            Provider::Codex => self.codex.get(key),
            Provider::Antigravity => self.antigravity.get(key),
            Provider::Gemini => self.gemini.get(key),
            Provider::Qwen => self.qwen.get(key),
            Provider::DeepSeek => self.deepseek.get(key),
            Provider::Grok => self.grok.get(key),
            Provider::MiniMax => self.minimax.get(key),
            Provider::Copilot => self.copilot.get(key),
            Provider::Claude => self.claude.get(key),
            Provider::Glm => self.glm.get(key),
            Provider::Compatible => self.compatible.get(key),
        }
    }

    pub fn account_usage_mut(
        &mut self,
        provider: Provider,
        key: String,
    ) -> &mut StoredAccountUsage {
        match provider {
            Provider::Codex => self.codex.entry(key).or_default(),
            Provider::Antigravity => self.antigravity.entry(key).or_default(),
            Provider::Gemini => self.gemini.entry(key).or_default(),
            Provider::Qwen => self.qwen.entry(key).or_default(),
            Provider::DeepSeek => self.deepseek.entry(key).or_default(),
            Provider::Grok => self.grok.entry(key).or_default(),
            Provider::MiniMax => self.minimax.entry(key).or_default(),
            Provider::Copilot => self.copilot.entry(key).or_default(),
            Provider::Claude => self.claude.entry(key).or_default(),
            Provider::Glm => self.glm.entry(key).or_default(),
            Provider::Compatible => self.compatible.entry(key).or_default(),
        }
    }
}

pub fn load(cfg: &crate::Config) -> StatsStore {
    let path = file_path(cfg);
    let Ok(data) = std::fs::read_to_string(&path) else {
        return StatsStore::default();
    };
    serde_json::from_str(&data).unwrap_or_default()
}

pub fn save(cfg: &crate::Config, store: &StatsStore) -> Result<(), String> {
    let path = file_path(cfg);
    let data = serde_json::to_vec_pretty(store).map_err(|err| err.to_string())?;
    crate::target::atomic_write(&path, &data, true)
}

fn file_path(cfg: &crate::Config) -> PathBuf {
    if let Some(dir) = cfg.auth_dir.as_ref() {
        return PathBuf::from(dir).join("gateway-stats.json");
    }
    PathBuf::from("gateway-stats.json")
}

fn default_store_type() -> String {
    "gateway_stats".to_string()
}

pub fn load_latest_error_messages(
    cfg: &crate::Config,
) -> Result<HashMap<(String, String), LatestErrorMessage>, String> {
    crate::usage_store::latest_error_messages(cfg)
}
