use axum::http::HeaderValue;
use axum::{
    body::Body,
    extract::{Form, OriginalUri, Path, Query, State},
    http::{HeaderMap, Method, StatusCode},
    response::{IntoResponse, Response},
    routing::any,
    Json, Router,
};
use bytes::{Bytes, BytesMut};
use clap::Parser;
use futures_util::{stream, Stream, StreamExt};
use serde::{Deserialize, Serialize};
use std::{
    cmp::Ordering,
    collections::{HashMap, HashSet, VecDeque},
    future::Future,
    net::SocketAddr,
    path::{Path as FsPath, PathBuf},
    pin::Pin,
    sync::{mpsc, Arc, Mutex, OnceLock, RwLock},
    time::Duration,
};
use tracing::{error, info, warn};
use uuid::Uuid;
mod admin_auth;
mod api_keys;
mod custom_models;
mod notifications;
mod source;
mod stats_store;
mod target;
mod usage_store;
use source::v1::response::{
    model_retrieve_to_openai_json, models_list_to_openai_json, openai_error_body,
    sse_to_response_json, status_to_error_code, status_to_error_type, upstream_error_to_openai,
};
use source::{route_request, ResponseMode, RoutedRequest, TargetModel};
use stats_store::{Provider, StatsStore};
use target::codex::auth::PendingOAuth;
use target::codex::quota::QuotaCacheEntry;
use target::codex::tokens::UpstreamToken;

#[derive(Clone, Default)]
struct UnifiedModelCatalog {
    fetched_at: Option<std::time::Instant>,
    models: Vec<serde_json::Value>,
    provider_models: HashMap<String, Vec<serde_json::Value>>,
    refresh_in_flight: bool,
}

static UNIFIED_MODEL_CACHE: OnceLock<tokio::sync::Mutex<UnifiedModelCatalog>> = OnceLock::new();
type CodexModelCache = Option<(std::time::Instant, Bytes)>;
static CODEX_MODEL_CACHE: OnceLock<tokio::sync::Mutex<CodexModelCache>> = OnceLock::new();
const MODEL_CACHE_TTL_SECONDS: u64 = 10 * 60;
const MODEL_LIST_TIMEOUT: Duration = Duration::from_secs(3);
const QUOTA_REFRESH_SECONDS: u64 = 60;
const EXHAUSTED_QUOTA_REFRESH_SECONDS: u64 = 60 * 60;
const CONFIG_PATH_ENV: &str = "IO_GATEWAY_CONFIG";
const MODEL_PROVIDER_PREFIXES: [&str; 10] = [
    "cod", "agw", "gem", "qwn", "dsk", "grk", "min", "cop", "cld", "glm",
];

#[derive(Debug, Parser)]
#[command(name = "io-gateway", about = "IO Gateway API proxy")]
struct GatewayArgs {
    /// Path to the runtime configuration file. Defaults to $IO_GATEWAY_CONFIG, then ./config.json.
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,
}
const MOBILE_QUOTA_SCALAR_KEYS: [&str; 25] = [
    "label",
    "email",
    "login",
    "name",
    "file_name",
    "account_id",
    "organization_uuid",
    "display_name",
    "limit_name",
    "model_id",
    "model",
    "scope",
    "used_percent",
    "usage_percent",
    "percent_used",
    "usedPercent",
    "remaining_percent",
    "limit",
    "remaining",
    "used",
    "reset_label",
    "used_text",
    "limit_text",
    "user_id",
    "team_id",
];
const MOBILE_QUOTA_CONTAINER_KEYS: [&str; 12] = [
    "code_generation",
    "code_review",
    "five_hour",
    "weekly",
    "additional_rate_limits",
    "models",
    "current",
    "quota",
    "limit",
    "groups",
    "limits",
    "current_window",
];
const MOBILE_RATE_LIMIT_RESET_CREDIT_SCALAR_KEYS: [&str; 7] = [
    "id",
    "reset_type",
    "status",
    "granted_at",
    "expires_at",
    "title",
    "description",
];

#[derive(Clone)]
struct AppState {
    cfg: Arc<Config>,
    config_path: PathBuf,
    rr: Arc<Mutex<usize>>,
    agw_rr: Arc<Mutex<usize>>,
    gemini_rr: Arc<Mutex<usize>>,
    qwen_rr: Arc<Mutex<usize>>,
    deepseek_rr: Arc<Mutex<usize>>,
    minimax_rr: Arc<Mutex<usize>>,
    grok_rr: Arc<Mutex<usize>>,
    copilot_rr: Arc<Mutex<usize>>,
    claude_rr: Arc<Mutex<usize>>,
    glm_rr: Arc<Mutex<usize>>,
    custom_model_rr: Arc<Mutex<HashMap<String, usize>>>,
    client: reqwest::Client,
    tokens: Arc<Mutex<Vec<UpstreamToken>>>,
    agw_accounts: Arc<Mutex<Vec<target::antigravity::accounts::AntigravityAccount>>>,
    gemini_accounts: Arc<Mutex<Vec<target::gemini::accounts::GeminiAccount>>>,
    qwen_accounts: Arc<Mutex<Vec<target::qwen::accounts::QwenAccount>>>,
    deepseek_accounts: Arc<Mutex<Vec<target::deepseek::accounts::DeepSeekAccount>>>,
    minimax_accounts: Arc<Mutex<Vec<target::minimax::accounts::MiniMaxAccount>>>,
    minimax_video_tasks: Arc<Mutex<HashMap<String, target::minimax::media::VideoTaskBinding>>>,
    grok_accounts: Arc<Mutex<Vec<target::grok::accounts::GrokAccount>>>,
    copilot_accounts: Arc<Mutex<Vec<target::copilot::accounts::CopilotAccount>>>,
    claude_accounts: Arc<Mutex<Vec<target::claude::accounts::ClaudeAccount>>>,
    glm_accounts: Arc<Mutex<Vec<target::glm::accounts::GlmAccount>>>,
    custom_models: Arc<Mutex<Vec<custom_models::CustomModel>>>,
    stats: Arc<Mutex<UsageStats>>,
    persisted_stats: Arc<Mutex<StatsStore>>,
    quota_cache: Arc<Mutex<Vec<Option<QuotaCacheEntry>>>>,
    agw_quota_cache: Arc<Mutex<HashMap<String, target::antigravity::quota::QuotaCacheEntry>>>,
    gemini_quota_cache: Arc<Mutex<HashMap<String, target::gemini::quota::QuotaCacheEntry>>>,
    qwen_quota_cache: Arc<Mutex<HashMap<String, target::qwen::quota::QuotaCacheEntry>>>,
    minimax_quota_cache: Arc<Mutex<HashMap<String, target::minimax::quota::QuotaCacheEntry>>>,
    deepseek_quota_cache: Arc<Mutex<HashMap<String, target::deepseek::quota::QuotaCacheEntry>>>,
    claude_quota_cache: Arc<Mutex<HashMap<String, target::claude::quota::QuotaCacheEntry>>>,
    glm_quota_cache: Arc<Mutex<HashMap<String, target::glm::quota::QuotaCacheEntry>>>,
    quota_snapshots: Arc<Mutex<HashMap<String, Vec<serde_json::Value>>>>,
    oauth_pending: Arc<Mutex<HashMap<String, PendingOAuth>>>,
    agw_oauth_pending: Arc<Mutex<HashMap<String, target::antigravity::auth::PendingOAuth>>>,
    gemini_oauth_pending: Arc<Mutex<HashMap<String, std::time::Instant>>>,
    qwen_oauth_pending: Arc<Mutex<HashMap<String, target::qwen::auth::PendingOAuth>>>,
    grok_oauth_pending: Arc<Mutex<HashMap<String, target::grok::auth::PendingOAuth>>>,
    copilot_oauth_pending: Arc<Mutex<HashMap<String, target::copilot::auth::PendingDevice>>>,
    claude_oauth_pending: Arc<Mutex<HashMap<String, target::claude::auth::PendingOAuth>>>,
    admin_sessions: Arc<Mutex<HashMap<String, admin_auth::AdminSession>>>,
    admin_login_attempts: Arc<Mutex<HashMap<String, admin_auth::LoginAttemptState>>>,
    api_keys: Arc<Mutex<api_keys::ApiKeyStore>>,
    api_key_cache: Arc<RwLock<HashMap<String, ApiKeyCacheEntry>>>,
    api_key_last_used: Arc<Mutex<HashMap<String, String>>>,
    request_api_key_id: Option<String>,
    internal_proxy_secret: Arc<String>,
    notification_settings: Arc<Mutex<notifications::NotificationSettings>>,
    account_routing: Arc<Mutex<AccountRoutingSettings>>,
    disabled: Arc<Mutex<HashSet<String>>>,
    persistence_tx: mpsc::Sender<PersistenceEvent>,
    account_router: Arc<Mutex<HashMap<String, AccountRuntimeState>>>,
    account_refresh_locks: Arc<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>>,
}

#[derive(Clone)]
enum ApiKeyCacheEntry {
    Verified(AuthenticatedApiKey),
    Rejected(std::time::Instant),
}

#[derive(Clone)]
struct AuthenticatedApiKey {
    id: Option<String>,
    access: api_keys::ApiKeyAccess,
}

#[derive(Default, Clone, Serialize, Deserialize)]
struct AccountRoutingSettings {
    #[serde(default)]
    providers: HashMap<String, AccountRoutingProviderSettings>,
}

#[derive(Default, Clone, Serialize, Deserialize)]
struct AccountRoutingProviderSettings {
    #[serde(default)]
    priority_accounts: Vec<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SourceApi {
    V1,
    Codex,
    Claude,
}

#[derive(Debug, Deserialize)]
struct Config {
    // Proxy listen port
    listen: String,
    // Upstream base url, e.g. https://chatgpt.com/backend-api/codex
    upstream_base: String,
    // One shared API key used by your Codex CLI (proxy client)
    proxy_api_key: String,
    // List of codex account access tokens (or API keys) to rotate
    tokens: Vec<String>,
    // Optional directory containing Codex credential json files
    auth_dir: Option<String>,
    // Optional list of disabled credential filenames
    disabled_files: Option<Vec<String>>,
    #[serde(default)]
    admin_auth: admin_auth::AdminAuthConfig,
    #[serde(default)]
    oauth: target::oauth::OAuthConfig,
    #[serde(default = "default_max_request_body_bytes")]
    max_request_body_bytes: usize,
    #[serde(default = "default_max_concurrent_requests")]
    max_concurrent_requests: usize,
    #[serde(default)]
    trusted_proxy: bool,
    #[serde(default = "default_history_retention_days")]
    history_retention_days: u64,
    #[serde(default = "default_history_max_entries")]
    history_max_entries: usize,
    #[serde(default = "default_upstream_connect_timeout_seconds")]
    upstream_connect_timeout_seconds: u64,
    #[serde(default = "default_upstream_read_timeout_seconds")]
    upstream_read_timeout_seconds: u64,
    #[serde(default = "default_upstream_first_event_timeout_seconds")]
    upstream_first_event_timeout_seconds: u64,
}

fn default_max_request_body_bytes() -> usize {
    16 * 1024 * 1024
}

fn default_max_concurrent_requests() -> usize {
    128
}

fn default_history_retention_days() -> u64 {
    30
}

fn default_history_max_entries() -> usize {
    200_000
}

fn default_upstream_connect_timeout_seconds() -> u64 {
    10
}

fn default_upstream_read_timeout_seconds() -> u64 {
    120
}

fn default_upstream_first_event_timeout_seconds() -> u64 {
    45
}

#[derive(Debug, Deserialize)]
struct ApiKeyCreateRequest {
    label: Option<String>,
    #[serde(default)]
    access: api_keys::ApiKeyAccess,
}

#[derive(Debug, Deserialize)]
struct ApiKeyRevokeRequest {
    id: String,
}

#[derive(Debug, Deserialize)]
struct ApiKeyAccessUpdateRequest {
    id: String,
    access: api_keys::ApiKeyAccess,
}

#[derive(Debug, Deserialize)]
struct AccountRoutingPriorityRequest {
    provider: String,
    account: String,
    priority: bool,
}

#[derive(Debug, Deserialize)]
struct ModelCatalogRefreshForm {
    provider: Option<String>,
    file_name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AdminTestApiRequest {
    model: String,
    prompt: String,
    instructions: Option<String>,
    max_output_tokens: Option<u32>,
}

#[derive(Default, Clone, Serialize)]
struct UsageStats {
    codex_accounts: Vec<AccountUsage>,
    agw_accounts: Vec<AccountUsage>,
    gemini_accounts: Vec<AccountUsage>,
    qwen_accounts: Vec<AccountUsage>,
    deepseek_accounts: Vec<AccountUsage>,
    minimax_accounts: Vec<AccountUsage>,
    grok_accounts: Vec<AccountUsage>,
    copilot_accounts: Vec<AccountUsage>,
    claude_accounts: Vec<AccountUsage>,
    glm_accounts: Vec<AccountUsage>,
    total_requests: u64,
    total_errors: u64,
    total_prompt_total: u64,
    total_prompt_error_total: u64,
    total_input_tokens: u64,
    total_output_tokens: u64,
    total_tokens_used: u64,
    total_cache_tokens: u64,
    total_reasoning_tokens: u64,
    first_recorded_at: Option<String>,
    last_recorded_at: Option<String>,
}

#[derive(Default, Clone, Serialize)]
struct AccountUsage {
    #[serde(skip_serializing)]
    key: String,
    label: String,
    account_id: String,
    requests: u64,
    errors: u64,
    prompt_total: u64,
    prompt_error_total: u64,
    input_tokens: u64,
    output_tokens: u64,
    total_tokens: u64,
    cache_tokens: u64,
    reasoning_tokens: u64,
    first_seen_at: Option<String>,
    last_seen_at: Option<String>,
    last_success_at: Option<String>,
    last_error_at: Option<String>,
    last_error_message: Option<String>,
}

#[derive(Default, Clone)]
pub(crate) struct PromptMetrics {
    input_chars: u64,
    prompt_items: u64,
    is_prompt: bool,
}

impl PromptMetrics {
    pub(crate) fn estimated_input_tokens(&self) -> u64 {
        estimated_tokens_from_chars(self.input_chars)
    }
}

#[derive(Default, Clone)]
pub(crate) struct UsageMetrics {
    input_tokens: u64,
    output_tokens: u64,
    total_tokens: u64,
    cache_tokens: u64,
    reasoning_tokens: u64,
    raw_usage: Option<serde_json::Value>,
}

#[derive(Clone)]
pub(crate) struct UsageContext {
    provider: Provider,
    provider_name: &'static str,
    key: String,
    label: String,
    account_id: String,
    credential_file: Option<String>,
    model: Option<String>,
    request_path: String,
    prompt: PromptMetrics,
}

pub(crate) struct StreamRequestGuard {
    state: AppState,
    provider: &'static str,
    account_key: String,
    finished: bool,
}

impl StreamRequestGuard {
    pub(crate) fn new(state: &AppState, context: &UsageContext) -> Self {
        Self {
            state: state.clone(),
            provider: context.provider_name,
            account_key: context.key.clone(),
            finished: false,
        }
    }

    pub(crate) fn finish(&mut self) {
        self.finished = true;
    }
}

impl Drop for StreamRequestGuard {
    fn drop(&mut self) {
        if !self.finished {
            router_request_abandoned(&self.state, self.provider, &self.account_key);
        }
    }
}

#[derive(Default, Clone)]
struct CounterDelta {
    request_delta: u64,
    error_delta: u64,
    prompt_total_delta: u64,
    prompt_error_total_delta: u64,
    input_tokens_delta: u64,
    output_tokens_delta: u64,
    total_tokens_delta: u64,
    cache_tokens_delta: u64,
    reasoning_tokens_delta: u64,
    observed_at: Option<String>,
    success_at: Option<String>,
    error_at: Option<String>,
    error_message: Option<String>,
}

enum PersistenceEvent {
    StatsDirty,
    History(usage_store::UsageHistoryEntry),
    ApiKeys(api_keys::ApiKeyStore),
    Shutdown(mpsc::SyncSender<()>),
}

#[derive(Default)]
struct AccountRuntimeState {
    active_requests: usize,
    reserved_requests: usize,
    recent_requests: VecDeque<std::time::Instant>,
    consecutive_failures: u32,
    cooldown_until: Option<std::time::Instant>,
    probing: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AccountRuntimeStatus {
    Healthy,
    Cooldown,
    Probing,
    Disabled,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt().with_env_filter("info").init();

    let args = GatewayArgs::parse();
    let requested_config_path = select_config_path(
        args.config,
        std::env::var_os(CONFIG_PATH_ENV).map(PathBuf::from),
    );
    let (cfg, config_path) = load_config(&requested_config_path);
    let disabled = cfg
        .disabled_files
        .clone()
        .unwrap_or_default()
        .into_iter()
        .collect::<HashSet<_>>();
    let tokens = target::codex::tokens::load_tokens(&cfg, &disabled);
    let agw_accounts = target::antigravity::accounts::load_accounts(&cfg, &disabled);
    let gemini_accounts = target::gemini::accounts::load_accounts(&cfg, &disabled);
    let qwen_accounts = target::qwen::accounts::load_accounts(&cfg, &disabled);
    let deepseek_accounts = target::deepseek::accounts::load_accounts(&cfg, &disabled);
    let grok_accounts = target::grok::accounts::load_accounts(&cfg, &disabled);
    let minimax_accounts = target::minimax::accounts::load_accounts(&cfg, &disabled);
    let copilot_accounts = target::copilot::accounts::load_accounts(&cfg, &disabled);
    let claude_accounts = target::claude::accounts::load_accounts(&cfg, &disabled);
    let glm_accounts = target::glm::accounts::load_accounts(&cfg, &disabled);
    let custom_models = custom_models::load(&cfg);
    let persisted_stats = Arc::new(Mutex::new(stats_store::load(&cfg)));
    let admin_sessions = admin_auth::load_sessions(&admin_session_path(&cfg));
    let notification_settings = notifications::load(&cfg);
    let account_routing = load_account_routing_settings(&cfg);
    let mut api_key_store = api_keys::load(&cfg);
    match api_keys::bootstrap_legacy_key(&mut api_key_store, &cfg.proxy_api_key, &now_rfc3339()) {
        Ok(true) => {
            if let Err(err) = api_keys::save(&cfg, &api_key_store) {
                error!("failed to persist API key store: {}", err);
            }
        }
        Ok(false) => {}
        Err(err) => {
            error!("failed to bootstrap legacy proxy API key: {}", err);
        }
    }
    let stats = build_usage_stats(
        &tokens,
        &agw_accounts,
        &gemini_accounts,
        &qwen_accounts,
        &deepseek_accounts,
        &grok_accounts,
        &minimax_accounts,
        &copilot_accounts,
        &claude_accounts,
        &glm_accounts,
        &persisted_stats.lock().unwrap(),
    );
    let quota_cache = vec![None; tokens.len()];

    let client = reqwest::Client::builder()
        .http1_only()
        .connect_timeout(Duration::from_secs(
            cfg.upstream_connect_timeout_seconds.max(1),
        ))
        .read_timeout(Duration::from_secs(
            cfg.upstream_read_timeout_seconds.max(1),
        ))
        .timeout(Duration::from_secs(30 * 60))
        .tcp_keepalive(Duration::from_secs(60))
        .pool_idle_timeout(None)
        .no_gzip()
        .no_brotli()
        .no_deflate()
        .build()
        .unwrap();

    let (persistence_tx, persistence_rx) = mpsc::channel();
    let cfg = Arc::new(cfg);
    let persistence_worker =
        start_persistence_worker(cfg.clone(), persisted_stats.clone(), persistence_rx);

    let state = AppState {
        cfg: cfg.clone(),
        config_path,
        rr: Arc::new(Mutex::new(0)),
        agw_rr: Arc::new(Mutex::new(0)),
        gemini_rr: Arc::new(Mutex::new(0)),
        qwen_rr: Arc::new(Mutex::new(0)),
        deepseek_rr: Arc::new(Mutex::new(0)),
        grok_rr: Arc::new(Mutex::new(0)),
        minimax_rr: Arc::new(Mutex::new(0)),
        copilot_rr: Arc::new(Mutex::new(0)),
        claude_rr: Arc::new(Mutex::new(0)),
        glm_rr: Arc::new(Mutex::new(0)),
        custom_model_rr: Arc::new(Mutex::new(HashMap::new())),
        client,
        tokens: Arc::new(Mutex::new(tokens)),
        agw_accounts: Arc::new(Mutex::new(agw_accounts)),
        gemini_accounts: Arc::new(Mutex::new(gemini_accounts)),
        qwen_accounts: Arc::new(Mutex::new(qwen_accounts)),
        deepseek_accounts: Arc::new(Mutex::new(deepseek_accounts)),
        grok_accounts: Arc::new(Mutex::new(grok_accounts)),
        minimax_accounts: Arc::new(Mutex::new(minimax_accounts)),
        minimax_video_tasks: Arc::new(Mutex::new(HashMap::new())),
        copilot_accounts: Arc::new(Mutex::new(copilot_accounts)),
        claude_accounts: Arc::new(Mutex::new(claude_accounts)),
        glm_accounts: Arc::new(Mutex::new(glm_accounts)),
        custom_models: Arc::new(Mutex::new(custom_models)),
        stats: Arc::new(Mutex::new(stats)),
        persisted_stats,
        quota_cache: Arc::new(Mutex::new(quota_cache)),
        agw_quota_cache: Arc::new(Mutex::new(HashMap::new())),
        gemini_quota_cache: Arc::new(Mutex::new(HashMap::new())),
        qwen_quota_cache: Arc::new(Mutex::new(HashMap::new())),
        minimax_quota_cache: Arc::new(Mutex::new(HashMap::new())),
        deepseek_quota_cache: Arc::new(Mutex::new(HashMap::new())),
        claude_quota_cache: Arc::new(Mutex::new(HashMap::new())),
        glm_quota_cache: Arc::new(Mutex::new(HashMap::new())),
        quota_snapshots: Arc::new(Mutex::new(HashMap::new())),
        oauth_pending: Arc::new(Mutex::new(HashMap::new())),
        agw_oauth_pending: Arc::new(Mutex::new(HashMap::new())),
        gemini_oauth_pending: Arc::new(Mutex::new(HashMap::new())),
        qwen_oauth_pending: Arc::new(Mutex::new(HashMap::new())),
        grok_oauth_pending: Arc::new(Mutex::new(HashMap::new())),
        copilot_oauth_pending: Arc::new(Mutex::new(HashMap::new())),
        claude_oauth_pending: Arc::new(Mutex::new(HashMap::new())),
        admin_sessions: Arc::new(Mutex::new(admin_sessions)),
        admin_login_attempts: Arc::new(Mutex::new(HashMap::new())),
        api_keys: Arc::new(Mutex::new(api_key_store)),
        api_key_cache: Arc::new(RwLock::new(HashMap::new())),
        api_key_last_used: Arc::new(Mutex::new(HashMap::new())),
        request_api_key_id: None,
        internal_proxy_secret: Arc::new(Uuid::new_v4().simple().to_string()),
        notification_settings: Arc::new(Mutex::new(notification_settings)),
        account_routing: Arc::new(Mutex::new(account_routing)),
        disabled: Arc::new(Mutex::new(disabled)),
        persistence_tx,
        account_router: Arc::new(Mutex::new(HashMap::new())),
        account_refresh_locks: Arc::new(Mutex::new(HashMap::new())),
    };
    migrate_qwen_usage_keys(&state);
    migrate_grok_usage_keys(&state);
    prune_account_routing_settings(&state);
    backfill_last_error_messages_from_history(&state);
    sync_usage_stats(&state);

    let app = Router::new()
        .route("/health", any(health))
        .route("/ready", any(ready))
        .route("/favicon.ico", any(favicon_route))
        .route("/", any(dashboard_root))
        .route("/dashboard", any(dashboard))
        .route("/admin/session", any(admin_session_route))
        .route("/admin/login", any(admin_login_route))
        .route("/admin/logout", any(admin_logout_route))
        .route("/admin/api-keys", any(admin_api_keys_route))
        .route("/admin/api-keys/create", any(admin_api_keys_create_route))
        .route("/admin/api-keys/access", any(admin_api_keys_access_route))
        .route("/admin/api-keys/revoke", any(admin_api_keys_revoke_route))
        .route("/admin/account-routing", any(admin_account_routing_route))
        .route(
            "/admin/account-routing/priority",
            any(admin_account_routing_priority_route),
        )
        .route("/admin/test-api", any(admin_test_api_route))
        .route("/dashboard.json", any(dashboard_json))
        .route("/dashboard/snapshot.json", any(dashboard_snapshot_route))
        .route("/quota.json", any(quota_json_route))
        .route("/models/refresh", any(model_catalog_refresh_route))
        .route(
            "/codex/rate-limit-reset-credit/consume",
            any(codex_rate_limit_reset_consume_route),
        )
        .route("/credentials/delete", any(delete_credential_route))
        .route("/credentials/toggle", any(toggle_credential_route))
        .route("/login/codex/start", any(login_start_route))
        .route("/login/codex/submit", any(login_submit_route))
        .route("/agw/accounts.json", any(agw_accounts_route))
        .route("/agw/quota.json", any(agw_quota_json_route))
        .route("/login/antigravity/start", any(agw_login_start_route))
        .route("/login/antigravity/submit", any(agw_login_submit_route))
        .route("/gemini/accounts.json", any(gemini_accounts_route))
        .route("/gemini/quota.json", any(gemini_quota_json_route))
        .route("/minimax/quota.json", any(minimax_quota_json_route))
        .route("/deepseek/quota.json", any(deepseek_quota_json_route))
        .route("/login/gemini/start", any(gemini_login_start_route))
        .route("/login/gemini/submit", any(gemini_login_submit_route))
        .route("/qwen/accounts.json", any(qwen_accounts_route))
        .route("/qwen/quota.json", any(qwen_quota_json_route))
        .route("/login/qwen/start", any(qwen_login_start_route))
        .route("/login/qwen/submit", any(qwen_login_submit_route))
        .route("/login/qwen/status", any(qwen_login_status_route))
        .route("/login/:provider/callback", any(oauth_login_callback_route))
        .route("/deepseek/accounts.json", any(deepseek_accounts_route))
        .route("/login/deepseek/start", any(deepseek_login_start_route))
        .route("/grok/accounts.json", any(grok_accounts_route))
        .route("/grok/quota.json", any(grok_quota_route))
        .route("/login/grok/start", any(grok_login_start_route))
        .route("/login/grok/submit", any(grok_login_submit_route))
        .route("/login/grok/status", any(grok_login_status_route))
        .route("/minimax/accounts.json", any(minimax_accounts_route))
        .route("/login/minimax/start", any(minimax_login_start_route))
        .route("/copilot/accounts.json", any(copilot_accounts_route))
        .route("/copilot/quota.json", any(copilot_quota_json_route))
        .route("/login/copilot/start", any(copilot_login_start_route))
        .route("/login/copilot/submit", any(copilot_login_submit_route))
        .route("/claude/accounts.json", any(claude_accounts_route))
        .route("/claude/quota.json", any(claude_quota_json_route))
        .route("/login/claude/start", any(claude_login_start_route))
        .route("/login/claude/submit", any(claude_login_submit_route))
        .route("/glm/accounts.json", any(glm_accounts_route))
        .route("/glm/quota.json", any(glm_quota_json_route))
        .route("/login/glm/start", any(glm_login_start_route))
        .route("/custom-models.json", any(custom_models_json_route))
        .route("/custom-models/save", any(custom_models_save_route))
        .route("/custom-models/delete", any(custom_models_delete_route))
        .route("/notifications/settings", any(notification_settings_route))
        .route("/notifications/test", any(notification_test_route))
        .route("/usage/summary.json", any(usage_summary_route))
        .route("/usage/mobile.json", any(mobile_usage_route))
        .route(
            "/usage/filter-summary.json",
            any(usage_filter_summary_route),
        )
        .route("/usage/history.json", any(usage_history_route))
        .route("/usage/context-history.json", any(context_history_route))
        .route("/temp-files/:name", any(temp_file_route))
        .route("/docs", any(source::openapi::swagger_ui_redirect))
        .route("/docs/", any(source::openapi::swagger_ui_root))
        .route("/docs/*rest", any(source::openapi::swagger_ui_asset))
        .route("/api-docs/openapi.json", any(source::openapi::openapi_json))
        .route("/*path", any(proxy))
        .with_state(state.clone())
        .layer(axum::extract::DefaultBodyLimit::max(
            state.cfg.max_request_body_bytes,
        ))
        .layer(tower::limit::ConcurrencyLimitLayer::new(
            state.cfg.max_concurrent_requests,
        ));

    let quota_refresh = tokio::spawn(background_quota_refresh(state.clone()));
    let maintenance = tokio::spawn(background_maintenance(state.clone()));

    let addr: SocketAddr = state.cfg.listen.parse().expect("invalid listen address");
    info!("listening on {}", addr);
    axum::serve(tokio::net::TcpListener::bind(addr).await.unwrap(), app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .unwrap();

    quota_refresh.abort();
    maintenance.abort();
    let (flushed_tx, flushed_rx) = mpsc::sync_channel(0);
    if state
        .persistence_tx
        .send(PersistenceEvent::Shutdown(flushed_tx))
        .is_ok()
    {
        let _ = flushed_rx.recv_timeout(Duration::from_secs(10));
    }
    let _ = persistence_worker.join();
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("failed to install SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = terminate.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
    info!("shutdown signal received; draining active requests");
}

/// Returns `ok` when the gateway process is running.
#[utoipa::path(
    get,
    path = "/health",
    responses((status = 200, description = "Gateway health check", body = String))
)]
async fn health() -> impl IntoResponse {
    (StatusCode::OK, "ok")
}

async fn ready(State(state): State<AppState>) -> impl IntoResponse {
    let has_enabled_account = state
        .tokens
        .lock()
        .unwrap()
        .iter()
        .any(|token| token.enabled)
        || state
            .agw_accounts
            .lock()
            .unwrap()
            .iter()
            .any(|account| account.enabled)
        || state
            .gemini_accounts
            .lock()
            .unwrap()
            .iter()
            .any(|account| account.enabled)
        || state
            .qwen_accounts
            .lock()
            .unwrap()
            .iter()
            .any(|account| account.enabled)
        || state
            .deepseek_accounts
            .lock()
            .unwrap()
            .iter()
            .any(|account| account.enabled)
        || state
            .minimax_accounts
            .lock()
            .unwrap()
            .iter()
            .any(|account| account.enabled)
        || state
            .grok_accounts
            .lock()
            .unwrap()
            .iter()
            .any(|account| account.enabled)
        || state
            .copilot_accounts
            .lock()
            .unwrap()
            .iter()
            .any(|account| account.enabled)
        || state
            .claude_accounts
            .lock()
            .unwrap()
            .iter()
            .any(|account| account.enabled)
        || state
            .glm_accounts
            .lock()
            .unwrap()
            .iter()
            .any(|account| account.enabled);
    if has_enabled_account {
        (StatusCode::OK, "ready")
    } else {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            "no enabled upstream accounts",
        )
    }
}

async fn background_quota_refresh(state: AppState) {
    let mut interval = tokio::time::interval(Duration::from_secs(60));
    loop {
        interval.tick().await;
        let (codex, antigravity, gemini, qwen, deepseek, minimax, copilot, claude, glm) = tokio::join!(
            target::codex::quota::get_quota_summaries(&state),
            target::antigravity::quota::get_quota_summaries(&state),
            target::gemini::quota::get_quota_summaries(&state),
            target::qwen::quota::get_quota_summaries(&state),
            target::deepseek::quota::get_quota_summaries(&state),
            target::minimax::quota::get_quota_summaries(&state),
            target::copilot::admin::quota_accounts(&state),
            target::claude::quota::get_quota_summaries(&state),
            target::glm::quota::get_quota_summaries(&state),
        );
        {
            let mut snapshots = state.quota_snapshots.lock().unwrap();
            snapshots.insert("codex".to_string(), codex.clone());
            snapshots.insert("antigravity".to_string(), antigravity.clone());
            snapshots.insert("gemini".to_string(), gemini.clone());
            snapshots.insert("qwen".to_string(), qwen.clone());
            snapshots.insert("deepseek".to_string(), deepseek.clone());
            snapshots.insert("minimax".to_string(), minimax.clone());
            snapshots.insert("copilot".to_string(), copilot.clone());
            snapshots.insert("claude".to_string(), claude.clone());
            snapshots.insert("glm".to_string(), glm.clone());
        }
        let now = now_rfc3339();
        let options = notification_account_options(&state);
        for (provider, label, accounts) in [
            ("codex", "Codex", codex),
            ("antigravity", "Antigravity", antigravity),
            ("gemini", "Gemini", gemini),
            ("qwen", "Qwen", qwen),
            ("deepseek", "DeepSeek", deepseek),
            ("minimax", "MiniMax", minimax),
            ("copilot", "GitHub Copilot", copilot),
            ("claude", "Claude", claude),
            ("glm", "GLM", glm),
        ] {
            notifications::notify_model_quota_transitions(
                &state,
                provider,
                label,
                &accounts,
                options.clone(),
                &now,
            );
        }
    }
}

fn quota_snapshot(state: &AppState, provider: &str) -> Vec<serde_json::Value> {
    state
        .quota_snapshots
        .lock()
        .unwrap()
        .get(provider)
        .cloned()
        .unwrap_or_default()
}

pub(crate) fn quota_cache_entry_is_fresh<T: Serialize>(
    now: std::time::Instant,
    fetched_at: std::time::Instant,
    summary: &T,
) -> bool {
    let snapshot = serde_json::to_value(summary).unwrap_or(serde_json::Value::Null);
    now.duration_since(fetched_at) < quota_refresh_interval_for_snapshot(&snapshot)
}

fn quota_refresh_interval_for_snapshot(snapshot: &serde_json::Value) -> Duration {
    if quota_snapshot_is_exhausted(snapshot) {
        Duration::from_secs(EXHAUSTED_QUOTA_REFRESH_SECONDS)
    } else {
        Duration::from_secs(QUOTA_REFRESH_SECONDS)
    }
}

fn quota_snapshot_is_exhausted(snapshot: &serde_json::Value) -> bool {
    match snapshot {
        serde_json::Value::Object(map) => {
            if map
                .get("used_percent")
                .and_then(|value| value.as_f64())
                .is_some_and(|value| value >= 100.0)
            {
                return true;
            }
            if map
                .get("remaining_percent")
                .and_then(|value| value.as_f64())
                .is_some_and(|value| value <= 0.0)
            {
                return true;
            }
            map.values().any(quota_snapshot_is_exhausted)
        }
        serde_json::Value::Array(values) => values.iter().any(quota_snapshot_is_exhausted),
        _ => false,
    }
}

async fn refresh_quota_snapshots_now(
    state: &AppState,
    provider: Option<&'static str>,
    file_name: Option<&str>,
) -> usize {
    invalidate_quota_cache_for_provider(state, provider, file_name);
    let providers = provider
        .map(|provider| vec![provider])
        .unwrap_or_else(|| MODEL_PROVIDER_PREFIXES.to_vec());
    let results = futures_util::future::join_all(
        providers
            .into_iter()
            .map(|provider| refresh_quota_snapshot_for_provider(state, provider)),
    )
    .await;

    let mut refreshed = 0usize;
    for outcome in results.into_iter().flatten() {
        refreshed += outcome.accounts.len();
        state
            .quota_snapshots
            .lock()
            .unwrap()
            .insert(outcome.snapshot_key.to_string(), outcome.accounts.clone());
        notifications::notify_model_quota_transitions(
            state,
            outcome.notify_provider,
            outcome.label,
            &outcome.accounts,
            notification_account_options(state),
            &now_rfc3339(),
        );
    }
    refreshed
}

struct QuotaRefreshOutcome {
    snapshot_key: &'static str,
    notify_provider: &'static str,
    label: &'static str,
    accounts: Vec<serde_json::Value>,
}

async fn refresh_quota_snapshot_for_provider(
    state: &AppState,
    provider: &'static str,
) -> Option<QuotaRefreshOutcome> {
    let outcome = match provider {
        "cod" => QuotaRefreshOutcome {
            snapshot_key: "codex",
            notify_provider: "codex",
            label: "Codex",
            accounts: target::codex::quota::get_quota_summaries(state).await,
        },
        "agw" => QuotaRefreshOutcome {
            snapshot_key: "antigravity",
            notify_provider: "antigravity",
            label: "Antigravity",
            accounts: target::antigravity::quota::get_quota_summaries(state).await,
        },
        "gem" => QuotaRefreshOutcome {
            snapshot_key: "gemini",
            notify_provider: "gemini",
            label: "Gemini",
            accounts: target::gemini::quota::get_quota_summaries(state).await,
        },
        "qwn" => QuotaRefreshOutcome {
            snapshot_key: "qwen",
            notify_provider: "qwen",
            label: "Qwen",
            accounts: target::qwen::quota::get_quota_summaries(state).await,
        },
        "dsk" => QuotaRefreshOutcome {
            snapshot_key: "deepseek",
            notify_provider: "deepseek",
            label: "DeepSeek",
            accounts: target::deepseek::quota::get_quota_summaries(state).await,
        },
        "min" => QuotaRefreshOutcome {
            snapshot_key: "minimax",
            notify_provider: "minimax",
            label: "MiniMax",
            accounts: target::minimax::quota::get_quota_summaries(state).await,
        },
        "cop" => QuotaRefreshOutcome {
            snapshot_key: "copilot",
            notify_provider: "copilot",
            label: "GitHub Copilot",
            accounts: target::copilot::admin::quota_accounts(state).await,
        },
        "cld" => QuotaRefreshOutcome {
            snapshot_key: "claude",
            notify_provider: "claude",
            label: "Claude",
            accounts: target::claude::quota::get_quota_summaries(state).await,
        },
        "glm" => QuotaRefreshOutcome {
            snapshot_key: "glm",
            notify_provider: "glm",
            label: "GLM",
            accounts: target::glm::quota::get_quota_summaries(state).await,
        },
        // Grok quota is populated from real request headers. Do not issue
        // paid probes from manual or background dashboard refresh.
        "grk" => return None,
        _ => return None,
    };
    Some(outcome)
}

fn invalidate_quota_cache_for_provider(
    state: &AppState,
    provider: Option<&'static str>,
    file_name: Option<&str>,
) {
    let providers = provider
        .map(|provider| vec![provider])
        .unwrap_or_else(|| MODEL_PROVIDER_PREFIXES.to_vec());
    for provider in providers {
        match provider {
            "cod" => {
                let mut cache = state.quota_cache.lock().unwrap();
                if let Some(file_name) = file_name {
                    let tokens = state.tokens.lock().unwrap();
                    for (idx, token) in tokens.iter().enumerate() {
                        if token.file_name.as_deref() == Some(file_name) && idx < cache.len() {
                            cache[idx] = None;
                        }
                    }
                } else {
                    cache.clear();
                }
            }
            "agw" => invalidate_named_quota_cache(&state.agw_quota_cache, file_name),
            "gem" => invalidate_named_quota_cache(&state.gemini_quota_cache, file_name),
            "qwn" => invalidate_named_quota_cache(&state.qwen_quota_cache, file_name),
            "dsk" => invalidate_named_quota_cache(&state.deepseek_quota_cache, file_name),
            "min" => invalidate_named_quota_cache(&state.minimax_quota_cache, file_name),
            "cop" => target::copilot::admin::invalidate_quota_cache(file_name),
            "cld" => invalidate_named_quota_cache(&state.claude_quota_cache, file_name),
            "glm" => invalidate_named_quota_cache(&state.glm_quota_cache, file_name),
            "grk" => {}
            _ => {}
        }
    }
}

fn invalidate_named_quota_cache<T>(
    cache: &Arc<Mutex<HashMap<String, T>>>,
    file_name: Option<&str>,
) {
    let mut cache = cache.lock().unwrap();
    if let Some(file_name) = file_name {
        cache.remove(file_name);
    } else {
        cache.clear();
    }
}

async fn background_maintenance(state: AppState) {
    const OAUTH_STATE_TTL: Duration = Duration::from_secs(15 * 60);
    let mut interval = tokio::time::interval(Duration::from_secs(60));
    loop {
        interval.tick().await;
        state
            .oauth_pending
            .lock()
            .unwrap()
            .retain(|_, pending| pending.created_at.elapsed() < OAUTH_STATE_TTL);
        state
            .agw_oauth_pending
            .lock()
            .unwrap()
            .retain(|_, pending| pending.created_at.elapsed() < OAUTH_STATE_TTL);
        state
            .gemini_oauth_pending
            .lock()
            .unwrap()
            .retain(|_, created_at| created_at.elapsed() < OAUTH_STATE_TTL);
        state
            .qwen_oauth_pending
            .lock()
            .unwrap()
            .retain(|_, pending| pending.created_at.elapsed() < OAUTH_STATE_TTL);
        state
            .grok_oauth_pending
            .lock()
            .unwrap()
            .retain(|_, pending| pending.created_at.elapsed() < OAUTH_STATE_TTL);
        state
            .claude_oauth_pending
            .lock()
            .unwrap()
            .retain(|_, pending| pending.created_at.elapsed() < OAUTH_STATE_TTL);
        let now_unix = chrono::Utc::now().timestamp();
        state
            .copilot_oauth_pending
            .lock()
            .unwrap()
            .retain(|_, pending| pending.expires_at_unix > now_unix);
        cleanup_generated_temp_files(Duration::from_secs(60 * 60));
    }
}

fn cleanup_generated_temp_files(max_age: Duration) {
    let dir = generated_temp_download_dir();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };
    let now = std::time::SystemTime::now();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let expired = entry
            .metadata()
            .ok()
            .and_then(|metadata| metadata.modified().ok())
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age >= max_age);
        if expired {
            let _ = std::fs::remove_file(path);
        }
    }
}

async fn favicon_route() -> impl IntoResponse {
    StatusCode::NO_CONTENT
}

/// Serves the HTML dashboard at the root path.
#[utoipa::path(
    get,
    path = "/",
    responses((status = 200, description = "Dashboard HTML", body = String))
)]
async fn dashboard_root() -> impl IntoResponse {
    dashboard().await
}

/// Serves the HTML dashboard page.
#[utoipa::path(
    get,
    path = "/dashboard",
    responses((status = 200, description = "Dashboard HTML", body = String))
)]
async fn dashboard() -> impl IntoResponse {
    let html = r###"<!doctype html>
<html>
  <head>
    <meta charset="utf-8">
    <meta name="viewport" content="width=device-width, initial-scale=1">
    <title>IO Gateway Dashboard</title>
    <style>
      :root {
        color-scheme: dark;
        --bg: #09111f;
        --surface: #111b2e;
        --surface-alt: #182338;
        --surface-raised: #0f1729;
        --border: #25314a;
        --text: #ecf2ff;
        --muted: #94a3b8;
        --button-bg: #3b82f6;
        --button-hover: #2563eb;
        --button-text: #eff6ff;
        --secondary-bg: #182338;
        --secondary-hover: #22304a;
        --secondary-text: #dbe7ff;
        --code-bg: rgba(15, 23, 42, 0.85);
        --overlay: rgba(2, 6, 23, 0.72);
        --row-hover: rgba(148, 163, 184, 0.08);
        --tip-bg: #e2e8f0;
        --tip-text: #0f172a;
        --shadow: 0 18px 40px rgba(2, 6, 23, 0.34);
        --success: #22c55e;
        --warning: #f59e0b;
        --danger: #ef4444;
        --info: #3b82f6;
      }
      :root[data-theme="light"] {
        color-scheme: light;
        --bg: #f3f6fb;
        --surface: #ffffff;
        --surface-alt: #eef2f7;
        --surface-raised: #f8fafc;
        --border: #d5ddeb;
        --text: #111827;
        --muted: #5b6474;
        --button-bg: #111827;
        --button-hover: #1f2937;
        --button-text: #f9fafb;
        --secondary-bg: #ffffff;
        --secondary-hover: #eef2f7;
        --secondary-text: #111827;
        --code-bg: #edf2f8;
        --overlay: rgba(15, 23, 42, 0.45);
        --row-hover: rgba(15, 23, 42, 0.05);
        --tip-bg: #111827;
        --tip-text: #f8fafc;
        --shadow: 0 18px 40px rgba(15, 23, 42, 0.12);
      }
      * { box-sizing: border-box; }
      [hidden] { display: none !important; }
      body {
        font-family: Inter, ui-sans-serif, system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif;
        margin: 0;
        font-size: 14px;
        min-height: 100vh;
        background: var(--bg);
        color: var(--text);
      }
      .page-shell { padding: 24px; }
      .sr-only {
        position: absolute;
        width: 1px;
        height: 1px;
        padding: 0;
        margin: -1px;
        overflow: hidden;
        clip: rect(0, 0, 0, 0);
        white-space: nowrap;
        border: 0;
      }
      h1 { margin: 0; font-size: 28px; line-height: 1.15; letter-spacing: 0; }
      h2 { margin: 0; }
      table { border-collapse: collapse; width: 100%; background: var(--surface); }
      th, td { border: 1px solid var(--border); padding: 8px; text-align: left; }
      th { background: var(--surface-alt); }
      code, pre {
        background: var(--code-bg);
        color: var(--text);
      }
      code {
        padding: 2px 6px;
        border-radius: 6px;
      }
      pre {
        padding: 10px 12px;
        border-radius: 10px;
        border: 1px solid var(--border);
      }
      button, input, textarea, select {
        font-size: 14px;
        font-family: inherit;
      }
      button {
        border: 1px solid transparent;
        background: var(--button-bg);
        color: var(--button-text);
        padding: 9px 14px;
        min-height: 38px;
        border-radius: 8px;
        cursor: pointer;
      }
      button:hover {
        background: var(--button-hover);
      }
      button:disabled,
      button:disabled:hover {
        background: var(--secondary-bg);
        color: var(--muted);
        border-color: var(--border);
        cursor: not-allowed;
        opacity: 0.7;
      }
      input, textarea, select {
        width: min(100%, 560px);
        padding: 10px 12px;
        border-radius: 8px;
        border: 1px solid var(--border);
        background: var(--surface);
        color: var(--text);
      }
      textarea {
        resize: vertical;
        line-height: 1.45;
        min-height: 108px;
      }
      input::placeholder,
      textarea::placeholder { color: var(--muted); }
      label {
        display: block;
        font-weight: 600;
      }
      .page-header {
        display: flex;
        align-items: flex-start;
        justify-content: space-between;
        gap: 16px;
        margin-bottom: 14px;
      }
      .page-title-block {
        min-width: 0;
      }
      .page-subtitle {
        margin-top: 8px;
      }
      .section-header {
        display: flex;
        align-items: center;
        justify-content: space-between;
        gap: 12px;
      }
      .section-header { margin-bottom: 8px; }
      .header-actions,
      .modal-actions {
        display: flex;
        align-items: center;
        gap: 8px;
        flex-wrap: wrap;
      }
      .mobile-menu-button {
        display: none;
      }
      .hamburger-icon {
        display: inline-flex;
        width: 22px;
        flex-direction: column;
        gap: 5px;
      }
      .hamburger-icon span {
        display: block;
        height: 2px;
        border-radius: 999px;
        background: currentColor;
      }
      .page-header.mobile-nav-open .hamburger-icon span:nth-child(1) {
        transform: translateY(7px) rotate(45deg);
      }
      .page-header.mobile-nav-open .hamburger-icon span:nth-child(2) {
        opacity: 0;
      }
      .page-header.mobile-nav-open .hamburger-icon span:nth-child(3) {
        transform: translateY(-7px) rotate(-45deg);
      }
      .provider-menu-wrap {
        position: relative;
      }
      .provider-menu {
        position: absolute;
        right: 0;
        top: calc(100% + 4px);
        z-index: 100;
        min-width: 220px;
        overflow: hidden;
        border: 1px solid var(--border);
        border-radius: 8px;
        background: var(--surface);
        box-shadow: var(--shadow);
      }
      .provider-menu-title {
        display: none;
        padding: 10px 14px;
        border-bottom: 1px solid var(--border);
        color: var(--muted);
        font-size: 12px;
        font-weight: 700;
        text-transform: uppercase;
      }
      .provider-menu-item {
        display: block;
        width: 100%;
        min-height: 40px;
        padding: 10px 14px;
        border: 0;
        border-bottom: 1px solid var(--border);
        border-radius: 0;
        background: transparent;
        color: var(--text);
        text-align: left;
      }
      .provider-menu-item:last-child { border-bottom: 0; }
      .provider-menu-item:hover,
      .provider-menu-item:focus { background: var(--row-hover); }
      .provider-settings-list {
        display: grid;
        gap: 8px;
        margin-top: 10px;
      }
      .provider-settings-row {
        display: grid;
        grid-template-columns: minmax(0, 1fr) auto auto;
        gap: 10px;
        align-items: center;
        padding: 8px;
        border: 1px solid var(--border);
        border-radius: 8px;
        background: var(--surface-alt);
      }
      .provider-settings-row.is-hidden {
        opacity: 0.62;
      }
      .provider-settings-visible {
        min-width: 0;
        overflow-wrap: anywhere;
      }
      .provider-settings-key {
        color: var(--muted);
        font-size: 12px;
      }
      .provider-settings-actions {
        display: flex;
        gap: 6px;
        justify-content: flex-end;
      }
      .provider-settings-actions .mini-btn {
        min-width: 34px;
      }
      .provider-settings-actions .mini-btn:disabled {
        cursor: not-allowed;
        opacity: 0.45;
      }
      .settings-segmented {
        display: inline-flex;
        flex-wrap: wrap;
        gap: 4px;
        padding: 4px;
        border: 1px solid var(--border);
        border-radius: 8px;
        background: var(--surface-alt);
      }
      .settings-segmented button {
        min-height: 36px;
        padding: 8px 12px;
        border: 0;
        border-radius: 6px;
        background: transparent;
        color: var(--muted);
      }
      .settings-segmented button:hover {
        background: rgba(148, 163, 184, 0.12);
        color: var(--text);
      }
      .settings-segmented button.is-active {
        background: var(--surface);
        color: var(--text);
        box-shadow: var(--shadow);
      }
      .settings-help {
        color: var(--muted);
        font-size: 12px;
        line-height: 1.4;
      }
      .settings-block {
        margin-top: 18px;
        padding-top: 16px;
        border-top: 1px solid var(--border);
      }
      .settings-block:first-of-type {
        margin-top: 0;
        padding-top: 0;
        border-top: 0;
      }
      .settings-block-title {
        display: flex;
        align-items: center;
        justify-content: space-between;
        gap: 10px;
        margin-bottom: 10px;
        font-weight: 700;
      }
      .settings-tabs {
        display: grid;
        grid-template-columns: repeat(4, minmax(0, 1fr));
        gap: 8px;
        margin: 10px 0 14px 0;
        border-bottom: 1px solid var(--border);
        overflow: visible;
      }
      .settings-tab {
        border: 0;
        border-bottom: 2px solid transparent;
        border-radius: 0;
        background: transparent;
        color: var(--muted);
        min-width: 0;
        padding: 9px 10px;
        white-space: normal;
        overflow-wrap: anywhere;
      }
      .settings-tab:hover {
        background: var(--surface-alt);
        color: var(--text);
      }
      .settings-tab.is-active {
        border-bottom-color: var(--accent);
        color: var(--text);
      }
      .settings-panel[hidden] {
        display: none;
      }
      .api-key-create-row {
        display: grid;
        grid-template-columns: minmax(0, 1fr) auto;
        gap: 8px;
        align-items: center;
      }
      .api-key-create-actions {
        display: flex;
        align-items: center;
        gap: 8px;
      }
      .api-key-access-editor {
        display: grid;
        gap: 8px;
        margin-top: 10px;
      }
      .api-key-access-groups {
        display: grid;
        grid-template-columns: repeat(2, minmax(0, 1fr));
        gap: 8px;
      }
      .api-key-access-provider {
        min-width: 0;
        padding: 9px;
        border: 1px solid var(--border);
        border-radius: 8px;
        background: var(--surface-alt);
      }
      .api-key-access-provider > .check-row {
        font-weight: 700;
      }
      .api-key-limit-row {
        display: grid;
        grid-template-columns: minmax(0, 1fr) minmax(120px, 160px);
        gap: 8px;
        align-items: center;
      }
      .api-key-limit-row input {
        min-width: 0;
      }
      .api-key-access-accounts {
        display: grid;
        gap: 6px;
        margin-top: 8px;
      }
      .api-key-access-account {
        display: grid;
        grid-template-columns: auto minmax(0, 1fr) minmax(110px, 140px);
        gap: 6px;
        min-width: 0;
        align-items: flex-start;
      }
      .api-key-account-limit-input {
        min-width: 0;
      }
      .account-choice-text,
      .api-key-access-account span {
        min-width: 0;
        overflow-wrap: anywhere;
      }
      .account-choice-title {
        display: block;
        color: var(--text);
      }
      .account-choice-meta,
      .api-key-access-account small {
        display: block;
        color: var(--muted);
        font-weight: 400;
        line-height: 1.35;
      }
      .api-key-access-summary {
        color: var(--text);
      }
      .api-key-list {
        display: grid;
        gap: 8px;
        margin-top: 12px;
      }
      .api-key-row {
        display: grid;
        grid-template-columns: minmax(0, 1fr) auto;
        gap: 10px;
        align-items: center;
        padding: 10px;
        border: 1px solid var(--border);
        border-radius: 8px;
        background: var(--surface-alt);
      }
      .api-key-row.is-revoked {
        opacity: 0.72;
      }
      .api-key-main {
        min-width: 0;
        display: grid;
        gap: 4px;
      }
      .api-key-title-row {
        display: flex;
        align-items: center;
        gap: 8px;
        flex-wrap: wrap;
        min-width: 0;
      }
      .api-key-title-row code {
        overflow-wrap: anywhere;
      }
      .api-key-meta {
        color: var(--muted);
        font-size: 12px;
        line-height: 1.4;
        overflow-wrap: anywhere;
      }
      .api-key-actions {
        display: flex;
        align-items: center;
        justify-content: flex-end;
        gap: 6px;
        flex-wrap: wrap;
      }
      .api-key-reveal {
        display: grid;
        gap: 8px;
        margin-top: 12px;
        padding: 10px;
        border: 1px solid var(--border);
        border-radius: 8px;
        background: var(--surface-alt);
      }
      .api-key-reveal-header {
        display: flex;
        align-items: center;
        justify-content: space-between;
        gap: 8px;
      }
      .api-key-reveal code {
        display: block;
        width: 100%;
        padding: 10px;
        border-radius: 8px;
        background: var(--surface);
        border: 1px solid var(--border);
        overflow-wrap: anywhere;
      }
      .test-api-grid {
        display: grid;
        grid-template-columns: minmax(0, 1fr) minmax(180px, 0.35fr);
        gap: 12px;
        align-items: start;
      }
      .test-api-grid input,
      .test-api-grid textarea,
      .test-api-grid select,
      .test-api-model-input {
        width: 100%;
        max-width: none;
      }
      .test-api-side {
        display: grid;
        gap: 10px;
      }
      .test-api-result {
        display: grid;
        gap: 10px;
        margin-top: 12px;
        padding: 10px;
        border: 1px solid var(--border);
        border-radius: 8px;
        background: var(--surface-alt);
      }
      .test-api-result-head {
        display: flex;
        align-items: center;
        justify-content: space-between;
        gap: 8px;
        flex-wrap: wrap;
      }
      .test-api-output,
      .test-api-raw {
        width: 100%;
        max-height: min(34vh, 360px);
        overflow: auto;
        white-space: pre-wrap;
        overflow-wrap: anywhere;
      }
      .test-api-result details summary {
        cursor: pointer;
        color: var(--muted);
        font-size: 12px;
        font-weight: 700;
      }
      .notification-channel-grid {
        display: grid;
        grid-template-columns: repeat(2, minmax(0, 1fr));
        gap: 12px;
      }
      .notification-watch-toolbar {
        display: flex;
        flex-wrap: wrap;
        gap: 8px;
        margin: 10px 0;
      }
      .notification-watch-list {
        display: grid;
        gap: 10px;
        max-height: min(46vh, 520px);
        overflow: auto;
        padding-right: 4px;
      }
      .notification-provider-group {
        border: 1px solid var(--border);
        border-radius: 8px;
        background: var(--surface-alt);
        overflow: hidden;
      }
      .notification-provider-head {
        display: grid;
        grid-template-columns: minmax(0, 1fr) auto;
        gap: 8px;
        align-items: center;
        padding: 8px;
        border-bottom: 1px solid var(--border);
      }
      .notification-provider-actions {
        display: flex;
        gap: 6px;
        justify-content: flex-end;
      }
      .notification-account-row {
        display: block;
        padding: 8px;
        border-bottom: 1px solid var(--border);
      }
      .notification-account-row .check-row {
        display: grid;
        grid-template-columns: auto minmax(0, 1fr);
        align-items: flex-start;
        width: 100%;
      }
      .notification-account-row:last-child {
        border-bottom: 0;
      }
      .notification-account-row.is-disabled {
        opacity: 0.62;
      }
      .notification-account-meta {
        display: block;
        margin-top: 2px;
        color: var(--muted);
        font-size: 12px;
        overflow-wrap: anywhere;
      }
      .section { margin-top: 28px; }
      .table-wrap {
        overflow-x: auto;
        border: 1px solid var(--border);
        border-radius: 14px;
        box-shadow: var(--shadow);
        background: var(--surface);
      }
      .muted { color: var(--muted); font-size: 12px; }
      .stacked { line-height: 1.35; white-space: nowrap; }
      .weekly-line { font-size: calc(1em - 4px); }
      .row-label {
        display: flex;
        align-items: center;
        gap: 6px;
        width: 100%;
      }
      .count-pill,
      .expander {
        color: var(--muted);
        font-size: 12px;
      }
      .count-pill { margin-left: auto; }
      .expander {
        min-width: 12px;
        text-align: center;
      }
      .help-trigger { cursor: help; }
      .secondary-button {
        background: var(--secondary-bg);
        color: var(--secondary-text);
        border-color: var(--border);
      }
      .secondary-button:hover { background: var(--secondary-hover); }
      .dot-button,
      .dot-indicator {
        display: inline-block;
        width: 10px;
        height: 10px;
        border-radius: 50%;
      }
      .dot-button {
        border: none;
        padding: 0;
        min-width: 10px;
      }
      .dot-button:hover {
        opacity: 0.85;
      }
      .icon-button {
        border: none;
        background: transparent;
        color: var(--muted);
        padding: 0 0 0 4px;
        line-height: 1;
      }
      .icon-button:hover {
        background: transparent;
        color: var(--text);
      }
      .clickable-row { cursor: pointer; }
      .clickable-row:hover { background: var(--row-hover); }
      .detail-row > td { padding: 0; }
      .detail-panel {
        padding: 12px 10px 14px 10px;
        background: var(--surface-raised);
      }
      .detail-table-wrap {
        overflow-x: auto;
        margin-top: 8px;
        width: 100%;
      }
      .detail-table {
        width: 100%;
        min-width: 560px;
      }
      .modal {
        position: fixed;
        inset: 0;
        z-index: 1000;
        background: var(--overlay);
        padding: 24px 12px;
      }
      #adminLoginGate {
        z-index: 1100;
      }
      .modal-card {
        position: relative;
        background: var(--surface);
        max-width: 720px;
        margin: 8% auto;
        padding: 16px;
        border-radius: 8px;
        max-height: 80vh;
        overflow: auto;
        border: 1px solid var(--border);
        box-shadow: var(--shadow);
      }
      .modal-close-button {
        position: sticky;
        top: 0;
        float: right;
        z-index: 5;
        min-width: 42px;
        width: 42px;
        min-height: 42px;
        margin: -6px -6px 4px 8px;
        padding: 0;
        border-radius: 8px;
        background: var(--surface-alt);
        color: var(--text);
        font-size: 24px;
        line-height: 1;
      }
      .modal-card > .modal-actions:last-child,
      .modal-card form > .modal-actions:last-child {
        position: sticky;
        bottom: -16px;
        z-index: 4;
        margin-right: -16px;
        margin-left: -16px;
        padding: 10px 16px 2px;
        border-top: 1px solid var(--border);
        background: var(--surface);
      }
      .auth-url {
        display: none;
        white-space: pre-wrap;
        word-break: break-all;
        overflow-wrap: anywhere;
      }
      .tap-tip {
        position: absolute;
        background: var(--tip-bg);
        color: var(--tip-text);
        border-radius: 6px;
        padding: 6px 8px;
        font-size: 12px;
        line-height: 1.25;
        white-space: nowrap;
        z-index: 9999;
        box-shadow: 0 4px 12px rgba(0,0,0,0.25);
      }
      .overview-grid {
        display: grid;
        grid-template-columns: repeat(4, minmax(0, 1fr));
        gap: 12px;
        margin: 14px 0;
      }
      .overview-card {
        min-width: 0;
        padding: 14px;
        border: 1px solid var(--border);
        border-radius: 8px;
        background: var(--surface);
        box-shadow: var(--shadow);
      }
      .overview-label {
        color: var(--muted);
        font-size: 12px;
        line-height: 1.2;
      }
      .overview-value {
        margin-top: 6px;
        font-size: 24px;
        font-weight: 800;
        line-height: 1;
      }
      .overview-note {
        margin-top: 6px;
        color: var(--muted);
        font-size: 12px;
        white-space: nowrap;
        overflow: hidden;
        text-overflow: ellipsis;
      }
      .chart-section {
        margin: 14px 0 18px 0;
        background: var(--surface);
        border: 1px solid var(--border);
        border-radius: 8px;
        padding: 14px;
        box-shadow: var(--shadow);
      }
      .chart-section:not([open]) {
        padding-bottom: 14px;
      }
      .chart-section summary {
        list-style: none;
      }
      .chart-section summary::-webkit-details-marker {
        display: none;
      }
      .chart-header {
        display: flex;
        align-items: center;
        justify-content: space-between;
        gap: 12px;
        cursor: pointer;
      }
      .chart-section[open] .chart-header { margin-bottom: 8px; }
      .chart-title-row {
        display: flex;
        align-items: center;
        gap: 8px;
        flex-wrap: wrap;
        min-width: 0;
      }
      .chart-title-row h2 {
        font-size: 17px;
        line-height: 1.2;
      }
      .chart-toggle-hint {
        display: inline-flex;
        width: 12px;
        height: 12px;
        flex: 0 0 auto;
        align-items: center;
        justify-content: center;
        color: var(--muted);
      }
      .chart-toggle-hint::before {
        content: "";
        display: block;
        width: 7px;
        height: 7px;
        border-right: 2px solid currentColor;
        border-bottom: 2px solid currentColor;
        transform: rotate(45deg);
        transition: transform 0.16s ease;
      }
      .chart-section:not([open]) .chart-toggle-hint::before {
        transform: rotate(-45deg);
      }
      .chart-summary {
        color: var(--muted);
        font-size: 12px;
        line-height: 1.35;
      }
      .chart-controls {
        display: flex;
        align-items: flex-end;
        gap: 8px;
        flex-wrap: wrap;
        margin: 6px 0 10px 0;
      }
      .chart-field {
        display: flex;
        flex-direction: column;
        gap: 4px;
        color: var(--muted);
        font-size: 12px;
      }
      .chart-field select,
      .chart-field input {
        width: auto;
        min-width: 110px;
      }
      .chart-custom-controls {
        display: flex;
        align-items: flex-end;
        gap: 8px;
        flex-wrap: wrap;
      }
      .chart-legend {
        display: flex;
        gap: 10px;
        flex-wrap: wrap;
        justify-content: flex-end;
        font-size: 12px;
      }
      .chart-legend-item {
        display: flex;
        align-items: center;
        gap: 4px;
        cursor: pointer;
        user-select: none;
      }
      .chart-legend-dot {
        display: inline-block;
        width: 10px;
        height: 10px;
        border-radius: 2px;
      }
      .chart-wrap { position: relative; height: 220px; }
      .providers-grid {
        display: grid;
        grid-template-columns: repeat(auto-fit, minmax(390px, 1fr));
        gap: 16px;
        align-items: start;
      }
      .providers-grid.provider-layout-row {
        grid-template-columns: repeat(var(--provider-row-capacity, 1), minmax(0, 1fr));
      }
      .providers-grid.provider-layout-single {
        grid-template-columns: 1fr;
      }
      .providers-grid.provider-layout-row .provider-cards,
      .providers-grid.provider-layout-single .provider-cards {
        display: grid;
        grid-template-columns: repeat(auto-fit, minmax(390px, 390px));
        gap: 12px;
        justify-content: flex-start;
        align-items: start;
      }
      .providers-grid.provider-layout-row .provider-cards .card,
      .providers-grid.provider-layout-single .provider-cards .card {
        margin-bottom: 0;
      }
      .custom-model-section {
        margin: 14px 0 18px 0;
      }
      .custom-model-header {
        display: flex;
        align-items: center;
        justify-content: space-between;
        gap: 12px;
        margin-bottom: 10px;
      }
      .custom-model-header h2 {
        margin: 0;
        font-size: 17px;
        line-height: 1.2;
      }
      .custom-model-grid {
        display: grid;
        grid-template-columns: repeat(auto-fit, minmax(300px, 1fr));
        gap: 12px;
      }
      .custom-model-card {
        margin-bottom: 0;
      }
      .custom-model-route-list {
        display: grid;
        gap: 8px;
        margin-top: 10px;
      }
      .custom-model-route-row {
        display: grid;
        grid-template-columns: 82px minmax(0, 1fr);
        gap: 8px;
        align-items: start;
      }
      .custom-model-route-label {
        color: var(--muted);
        font-size: 12px;
        font-weight: 700;
        line-height: 1.8;
      }
      .custom-model-targets {
        display: flex;
        flex-wrap: wrap;
        gap: 6px;
        min-width: 0;
      }
      .custom-model-chip {
        display: inline-flex;
        align-items: center;
        min-height: 26px;
        max-width: 100%;
        padding: 3px 8px;
        border: 1px solid var(--border);
        border-radius: 8px;
        background: var(--surface-alt);
        color: var(--text);
        font-size: 12px;
        overflow-wrap: anywhere;
      }
      .custom-model-form {
        display: grid;
        gap: 12px;
        min-width: 0;
      }
      .custom-model-form-row {
        display: grid;
        gap: 6px;
        min-width: 0;
      }
      .custom-model-account-provider {
        display: grid;
        gap: 6px;
      }
      .custom-model-account-row {
        display: grid;
        grid-template-columns: minmax(0, 1fr) auto;
        gap: 8px;
        align-items: center;
      }
      .custom-model-account-row code {
        overflow-wrap: anywhere;
      }
      .custom-model-editor {
        display: grid;
        gap: 12px;
        min-width: 0;
      }
      .custom-model-steps {
        display: grid;
        gap: 12px;
        min-width: 0;
      }
      .custom-model-step {
        border: 1px solid var(--border);
        border-radius: 10px;
        background: var(--surface);
        padding: 12px;
        display: grid;
        gap: 10px;
        min-width: 0;
      }
      .custom-model-step-header {
        display: flex;
        align-items: center;
        gap: 10px;
        flex-wrap: wrap;
      }
      .custom-model-step-index {
        font-weight: 800;
        font-size: 13px;
        color: var(--text);
      }
      .custom-model-step-summary {
        color: var(--muted);
        font-size: 12px;
        flex: 1 1 auto;
        min-width: 0;
      }
      .custom-model-step-toolbar {
        display: flex;
        gap: 6px;
        justify-content: flex-end;
      }
      .custom-model-step-targets {
        display: grid;
        gap: 8px;
        min-width: 0;
      }
      .custom-model-target {
        display: grid;
        grid-template-columns: minmax(110px, 0.75fr) minmax(150px, 1.25fr) minmax(120px, 0.85fr) minmax(150px, 1fr) minmax(96px, auto) auto auto;
        gap: 8px;
        align-items: center;
        padding: 8px;
        border: 1px solid var(--border);
        border-radius: 8px;
        background: var(--surface-alt);
        position: relative;
        min-width: 0;
      }
      .custom-model-target.disabled {
        opacity: 0.55;
      }
      .custom-model-target > * {
        min-width: 0;
      }
      .custom-model-target select,
      .custom-model-target input {
        min-height: 32px;
        width: 100%;
      }
      .custom-model-target-share {
        display: inline-grid;
        grid-template-columns: auto minmax(54px, 1fr);
        gap: 6px;
        align-items: center;
        min-height: 32px;
        padding: 0 8px;
        border: 1px solid var(--border);
        border-radius: 8px;
        background: var(--surface);
        color: var(--muted);
        font-size: 12px;
        white-space: nowrap;
      }
      .custom-model-target-share[hidden] {
        display: none;
      }
      .custom-model-target-share span {
        font-weight: 700;
      }
      .custom-model-target-share select {
        min-height: 28px;
        padding: 2px 6px;
      }
      .custom-model-target-toolbar {
        display: flex;
        gap: 6px;
        justify-content: flex-end;
      }
      .custom-model-step-footer {
        display: flex;
        gap: 8px;
        justify-content: flex-start;
      }
      .custom-model-account-picker-popover {
        position: absolute;
        top: calc(100% + 4px);
        left: 0;
        right: 0;
        z-index: 20;
        background: var(--surface);
        border: 1px solid var(--border);
        border-radius: 8px;
        box-shadow: var(--shadow);
        padding: 8px;
        max-height: 260px;
        overflow: auto;
        display: grid;
        gap: 8px;
      }
      .custom-model-account-picker-head {
        display: flex;
        gap: 6px;
        justify-content: space-between;
        border-bottom: 1px solid var(--border);
        padding-bottom: 6px;
      }
      .custom-model-account-picker-head .mini-btn {
        flex: 1;
      }
      .custom-model-field-error {
        color: #f87171;
        font-size: 12px;
        font-weight: 600;
        min-height: 16px;
      }
      .custom-model-preview-wrap {
        border: 1px dashed var(--border);
        border-radius: 8px;
        padding: 10px;
        background: var(--surface-alt);
        min-height: 80px;
        min-width: 0;
      }
      .custom-model-preview-empty {
        color: var(--muted);
        font-size: 12px;
        text-align: center;
        padding: 16px;
      }
      .custom-model-modal-card {
        max-width: min(1120px, calc(100vw - 32px));
      }
      @media (max-width: 1120px) {
        .custom-model-target {
          grid-template-columns: repeat(2, minmax(0, 1fr));
        }
        .custom-model-target-share,
        .custom-model-target-toolbar {
          justify-self: start;
        }
        .custom-model-target-toolbar {
          grid-column: 1 / -1;
        }
      }
      @media (max-width: 760px) {
        .custom-model-target {
          grid-template-columns: 1fr;
        }
        .custom-model-account-picker-popover {
          position: static;
        }
      }
      .inline-checks {
        display: flex;
        gap: 12px;
        flex-wrap: wrap;
      }
      .check-row {
        display: inline-flex;
        align-items: center;
        gap: 7px;
        color: var(--text);
        font-weight: 700;
      }
      .check-row input {
        width: auto;
      }
      .prefixed-input {
        display: flex;
        align-items: center;
      }
      .prefixed-input span {
        display: inline-flex;
        align-items: center;
        align-self: stretch;
        padding: 0 10px;
        border: 1px solid var(--border);
        border-right: 0;
        border-radius: 8px 0 0 8px;
        background: var(--surface-alt);
        color: var(--muted);
        font-size: 13px;
        font-weight: 700;
      }
      .prefixed-input input {
        border-radius: 0 8px 8px 0;
      }
      .card {
        background: var(--surface);
        border: 1px solid var(--border);
        border-radius: 8px;
        box-shadow: var(--shadow);
        padding: 14px;
        margin-bottom: 12px;
        transition: border-color 0.2s;
      }
      .card:hover { border-color: var(--muted); }
      .card-header {
        display: flex;
        align-items: flex-start;
        gap: 10px;
        margin-bottom: 10px;
        flex-wrap: nowrap;
      }
      .card-identity {
        min-width: 0;
        flex: 1 1 auto;
        display: flex;
        flex-direction: column;
        align-items: flex-start;
        gap: 4px;
      }
      .card-email {
        min-width: 0;
        overflow-wrap: anywhere;
        font-weight: 800;
        font-size: 14px;
        line-height: 1.2;
      }
      .card-badges {
        display: flex;
        flex-wrap: wrap;
        align-items: center;
        gap: 6px;
      }
      .card-actions {
        margin-left: auto;
        display: flex;
        gap: 6px;
        align-items: center;
        flex: 0 0 auto;
        flex-wrap: nowrap;
        position: relative;
        align-self: flex-start;
      }
      .account-state {
        display: inline-flex;
        align-items: center;
        gap: 5px;
        min-height: 24px;
        padding: 3px 8px;
        border-radius: 999px;
        border: 1px solid var(--border);
        background: var(--surface-alt);
        color: var(--muted);
        font-size: 12px;
        font-weight: 700;
      }
      .account-state::before {
        content: "";
        display: inline-block;
        width: 8px;
        height: 8px;
        border-radius: 50%;
        background: var(--muted);
      }
      .account-state.enabled {
        color: var(--success);
        border-color: rgba(34, 197, 94, 0.35);
      }
      .account-state.enabled::before { background: var(--success); }
      .account-state.cooldown {
        color: var(--warning);
        border-color: rgba(245, 158, 11, 0.5);
        background: rgba(245, 158, 11, 0.1);
      }
      .account-state.cooldown::before { background: var(--warning); }
      .account-state.disabled {
        color: var(--danger);
        border-color: rgba(239, 68, 68, 0.42);
      }
      .account-state.disabled::before { background: var(--danger); }
      .attention-account-badge {
        display: inline-flex;
        align-items: center;
        gap: 5px;
        min-height: 24px;
        padding: 3px 8px;
        border-radius: 999px;
        border: 1px solid rgba(245, 158, 11, 0.5);
        background: rgba(245, 158, 11, 0.1);
        color: var(--warning);
        font-size: 12px;
        font-weight: 800;
      }
      .attention-account-badge::before {
        content: "";
        display: inline-block;
        width: 8px;
        height: 8px;
        border-radius: 50%;
        background: var(--warning);
      }
      .account-attention-details {
        clear: both;
        margin-top: 8px;
        line-height: 1.45;
      }
      .account-attention-details summary {
        cursor: pointer;
        color: var(--secondary-text);
        font-weight: 700;
      }
      .attention-count {
        color: var(--muted);
      }
      .attention-list {
        display: grid;
        gap: 8px;
        margin-top: 6px;
      }
      .attention-item,
      .attention-empty {
        padding: 8px;
        border: 1px solid var(--border);
        border-radius: 8px;
        background: var(--surface-alt);
      }
      .attention-title {
        color: var(--text);
        font-weight: 700;
        overflow-wrap: anywhere;
      }
      .attention-detail {
        margin-top: 2px;
        color: var(--muted);
        font-size: 12px;
        overflow-wrap: anywhere;
      }
      .stat-pills {
        display: flex;
        gap: 6px;
        flex-wrap: wrap;
        margin-bottom: 10px;
      }
      .stat-pill {
        display: flex;
        align-items: center;
        gap: 5px;
        background: var(--surface-alt);
        border: 1px solid var(--border);
        border-radius: 8px;
        padding: 5px 8px;
        font-size: 12px;
        white-space: nowrap;
      }
      .stat-pill-icon { font-size: 16px; }
      .stat-pill-value { font-weight: 700; color: var(--text); }
      .stat-pill-label { color: var(--muted); }
      .stat-pill-divider {
        display: inline-block;
        width: 1px;
        height: 22px;
        background: var(--border);
        margin: 0 4px;
        align-self: center;
      }
      .quota-mini-pill {
        display: inline-flex;
        align-items: center;
        gap: 4px;
        background: var(--surface-alt);
        border: 1px solid var(--border);
        border-radius: 8px;
        padding: 6px 10px;
        font-size: 12px;
        white-space: nowrap;
      }
      .quota-mini-pill .quota-mini-pill-label { color: var(--muted); }
      .quota-mini-pill .quota-mini-pill-value { font-weight: 700; }
      .quota-mini-pill.low  { border-color: rgba(34,197,94,0.35); }
      .quota-mini-pill.mid  { border-color: rgba(245,158,11,0.35); }
      .quota-mini-pill.high { border-color: rgba(239,68,68,0.45); background: rgba(239,68,68,0.08); }
      .quota-cost-line {
        font-size: 12px;
        margin-top: 4px;
        width: 100%;
      }
      .quota-kind-group {
        flex: 1 1 100%;
        display: flex;
        gap: 8px;
        flex-wrap: wrap;
        margin-bottom: 4px;
      }
      .card-chart-wrap {
        position: relative;
        height: 180px;
        margin-bottom: 14px;
        background: var(--surface-raised);
        border-radius: 8px;
        padding: 8px;
        border: 1px solid var(--border);
      }
      .card-chart-placeholder {
        display: flex;
        align-items: center;
        justify-content: center;
        height: 100%;
        color: var(--muted);
        font-size: 13px;
      }
      .card-quota {
        display: flex;
        gap: 12px 16px;
        flex-wrap: wrap;
        font-size: 13px;
        margin-bottom: 10px;
      }
      .reset-credit-details {
        clear: both;
        margin: 2px 0 10px;
        line-height: 1.45;
      }
      .reset-credit-details summary {
        cursor: pointer;
        color: var(--secondary-text);
        font-weight: 700;
      }
      .reset-credit-count {
        color: var(--muted);
      }
      .reset-credit-list {
        display: grid;
        gap: 8px;
        margin-top: 6px;
      }
      .reset-credit-item {
        display: flex;
        align-items: center;
        justify-content: space-between;
        gap: 8px;
        flex-wrap: wrap;
        padding: 8px;
        border: 1px solid var(--border);
        border-radius: 8px;
        background: var(--surface-alt);
      }
      .reset-credit-main {
        flex: 1 1 220px;
        min-width: 0;
      }
      .reset-credit-title {
        color: var(--text);
        font-weight: 700;
        overflow-wrap: anywhere;
      }
      .reset-credit-meta {
        margin-top: 2px;
        color: var(--muted);
        font-size: 12px;
        overflow-wrap: anywhere;
      }
      .reset-credit-id-row {
        display: flex;
        align-items: center;
        gap: 6px;
        flex-wrap: wrap;
        margin-top: 6px;
      }
      .reset-credit-id-row code {
        max-width: 100%;
        overflow-wrap: anywhere;
      }
      .reset-credit-empty {
        margin-top: 6px;
      }
      .quota-bar-wrap {
        flex: 1;
        min-width: 180px;
      }
      .quota-pair-wrap {
        display: flex;
        flex: 1;
        flex-direction: column;
        gap: 3px;
        min-width: 220px;
      }
      .quota-pair-wrap .quota-bar-wrap {
        flex: none;
        min-width: 0;
        width: 100%;
      }
      .quota-bar {
        position: relative;
        height: 26px;
        background: var(--surface-alt);
        border: 1px solid var(--border);
        border-radius: 7px;
        overflow: hidden;
      }
      .quota-bar-fill {
        position: absolute;
        inset: 0 auto 0 0;
        height: 100%;
        border-radius: 7px;
        background-color: var(--quota-bar-color, #22c55e);
        transition: width 0.4s, background-color 0.4s;
      }
      .quota-bar-text {
        position: absolute;
        inset: 0;
        z-index: 1;
        display: flex;
        align-items: center;
        justify-content: space-between;
        gap: 8px;
        padding: 0 9px;
        color: var(--text);
        font-size: 12px;
        font-weight: 700;
        line-height: 1;
      }
      .quota-bar-text span {
        min-width: 0;
        overflow: hidden;
        text-overflow: ellipsis;
        white-space: nowrap;
      }
      .quota-bar-text span:last-child {
        flex-shrink: 0;
        color: var(--muted);
        font-weight: 600;
      }
      .quota-status {
        flex-basis: 100%;
        margin-top: -2px;
      }
      .glm-balance-tone {
        display: inline-flex;
        align-items: center;
        gap: 6px;
        padding: 2px 6px;
        border-radius: 4px;
        font-size: 12px;
      }
      .glm-balance-tone.success {
        color: #22c55e;
        background: rgba(34, 197, 94, 0.08);
      }
      .glm-balance-tone.warn {
        color: #f59e0b;
        background: rgba(245, 158, 11, 0.08);
      }
      .glm-balance-tone.info {
        color: #94a3b8;
        background: rgba(148, 163, 184, 0.08);
      }
      .glm-balance-tone.muted {
        color: #94a3b8;
      }
      .glm-balance-list {
        list-style: disc inside;
        margin: 6px 0 0 0;
        padding: 0;
        font-size: 12px;
        color: #cbd5e1;
      }
      .glm-balance-list code {
        font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace;
      }
      .quota-balance-block {
        flex-basis: 100%;
        display: flex;
        flex-direction: column;
        gap: 4px;
        padding: 6px 8px;
        border: 1px solid var(--border);
        border-radius: 6px;
        background: var(--surface-raised);
      }
      .quota-balance-label {
        font-size: 12px;
        font-weight: 600;
        color: var(--muted);
        text-transform: uppercase;
        letter-spacing: 0.04em;
      }
      .model-limit-toggle {
        flex-basis: 100%;
        margin-top: 2px;
      }
      .model-quota-details {
        display: flex;
        flex-basis: 100%;
        flex-wrap: wrap;
        gap: 8px;
        padding: 8px;
        border: 1px solid var(--border);
        border-radius: 8px;
        background: var(--surface-raised);
      }
      .model-quota-details .quota-bar-wrap {
        flex: 1;
        min-width: 220px;
      }
      .account-models {
        clear: both;
        margin-top: 10px;
        line-height: 1.45;
      }
      .account-models summary,
      .account-meta summary,
      .quota-notes summary {
        cursor: pointer;
        color: var(--secondary-text);
        font-weight: 700;
      }
      .account-models .model-list {
        display: block;
        box-sizing: border-box;
        margin-top: 4px;
        max-width: 100%;
        padding: 6px 8px;
        border: 1px solid var(--border);
        border-radius: 8px;
        background: var(--surface-alt);
        color: var(--text);
        white-space: normal;
        overflow-wrap: anywhere;
        word-break: break-word;
      }
      .account-models .model-list-grouped {
        display: grid;
        gap: 10px;
      }
      .model-group {
        display: grid;
        gap: 6px;
      }
      .model-group-title {
        color: var(--secondary-text);
        font-size: 12px;
        font-weight: 800;
        text-transform: uppercase;
      }
      .model-chip-list {
        display: flex;
        flex-wrap: wrap;
        gap: 6px;
      }
      .model-chip {
        display: inline-flex;
        align-items: center;
        gap: 5px;
        max-width: 100%;
        padding: 4px 7px;
        border: 1px solid var(--border);
        border-radius: 6px;
        background: var(--surface);
        color: var(--text);
        font-size: 12px;
        line-height: 1.3;
      }
      .model-chip-name {
        overflow-wrap: anywhere;
      }
      .model-badge {
        flex: 0 0 auto;
        padding: 1px 5px;
        border: 1px solid var(--border);
        border-radius: 999px;
        color: var(--muted);
        font-size: 10px;
        font-weight: 800;
        text-transform: uppercase;
      }
      .model-badge-premium {
        border-color: rgba(217,119,6,0.45);
        background: rgba(217,119,6,0.12);
        color: #f59e0b;
      }
      .model-badge-non-premium {
        border-color: rgba(34,197,94,0.35);
        background: rgba(34,197,94,0.10);
        color: #22c55e;
      }
      .model-badge-unknown {
        border-color: rgba(148,163,184,0.35);
        background: rgba(148,163,184,0.10);
      }
      .model-badge-category,
      .model-badge-policy {
        text-transform: none;
      }
      .model-count {
        color: var(--muted);
        font-weight: 600;
      }
      .account-meta,
      .quota-notes {
        margin-top: 8px;
      }
      .meta-list {
        display: grid;
        gap: 4px;
        margin-top: 6px;
      }
      .meta-list > div,
      .meta-list code {
        overflow-wrap: anywhere;
        word-break: break-word;
      }
      .meta-raw-details {
        min-width: 0;
        padding: 6px 0;
      }
      .meta-raw-details summary {
        cursor: pointer;
        color: var(--secondary-text);
        font-weight: 700;
      }
      .meta-raw-preview {
        display: block;
        margin-top: 3px;
        color: var(--muted);
        font-weight: 400;
        overflow-wrap: anywhere;
      }
      .meta-raw-block {
        margin: 6px 0 0 0;
        max-height: 220px;
        overflow: auto;
        white-space: pre-wrap;
        word-break: break-word;
        padding: 8px;
        border: 1px solid var(--border);
        border-radius: 8px;
        background: var(--surface-alt);
        color: var(--text);
        font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace;
        font-size: 12px;
        line-height: 1.45;
      }
      .provider-section {
        min-width: 0;
      }
      .provider-badge {
        display: inline-flex;
        align-items: center;
        gap: 6px;
        width: 100%;
        background: var(--surface-raised);
        border: 1px solid var(--border);
        border-radius: 8px;
        padding: 9px 12px;
        margin-bottom: 12px;
        font-weight: 700;
        font-size: 14px;
      }
      .provider-badge-main {
        display: inline-flex;
        align-items: center;
        gap: 6px;
        min-width: 0;
      }
      .provider-badge-count {
        background: var(--surface-alt);
        border-radius: 20px;
        padding: 2px 10px;
        font-size: 12px;
        color: var(--muted);
      }
      .provider-refresh-button {
        margin-left: auto;
        width: 30px;
        min-width: 30px;
        height: 30px;
        min-height: 30px;
        padding: 0;
        display: inline-flex;
        align-items: center;
        justify-content: center;
        border-radius: 7px;
        line-height: 1;
      }
      .empty-state {
        text-align: center;
        padding: 28px;
        color: var(--muted);
        font-style: italic;
      }
      .mini-btn {
        font-size: 12px;
        min-height: 32px;
        padding: 5px 9px;
        border-radius: 7px;
        background: var(--secondary-bg);
        color: var(--secondary-text);
        border-color: var(--border);
      }
      .mini-btn:hover { background: var(--secondary-hover); }
      .mini-btn.danger { color: #ef4444; }
      .mini-btn.danger:hover { background: #ef4444; color: #fff; }
      .account-menu-wrap {
        position: relative;
        display: inline-flex;
      }
      .account-menu-button {
        display: inline-flex;
        align-items: center;
        justify-content: center;
        width: 34px;
        min-width: 34px;
        height: 34px;
        min-height: 34px;
        padding: 0;
        font-size: 16px;
        font-weight: 800;
        line-height: 1;
      }
      .account-action-menu {
        position: absolute;
        top: calc(100% + 6px);
        right: 0;
        z-index: 180;
        min-width: 160px;
        overflow: hidden;
        border: 1px solid var(--border);
        border-radius: 8px;
        background: var(--surface);
        box-shadow: var(--shadow);
      }
      .account-action-menu .mini-btn {
        display: block;
        width: 100%;
        min-height: 38px;
        padding: 9px 12px;
        border: 0;
        border-bottom: 1px solid var(--border);
        border-radius: 0;
        background: transparent;
        text-align: left;
      }
      .account-action-menu .mini-btn:last-child {
        border-bottom: 0;
      }
      .account-action-menu .mini-btn:hover,
      .account-action-menu .mini-btn:focus {
        background: var(--row-hover);
      }
      .action-btn.is-enabled {
        color: var(--success);
        border-color: rgba(34, 197, 94, 0.35);
      }
      .action-btn.is-disabled {
        color: var(--danger);
        border-color: rgba(239, 68, 68, 0.42);
      }
      .card-model-legend {
        display: flex;
        gap: 10px;
        flex-wrap: wrap;
        margin-bottom: 6px;
        font-size: 11px;
      }
      .card-model-legend-item {
        display: flex;
        align-items: center;
        gap: 3px;
        cursor: pointer;
      }
      .card-model-legend-dot {
        width: 8px; height: 8px; border-radius: 2px; display: inline-block;
      }
      .admin-login-card {
        max-width: 420px;
        margin: 6vh auto;
      }
      .admin-login-copy {
        margin: 0 0 14px 0;
        line-height: 1.5;
      }
      .admin-login-form {
        display: flex;
        flex-direction: column;
        gap: 12px;
      }
      .toast {
        position: fixed;
        right: 18px;
        bottom: 18px;
        z-index: 1200;
        display: none;
        max-width: min(420px, calc(100vw - 24px));
        padding: 10px 12px;
        border: 1px solid var(--border);
        border-radius: 8px;
        background: var(--surface);
        color: var(--text);
        box-shadow: var(--shadow);
      }
      .toast.show { display: block; }
      .toast.error {
        border-color: rgba(239, 68, 68, 0.42);
      }
      .toast.success {
        border-color: rgba(34, 197, 94, 0.42);
      }
      .confirm-modal-card {
        max-width: 460px;
      }
      .confirm-modal-message {
        margin: 10px 0 0 0;
        line-height: 1.5;
      }
      .confirm-actions {
        display: flex;
        justify-content: flex-end;
        gap: 8px;
        margin-top: 18px;
      }
      .confirm-actions button {
        min-width: 96px;
      }
      .confirm-actions button.danger {
        background: var(--danger);
        border-color: var(--danger);
        color: #fff;
      }
      .confirm-actions button.danger:hover {
        background: #dc2626;
        border-color: #dc2626;
      }
      @media (max-width: 900px) {
        .overview-grid {
          grid-template-columns: repeat(2, minmax(0, 1fr));
        }
        .providers-grid {
          grid-template-columns: 1fr;
        }
        .providers-grid.provider-layout-row {
          grid-template-columns: 1fr;
        }
        .providers-grid.provider-layout-row .provider-cards,
        .providers-grid.provider-layout-single .provider-cards {
          grid-template-columns: 1fr;
        }
      }
      @media (max-width: 768px) {
        body { font-size: 15px; }
        .page-shell { padding: 12px; }
        h1 { font-size: 22px; }
        th, td { padding: 10px; font-size: 15px; }
        .muted { font-size: 12px; }
        input, textarea, select, button { font-size: 16px; }
        button { min-height: 44px; }
        .section-header {
          align-items: flex-start;
          flex-direction: column;
        }
        .custom-model-header {
          align-items: stretch;
          flex-direction: column;
        }
        .custom-model-header button {
          width: 100%;
        }
        .custom-model-grid {
          grid-template-columns: 1fr;
        }
        .custom-model-route-row {
          grid-template-columns: 1fr;
        }
        .page-header {
          align-items: flex-start;
          flex-wrap: wrap;
        }
        .page-title-block {
          flex: 1 1 min(260px, calc(100% - 64px));
        }
        .mobile-menu-button {
          display: inline-flex;
          align-items: center;
          justify-content: center;
          min-width: 48px;
          width: 48px;
          padding: 0;
        }
        .header-actions {
          display: none;
          flex-basis: 100%;
          width: 100%;
          align-items: stretch;
        }
        .page-header.mobile-nav-open .header-actions {
          display: flex;
        }
        .header-actions > button,
        .header-actions .provider-menu-wrap,
        .header-actions .provider-menu-wrap > button {
          width: 100%;
        }
        .provider-menu-wrap {
          width: 100%;
        }
        .provider-menu {
          left: 0;
          right: auto;
          width: 100%;
        }
        .provider-menu-title {
          display: block;
        }
        .settings-tabs {
          grid-template-columns: repeat(2, minmax(0, 1fr));
          gap: 4px;
          border-bottom: 0;
        }
        .settings-tab {
          min-height: 44px;
          padding: 8px 6px;
          border: 1px solid var(--border);
          border-radius: 8px;
          background: var(--surface-alt);
          font-size: 14px;
          line-height: 1.2;
          text-align: center;
        }
        .settings-tab.is-active {
          border-color: var(--accent);
          background: var(--surface);
        }
        .provider-settings-row {
          grid-template-columns: 1fr;
          align-items: stretch;
        }
        .api-key-create-row,
        .api-key-row {
          grid-template-columns: 1fr;
        }
        .api-key-access-groups {
          grid-template-columns: 1fr;
        }
        .api-key-limit-row,
        .api-key-access-account {
          grid-template-columns: 1fr;
        }
        .api-key-reveal-header,
        .provider-settings-actions,
        .api-key-actions {
          justify-content: flex-start;
        }
        .provider-settings-actions .mini-btn {
          flex: 1;
        }
        .notification-channel-grid,
        .test-api-grid,
        .notification-provider-head,
        .custom-model-account-row {
          grid-template-columns: 1fr;
        }
        .notification-provider-actions {
          justify-content: flex-start;
        }
        .overview-grid {
          gap: 8px;
        }
        .overview-card {
          padding: 10px;
        }
        .overview-value {
          font-size: 20px;
        }
        .overview-note {
          white-space: normal;
        }
        .chart-section {
          padding: 12px;
        }
        .chart-header {
          align-items: flex-start;
          flex-direction: column;
        }
        .chart-controls,
        .chart-custom-controls,
        .chart-field,
        .chart-field select,
        .chart-field input,
        .chart-controls button {
          width: 100%;
        }
        .chart-title-row h2 {
          font-size: 16px;
        }
        .chart-legend {
          justify-content: flex-start;
          gap: 8px 10px;
        }
        .chart-wrap { height: 210px; }
        .card {
          padding: 12px;
        }
        .card-actions {
          width: auto;
          margin-left: auto;
        }
        .mini-btn {
          min-height: 44px;
          padding: 8px 12px;
        }
        .quota-pair-wrap,
        .quota-bar-wrap,
        .model-quota-details .quota-bar-wrap {
          min-width: 100%;
        }
        .modal {
          padding: 12px;
        }
        .modal-card {
          margin: 0 auto;
          max-height: calc(100vh - 24px);
        }
        .modal-close-button {
          top: -6px;
        }
        .modal-card > .modal-actions:last-child,
        .modal-card form > .modal-actions:last-child {
          align-items: stretch;
          flex-wrap: nowrap;
        }
        .modal-card > .modal-actions:last-child > button,
        .modal-card form > .modal-actions:last-child > button {
          flex: 1 1 0;
          min-width: 0;
          padding-right: 8px;
          padding-left: 8px;
        }
        .toast {
          right: 12px;
          bottom: 12px;
        }
      }
    </style>
    <script>
      (() => {
        try {
          const savedTheme = localStorage.getItem('io-gateway-theme');
          document.documentElement.setAttribute('data-theme', savedTheme === 'light' ? 'light' : 'dark');
        } catch (_) {
          document.documentElement.setAttribute('data-theme', 'dark');
        }
      })();
    </script>
    <script src="https://cdn.jsdelivr.net/npm/chart.js@4.4.7/dist/chart.umd.min.js"></script>
  </head>
  <body>
    <div id="adminLoginGate" class="modal" role="dialog" aria-modal="true" aria-labelledby="adminLoginTitle" aria-hidden="true" style="display:none;">
      <div class="modal-card admin-login-card">
        <h2 id="adminLoginTitle" style="margin-top:0;">Admin Login</h2>
        <p class="admin-login-copy">Enter the current 6-digit OTP from Google Authenticator to manage accounts.</p>
        <form id="adminLoginForm" class="admin-login-form">
          <input class="sr-only" type="text" name="username" autocomplete="username" value="admin" tabindex="-1" aria-hidden="true">
          <div>
            <label for="adminOtpInput">Google Authenticator OTP</label>
            <input id="adminOtpInput" name="otp" inputmode="numeric" autocomplete="one-time-code" pattern="[0-9]*" placeholder="123456">
          </div>
          <button type="submit">Log in</button>
          <div id="adminLoginStatus" class="muted"></div>
        </form>
      </div>
    </div>
    <main id="dashboardContent" class="page-shell">
      <header class="page-header">
        <div class="page-title-block">
          <h1>IO Gateway Usage</h1>
          <div id="pageSubtitle" class="page-subtitle muted">Loading accounts...</div>
        </div>
        <button type="button" id="mobileMenuBtn" class="mobile-menu-button secondary-button" aria-label="Open navigation menu" aria-controls="headerActions" aria-expanded="false">
          <span class="hamburger-icon" aria-hidden="true"><span></span><span></span><span></span></span>
        </button>
        <div id="headerActions" class="header-actions">
          <button type="button" id="themeToggleBtn" class="secondary-button">Theme: Dark</button>
          <div class="provider-menu-wrap">
            <button type="button" id="addProviderBtn" aria-haspopup="menu" aria-expanded="false" aria-controls="providerMenu">+ Add account</button>
            <div id="providerMenu" class="provider-menu" role="menu" hidden>
              <div class="provider-menu-title" role="presentation">Choose provider</div>
              <button type="button" class="provider-menu-item" role="menuitem" data-provider="codex">Codex (ChatGPT)</button>
              <button type="button" class="provider-menu-item" role="menuitem" data-provider="antigravity">Antigravity (Google)</button>
              <button type="button" class="provider-menu-item" role="menuitem" data-provider="gemini">Gemini (Google)</button>
              <button type="button" class="provider-menu-item" role="menuitem" data-provider="qwen">Qwen</button>
              <button type="button" class="provider-menu-item" role="menuitem" data-provider="deepseek">DeepSeek</button>
              <button type="button" class="provider-menu-item" role="menuitem" data-provider="minimax">MiniMax</button>
              <button type="button" class="provider-menu-item" role="menuitem" data-provider="grok">Grok (xAI)</button>
              <button type="button" class="provider-menu-item" role="menuitem" data-provider="copilot">GitHub Copilot</button>
              <button type="button" class="provider-menu-item" role="menuitem" data-provider="claude">Claude</button>
              <button type="button" class="provider-menu-item" role="menuitem" data-provider="glm">GLM (Z.AI)</button>
              <button type="button" class="provider-menu-item" role="menuitem" data-provider="custom-model">Custom model</button>
            </div>
          </div>
          <button type="button" id="openTestApiBtn" class="secondary-button">Test API</button>
          <button type="button" id="appSettingsBtn" class="secondary-button">Settings</button>
          <button type="button" id="logoutBtn" class="secondary-button" style="display:none;">Log out</button>
        </div>
      </header>
      <section class="overview-grid" aria-label="Gateway overview">
        <div class="overview-card">
          <div class="overview-label">Requests</div>
          <div id="overviewRequests" class="overview-value">...</div>
          <div class="overview-note">Last recorded total</div>
        </div>
        <div class="overview-card">
          <div class="overview-label">Errors</div>
          <div id="overviewErrors" class="overview-value">...</div>
          <div id="overviewErrorNote" class="overview-note">Waiting for data</div>
        </div>
        <div class="overview-card">
          <div class="overview-label">Accounts</div>
          <div id="overviewAccounts" class="overview-value">...</div>
          <div id="overviewProviderNote" class="overview-note">Across providers</div>
        </div>
        <div class="overview-card">
          <div class="overview-label">Attention</div>
          <div id="overviewAttention" class="overview-value">...</div>
          <div id="overviewAttentionNote" class="overview-note">Loading status</div>
        </div>
      </section>
      <details id="chartDetails" class="chart-section" open>
        <summary class="chart-header">
          <span class="chart-title-row">
            <h2 id="contextChartTitle">Context Usage (24h)</h2>
            <span id="contextUsageSummary" class="chart-summary">Loading usage...</span>
            <span class="chart-toggle-hint" aria-hidden="true"></span>
          </span>
          <div class="chart-legend" id="chartLegend"></div>
        </summary>
        <div id="contextRangeControls" class="chart-controls">
          <label class="chart-field" for="contextRangeSelect">
            <span>Range</span>
            <select id="contextRangeSelect">
              <option value="hour">1 hour</option>
              <option value="day">1 day</option>
              <option value="week">1 week</option>
              <option value="custom">Custom</option>
            </select>
          </label>
          <div id="contextCustomControls" class="chart-custom-controls" hidden>
            <label class="chart-field" for="contextCustomHours">
              <span>Hours</span>
              <input id="contextCustomHours" type="number" min="1" max="720" step="1" value="24">
            </label>
            <label class="chart-field" for="contextBucketMinutes">
              <span>Bucket</span>
              <select id="contextBucketMinutes">
                <option value="1">1 min</option>
                <option value="5">5 min</option>
                <option value="15">15 min</option>
                <option value="30">30 min</option>
                <option value="60">60 min</option>
              </select>
            </label>
            <button type="button" id="contextApplyRangeBtn" class="secondary-button">Apply</button>
          </div>
        </div>
        <div class="chart-wrap"><canvas id="contextChart"></canvas></div>
      </details>
      <section class="custom-model-section" aria-labelledby="customModelsTitle">
        <div class="custom-model-header">
          <div>
            <h2 id="customModelsTitle">Custom Models</h2>
            <div id="customModelsNote" class="muted">Loading custom routes...</div>
          </div>
          <button type="button" id="addCustomModelBtn" class="secondary-button">+ Custom model</button>
        </div>
        <div id="customModelCards" class="custom-model-grid"></div>
      </section>
      <div class="providers-grid">
      <section class="provider-section" data-provider-section="codex" aria-labelledby="codexProviderTitle">
        <div class="provider-badge">
          <span class="provider-badge-main">
            <span id="codexProviderTitle">Codex</span>
            <span class="provider-badge-count" id="codexBadgeCount">0 accounts</span>
          </span>
          <button type="button" class="mini-btn provider-refresh-button" title="Refresh Codex models and quota" aria-label="Refresh Codex models and quota" onclick="refreshModelCatalog('codex')">&#8635;</button>
        </div>
        <div id="codexCards" class="provider-cards"></div>
      </section>
      <section class="provider-section" data-provider-section="agw" aria-labelledby="agwProviderTitle">
        <div class="provider-badge">
          <span class="provider-badge-main">
            <span id="agwProviderTitle">Antigravity</span>
            <span class="provider-badge-count" id="agwBadgeCount">0 accounts</span>
          </span>
          <button type="button" class="mini-btn provider-refresh-button" title="Refresh Antigravity models and quota" aria-label="Refresh Antigravity models and quota" onclick="refreshModelCatalog('agw')">&#8635;</button>
        </div>
        <div id="agwCards" class="provider-cards"></div>
      </section>
      <section class="provider-section" data-provider-section="gemini" aria-labelledby="geminiProviderTitle">
        <div class="provider-badge">
          <span class="provider-badge-main">
            <span id="geminiProviderTitle">Gemini</span>
            <span class="provider-badge-count" id="geminiBadgeCount">0 accounts</span>
          </span>
          <button type="button" class="mini-btn provider-refresh-button" title="Refresh Gemini models and quota" aria-label="Refresh Gemini models and quota" onclick="refreshModelCatalog('gemini')">&#8635;</button>
        </div>
        <div id="geminiCards" class="provider-cards"></div>
      </section>
      <section class="provider-section" data-provider-section="qwen" aria-labelledby="qwenProviderTitle">
        <div class="provider-badge">
          <span class="provider-badge-main">
            <span id="qwenProviderTitle">Qwen</span>
            <span class="provider-badge-count" id="qwenBadgeCount">0 accounts</span>
          </span>
          <button type="button" class="mini-btn provider-refresh-button" title="Refresh Qwen models and quota" aria-label="Refresh Qwen models and quota" onclick="refreshModelCatalog('qwen')">&#8635;</button>
        </div>
        <div id="qwenCards" class="provider-cards"></div>
      </section>
      <section class="provider-section" data-provider-section="deepseek" aria-labelledby="deepseekProviderTitle">
        <div class="provider-badge">
          <span class="provider-badge-main">
            <span id="deepseekProviderTitle">DeepSeek</span>
            <span class="provider-badge-count" id="deepseekBadgeCount">0 accounts</span>
          </span>
          <button type="button" class="mini-btn provider-refresh-button" title="Refresh DeepSeek models and quota" aria-label="Refresh DeepSeek models and quota" onclick="refreshModelCatalog('deepseek')">&#8635;</button>
        </div>
        <div id="deepseekCards" class="provider-cards"></div>
      </section>
      <section class="provider-section" data-provider-section="minimax" aria-labelledby="minimaxProviderTitle">
        <div class="provider-badge">
          <span class="provider-badge-main">
            <span id="minimaxProviderTitle">MiniMax</span>
            <span class="provider-badge-count" id="minimaxBadgeCount">0 accounts</span>
          </span>
          <button type="button" class="mini-btn provider-refresh-button" title="Refresh MiniMax models and quota" aria-label="Refresh MiniMax models and quota" onclick="refreshModelCatalog('minimax')">&#8635;</button>
        </div>
        <div id="minimaxCards" class="provider-cards"></div>
      </section>
      <section class="provider-section" data-provider-section="grok" aria-labelledby="grokProviderTitle">
        <div class="provider-badge">
          <span class="provider-badge-main">
            <span id="grokProviderTitle">Grok (xAI)</span>
            <span class="provider-badge-count" id="grokBadgeCount">— accounts</span>
          </span>
          <button type="button" class="mini-btn provider-refresh-button" title="Refresh Grok models and quota" aria-label="Refresh Grok models and quota" onclick="refreshModelCatalog('grok')">&#8635;</button>
        </div>
        <div id="grokCards" class="provider-cards"></div>
      </section>
      <section class="provider-section" data-provider-section="copilot" aria-labelledby="copilotProviderTitle">
        <div class="provider-badge">
          <span class="provider-badge-main">
            <span id="copilotProviderTitle">GitHub Copilot</span>
            <span class="provider-badge-count" id="copilotBadgeCount">0 accounts</span>
          </span>
          <button type="button" class="mini-btn provider-refresh-button" title="Refresh GitHub Copilot models and quota" aria-label="Refresh GitHub Copilot models and quota" onclick="refreshModelCatalog('copilot')">&#8635;</button>
        </div>
        <div id="copilotCards" class="provider-cards"></div>
      </section>
      <section class="provider-section" data-provider-section="claude" aria-labelledby="claudeProviderTitle">
        <div class="provider-badge">
          <span class="provider-badge-main">
            <span id="claudeProviderTitle">Claude</span>
            <span class="provider-badge-count" id="claudeBadgeCount">0 accounts</span>
          </span>
          <button type="button" class="mini-btn provider-refresh-button" title="Refresh Claude models and quota" aria-label="Refresh Claude models and quota" onclick="refreshModelCatalog('claude')">&#8635;</button>
        </div>
        <div id="claudeCards" class="provider-cards"></div>
      </section>
      <section class="provider-section" data-provider-section="glm" aria-labelledby="glmProviderTitle">
        <div class="provider-badge">
          <span class="provider-badge-main">
            <span id="glmProviderTitle">GLM (Z.AI)</span>
            <span class="provider-badge-count" id="glmBadgeCount">0 accounts</span>
          </span>
          <button type="button" class="mini-btn provider-refresh-button" title="Refresh GLM models and quota" aria-label="Refresh GLM models and quota" onclick="refreshModelCatalog('glm')">&#8635;</button>
        </div>
        <div id="glmCards" class="provider-cards"></div>
      </section>
      </div>
    </main>
    <div id="toast" class="toast" role="status" aria-live="polite"></div>
    <div id="confirmActionModal" class="modal" role="dialog" aria-modal="true" aria-labelledby="confirmActionTitle" aria-describedby="confirmActionMessage" aria-hidden="true" style="display:none;">
      <div class="modal-card confirm-modal-card">
        <h2 id="confirmActionTitle" style="margin-top:0;">Confirm action</h2>
        <p id="confirmActionMessage" class="confirm-modal-message"></p>
        <div class="confirm-actions">
          <button type="button" id="confirmActionRejectBtn" class="secondary-button">Cancel</button>
          <button type="button" id="confirmActionApproveBtn">Approve</button>
        </div>
      </div>
    </div>
    <script>
      let adminAuthEnabled = false;
      let adminAuthenticated = false;
      let adminAuthEpoch = 0;
      let dashboardIntervalsStarted = false;
      let lastQuota = new Map();
      let lastAgwQuota = new Map();
      let lastGeminiQuota = new Map();
      let lastQwenQuota = new Map();
      let lastCopilotQuota = new Map();
      let lastClaudeQuota = new Map();
      let lastGlmQuota = new Map();
      let dashboardSnapshot = null;
      let dashboardSnapshotAt = 0;
      let dashboardSnapshotRequest = null;
      let openAgwRows = new Set();
      let openGeminiRows = new Set();
      let openQwenRows = new Set();
      const openAccountDetails = {
        models: new Set(),
        connection: new Set(),
        attention: new Set(),
        resetCredits: new Set()
      };
      let activeTipEl = null;
      let activeTipTimer = null;
      let copilotDevicePollTimer = null;
      let copilotDevicePollInFlight = false;
      let copilotDeviceExpiresAt = 0;
      let providerRowArrangeTimer = null;
      const THEME_KEY = 'io-gateway-theme';
      const CONTEXT_RANGE_KEY = 'io-gateway-context-range';
      const PROVIDER_DASHBOARD_SETTINGS_KEY = 'io-gateway-provider-dashboard-settings';
      const dashboardProviderKeys = [
        'codex',
        'agw',
        'gemini',
        'qwen',
        'deepseek',
        'minimax',
        'grok',
        'copilot',
        'claude',
        'glm'
      ];
      const PROVIDER_ROW_CARD_WIDTH = 390;
      const dashboardDateFormatter = new Intl.DateTimeFormat('en-US', {
        month: 'short',
        day: 'numeric',
        year: 'numeric'
      });
      const dashboardTimeFormatter = new Intl.DateTimeFormat('en-US', {
        hour: '2-digit',
        minute: '2-digit',
        second: '2-digit',
        hourCycle: 'h23'
      });
      const dashboardDateTimeFormatter = new Intl.DateTimeFormat('en-US', {
        month: 'short',
        day: 'numeric',
        year: 'numeric',
        hour: '2-digit',
        minute: '2-digit',
        second: '2-digit',
        hourCycle: 'h23'
      });
      let pendingCredentialAction = null;
      const modalIds = [
        'addModal',
        'addAgwModal',
        'addGeminiModal',
        'addQwenModal',
        'addDeepSeekModal',
        'addMiniMaxModal',
        'addGrokModal',
        'addCopilotModal',
        'addClaudeModal',
        'addGlmModal',
        'appSettingsModal',
        'customModelModal',
        'confirmActionModal'
      ];
      const dashboardState = {
        totalRequests: 0,
        totalErrors: 0,
        providers: {
          codex: [],
          agw: [],
          gemini: [],
          qwen: [],
          deepseek: [],
          minimax: [],
          grok: [],
          copilot: [],
          claude: [],
          glm: []
        },
        customModels: [],
        customModelAccounts: [],
        customModelModelOptions: [],
        testApiModelOptions: [],
        providerSettings: readProviderDashboardSettings(),
        notificationSettings: null,
        accountRouting: { providers: {} },
        apiKeys: [],
        apiKeyAccounts: [],
        apiKeyEditingId: '',
        quotas: {
          codex: new Map(),
          agw: new Map(),
          gemini: new Map(),
          qwen: new Map(),
          deepseek: new Map(),
          minimax: new Map(),
          grok: new Map(),
          copilot: new Map(),
          claude: new Map(),
          glm: new Map()
        }
      };
      const providerLabels = {
        cod: 'Codex',
        codex: 'Codex',
        agw: 'Antigravity',
        gem: 'Gemini',
        gemini: 'Gemini',
        qwn: 'Qwen',
        qwen: 'Qwen',
        dsk: 'DeepSeek',
        deepseek: 'DeepSeek',
        min: 'MiniMax',
        minimax: 'MiniMax',
        grk: 'Grok',
        grok: 'Grok',
        cop: 'GitHub Copilot',
        copilot: 'GitHub Copilot',
        cld: 'Claude',
        claude: 'Claude',
        glm: 'GLM (Z.AI)',
        ctm: 'Custom'
      };
      function normalizeProviderDashboardViewMode(mode) {
        return mode === 'row' || mode === 'single' ? 'row' : 'column';
      }
      function normalizeProviderDashboardSettings(value) {
        var known = new Set(dashboardProviderKeys);
        var order = [];
        if (value && Array.isArray(value.order)) {
          value.order.forEach(function(provider) {
            if (known.has(provider) && order.indexOf(provider) === -1) {
              order.push(provider);
            }
          });
        }
        dashboardProviderKeys.forEach(function(provider) {
          if (order.indexOf(provider) === -1) order.push(provider);
        });
        var hidden = {};
        var rawHidden = value && value.hidden && typeof value.hidden === 'object' ? value.hidden : {};
        dashboardProviderKeys.forEach(function(provider) {
          if (rawHidden[provider] === true) hidden[provider] = true;
        });
        var viewMode = normalizeProviderDashboardViewMode(value && value.viewMode);
        return { order: order, hidden: hidden, viewMode: viewMode };
      }
      function readProviderDashboardSettings() {
        try {
          return normalizeProviderDashboardSettings(JSON.parse(localStorage.getItem(PROVIDER_DASHBOARD_SETTINGS_KEY) || 'null'));
        } catch (_) {
          return normalizeProviderDashboardSettings(null);
        }
      }
      function writeProviderDashboardSettings(settings) {
        dashboardState.providerSettings = normalizeProviderDashboardSettings(settings);
        try {
          localStorage.setItem(PROVIDER_DASHBOARD_SETTINGS_KEY, JSON.stringify(dashboardState.providerSettings));
        } catch (_) {}
      }
      function providerRowCapacity(grid) {
        if (!grid) return 1;
        if (window.matchMedia && window.matchMedia('(max-width: 900px)').matches) return 1;
        var styles = window.getComputedStyle ? window.getComputedStyle(grid) : null;
        var gap = styles ? parseFloat(styles.columnGap || styles.gap || '16') : 16;
        if (!Number.isFinite(gap)) gap = 16;
        var width = grid.getBoundingClientRect().width;
        if (!Number.isFinite(width) || width <= 0) return 1;
        return Math.max(1, Math.floor((width + gap) / (PROVIDER_ROW_CARD_WIDTH + gap)));
      }
      function providerAccountCardCount(section) {
        if (!section) return 0;
        return section.querySelectorAll('.provider-cards > .card').length;
      }
      function clearProviderRowArrangement(grid) {
        if (grid) grid.style.removeProperty('--provider-row-capacity');
        dashboardProviderKeys.forEach(function(provider) {
          var section = document.querySelector('[data-provider-section="' + provider + '"]');
          if (section) section.style.gridColumn = '';
        });
      }
      function applyProviderRowArrangement() {
        var grid = document.querySelector('.providers-grid');
        if (!grid) return;
        var settings = normalizeProviderDashboardSettings(dashboardState.providerSettings);
        if (settings.viewMode !== 'row') {
          clearProviderRowArrangement(grid);
          return;
        }
        var capacity = providerRowCapacity(grid);
        grid.style.setProperty('--provider-row-capacity', String(capacity));
        dashboardProviderKeys.forEach(function(provider) {
          var section = document.querySelector('[data-provider-section="' + provider + '"]');
          if (!section) return;
          if (settings.hidden[provider] === true) {
            section.style.gridColumn = '';
            return;
          }
          var count = providerAccountCardCount(section);
          if (count > capacity) {
            section.style.gridColumn = '1 / -1';
          } else {
            section.style.gridColumn = 'span ' + Math.max(1, count);
          }
        });
      }
      function scheduleProviderRowArrangement() {
        clearTimeout(providerRowArrangeTimer);
        providerRowArrangeTimer = setTimeout(applyProviderRowArrangement, 80);
      }
      function applyProviderDashboardSettings() {
        var settings = normalizeProviderDashboardSettings(dashboardState.providerSettings);
        dashboardState.providerSettings = settings;
        var grid = document.querySelector('.providers-grid');
        if (grid) {
          grid.classList.toggle('provider-layout-row', settings.viewMode === 'row');
          grid.classList.remove('provider-layout-single');
        }
        var orderByProvider = {};
        settings.order.forEach(function(provider, index) {
          orderByProvider[provider] = index;
        });
        dashboardProviderKeys.forEach(function(provider, fallbackIndex) {
          var section = document.querySelector('[data-provider-section="' + provider + '"]');
          if (!section) return;
          section.style.order = String(orderByProvider[provider] != null ? orderByProvider[provider] : fallbackIndex);
          section.hidden = settings.hidden[provider] === true;
        });
        applyProviderRowArrangement();
      }
      function providerDashboardVisibleCount() {
        var settings = normalizeProviderDashboardSettings(dashboardState.providerSettings);
        return dashboardProviderKeys.filter(function(provider) {
          return settings.hidden[provider] !== true;
        }).length;
      }
      function providerDashboardViewModeLabel(mode) {
        return normalizeProviderDashboardViewMode(mode) === 'row' ? 'row layout' : 'column layout';
      }
      function updateProviderLayoutModeControl() {
        var settings = normalizeProviderDashboardSettings(dashboardState.providerSettings);
        document.querySelectorAll('[data-provider-layout-mode]').forEach(function(button) {
          var active = normalizeProviderDashboardViewMode(button.getAttribute('data-provider-layout-mode')) === settings.viewMode;
          button.classList.toggle('is-active', active);
          button.setAttribute('aria-pressed', active ? 'true' : 'false');
        });
      }
      function updateAppSettingsStatus() {
        var settings = normalizeProviderDashboardSettings(dashboardState.providerSettings);
        setText(
          'appSettingsStatus',
          providerDashboardVisibleCount() + ' providers visible · ' + providerDashboardViewModeLabel(settings.viewMode)
        );
      }
      function renderProviderSettingsList() {
        var list = document.getElementById('providerSettingsList');
        if (!list) return;
        var settings = normalizeProviderDashboardSettings(dashboardState.providerSettings);
        dashboardState.providerSettings = settings;
        var total = settings.order.length;
        list.innerHTML = settings.order.map(function(provider, index) {
          var label = providerLabels[provider] || provider;
          var checked = settings.hidden[provider] === true ? '' : ' checked';
          var hiddenClass = settings.hidden[provider] === true ? ' is-hidden' : '';
          var providerArg = escapeHtml(jsString(provider));
          return '<div class="provider-settings-row' + hiddenClass + '">'
            + '<label class="check-row provider-settings-visible">'
            + '<input type="checkbox" onchange="setProviderDashboardVisible(' + providerArg + ', this.checked)"' + checked + '> '
            + '<span>' + escapeHtml(label) + '</span>'
            + '</label>'
            + '<span class="provider-settings-key"><code>' + escapeHtml(provider) + '</code></span>'
            + '<span class="provider-settings-actions">'
            + '<button type="button" class="mini-btn" aria-label="' + escapeHtml('Move ' + label + ' up') + '" onclick="moveProviderDashboardSetting(' + providerArg + ', -1)"' + (index === 0 ? ' disabled' : '') + '>&uarr;</button>'
            + '<button type="button" class="mini-btn" aria-label="' + escapeHtml('Move ' + label + ' down') + '" onclick="moveProviderDashboardSetting(' + providerArg + ', 1)"' + (index === total - 1 ? ' disabled' : '') + '>&darr;</button>'
            + '</span>'
            + '</div>';
        }).join('');
        updateProviderLayoutModeControl();
        updateAppSettingsStatus();
      }
      function saveAndRenderProviderDashboardSettings(settings) {
        writeProviderDashboardSettings(settings);
        applyProviderDashboardSettings();
        renderProviderSettingsList();
      }
      function setProviderDashboardVisible(provider, visible) {
        var settings = normalizeProviderDashboardSettings(dashboardState.providerSettings);
        if (visible) {
          delete settings.hidden[provider];
        } else {
          settings.hidden[provider] = true;
        }
        saveAndRenderProviderDashboardSettings(settings);
      }
      function moveProviderDashboardSetting(provider, direction) {
        var settings = normalizeProviderDashboardSettings(dashboardState.providerSettings);
        var index = settings.order.indexOf(provider);
        if (index === -1) return;
        var nextIndex = Math.max(0, Math.min(settings.order.length - 1, index + direction));
        if (nextIndex === index) return;
        var moved = settings.order.splice(index, 1)[0];
        settings.order.splice(nextIndex, 0, moved);
        saveAndRenderProviderDashboardSettings(settings);
      }
      function setProviderDashboardViewMode(mode) {
        var settings = normalizeProviderDashboardSettings(dashboardState.providerSettings);
        settings.viewMode = normalizeProviderDashboardViewMode(mode);
        saveAndRenderProviderDashboardSettings(settings);
      }
      function resetProviderDashboardSettings() {
        saveAndRenderProviderDashboardSettings(normalizeProviderDashboardSettings(null));
      }
      function setAppSettingsTab(tab) {
        var target = tab === 'notifications'
          ? 'notifications'
          : tab === 'api-keys'
            ? 'api-keys'
            : tab === 'test-api'
              ? 'test-api'
              : 'dashboard';
        var activeButton = null;
        document.querySelectorAll('[data-settings-tab]').forEach(function(button) {
          var active = button.getAttribute('data-settings-tab') === target;
          button.classList.toggle('is-active', active);
          button.setAttribute('aria-selected', active ? 'true' : 'false');
          if (active) activeButton = button;
        });
        document.querySelectorAll('[data-settings-panel]').forEach(function(panel) {
          panel.hidden = panel.getAttribute('data-settings-panel') !== target;
        });
        if (activeButton && activeButton.scrollIntoView) {
          setTimeout(function() {
            activeButton.scrollIntoView({ block: 'nearest', inline: 'center' });
          }, 0);
        }
        if (target === 'test-api') prepareTestApiPanel();
      }
      async function loadNotificationSettings() {
        setText('notificationStatus', 'Loading notification settings...');
        const res = await adminFetch('/notifications/settings');
        if (!res) return;
        const data = await res.json();
        dashboardState.notificationSettings = data.settings || null;
        renderNotificationSettings();
      }
      function renderNotificationSettings() {
        var settings = dashboardState.notificationSettings || {};
        var enabled = document.getElementById('notificationEnabledInput');
        var channel = document.getElementById('notificationChannelInput');
        var telegramChatId = document.getElementById('telegramChatIdInput');
        var telegramToken = document.getElementById('telegramBotTokenInput');
        var googleWebhook = document.getElementById('googleChatWebhookInput');
        if (enabled) enabled.checked = settings.enabled === true;
        if (channel) channel.value = settings.channel === 'google_chat' ? 'google_chat' : 'telegram';
        if (telegramChatId) telegramChatId.value = settings.telegram && settings.telegram.chat_id ? settings.telegram.chat_id : '';
        if (telegramToken) {
          telegramToken.value = '';
          telegramToken.placeholder = settings.telegram && settings.telegram.bot_token_configured
            ? 'Configured. Leave blank to keep current token.'
            : 'Telegram bot token';
        }
        if (googleWebhook) {
          googleWebhook.value = '';
          googleWebhook.placeholder = settings.google_chat && settings.google_chat.webhook_configured
            ? 'Configured. Leave blank to keep current webhook URL.'
            : 'Google Chat incoming webhook URL';
        }
        updateNotificationChannelUi();
        renderNotificationWatchList();
        updateNotificationStatusText();
      }
      function updateNotificationChannelUi() {
        var value = document.getElementById('notificationChannelInput')?.value || 'telegram';
        var telegram = document.getElementById('telegramNotificationFields');
        var google = document.getElementById('googleChatNotificationFields');
        if (telegram) telegram.hidden = value !== 'telegram';
        if (google) google.hidden = value !== 'google_chat';
      }
      function updateNotificationStatusText(message) {
        if (message) {
          setText('notificationStatus', message);
          return;
        }
        var settings = dashboardState.notificationSettings || {};
        var watched = Array.isArray(settings.watched_accounts) ? settings.watched_accounts.length : 0;
        var total = Array.isArray(settings.accounts) ? settings.accounts.length : 0;
        var stateText = settings.enabled ? 'enabled' : 'disabled';
        setText('notificationStatus', 'Notifications ' + stateText + ' - watching ' + watched + ' of ' + total + ' accounts');
      }
      function renderNotificationWatchList() {
        var list = document.getElementById('notificationWatchList');
        if (!list) return;
        var settings = dashboardState.notificationSettings || {};
        var accounts = Array.isArray(settings.accounts) ? settings.accounts : [];
        var watched = new Set(Array.isArray(settings.watched_accounts) ? settings.watched_accounts : []);
        if (!accounts.length) {
          list.innerHTML = '<div class="empty-state">No provider accounts available</div>';
          return;
        }
        var grouped = {};
        accounts.forEach(function(account) {
          var provider = account.provider || 'unknown';
          if (!grouped[provider]) grouped[provider] = [];
          grouped[provider].push(account);
        });
        var providers = dashboardProviderKeys.slice();
        Object.keys(grouped).forEach(function(provider) {
          if (providers.indexOf(provider) === -1) providers.push(provider);
        });
        list.innerHTML = providers.filter(function(provider) {
          return grouped[provider] && grouped[provider].length;
        }).map(function(provider) {
          var items = grouped[provider] || [];
          var label = items[0].provider_label || providerLabels[provider] || provider;
          var providerArg = escapeHtml(jsString(provider));
          var rows = items.map(function(account) {
            var key = account.key || '';
            var keyArg = escapeHtml(jsString(key));
            var checked = watched.has(key) ? ' checked' : '';
            var disabledClass = account.enabled === false ? ' is-disabled' : '';
            var title = account.label || account.account_id || key;
            var meta = [account.account_id, account.credential_file, key].filter(Boolean).join(' - ');
            return '<div class="notification-account-row' + disabledClass + '" data-notification-provider="' + escapeHtml(provider) + '">'
              + '<label class="check-row">'
              + '<input type="checkbox" data-notification-account value="' + escapeHtml(key) + '" onchange="toggleNotificationAccount(' + keyArg + ', this.checked)"' + checked + '> '
              + '<span class="account-choice-text">'
              + '<span class="account-choice-title">' + escapeHtml(title) + '</span>'
              + (meta ? '<span class="notification-account-meta account-choice-meta" title="' + escapeHtml(meta) + '">' + escapeHtml(compactMiddle(meta, 72)) + '</span>' : '')
              + '</span>'
              + '</label>'
              + '</div>';
          }).join('');
          return '<div class="notification-provider-group">'
            + '<div class="notification-provider-head">'
            + '<div><strong>' + escapeHtml(label) + '</strong> <span class="muted">' + items.length + ' accounts</span></div>'
            + '<span class="notification-provider-actions">'
            + '<button type="button" class="mini-btn" onclick="setNotificationProviderWatch(' + providerArg + ', true)">Check all</button>'
            + '<button type="button" class="mini-btn secondary-button" onclick="setNotificationProviderWatch(' + providerArg + ', false)">Uncheck all</button>'
            + '</span>'
            + '</div>'
            + rows
            + '</div>';
        }).join('');
      }
      function notificationWatchedKeysFromDom() {
        return Array.from(document.querySelectorAll('[data-notification-account]:checked'))
          .map(function(input) { return input.value; })
          .filter(Boolean);
      }
      function setNotificationWatchedKeys(keys) {
        var settings = dashboardState.notificationSettings || {};
        var unique = Array.from(new Set(keys || []));
        settings.watched_accounts = unique;
        dashboardState.notificationSettings = settings;
        renderNotificationWatchList();
        updateNotificationStatusText();
      }
      function toggleNotificationAccount(key, checked) {
        var settings = dashboardState.notificationSettings || {};
        var watched = new Set(Array.isArray(settings.watched_accounts) ? settings.watched_accounts : []);
        if (checked) watched.add(key);
        else watched.delete(key);
        setNotificationWatchedKeys(Array.from(watched));
      }
      function setNotificationAllWatch(checked) {
        var settings = dashboardState.notificationSettings || {};
        var accounts = Array.isArray(settings.accounts) ? settings.accounts : [];
        setNotificationWatchedKeys(checked ? accounts.map(function(account) { return account.key; }).filter(Boolean) : []);
      }
      function setNotificationProviderWatch(provider, checked) {
        var settings = dashboardState.notificationSettings || {};
        var watched = new Set(Array.isArray(settings.watched_accounts) ? settings.watched_accounts : []);
        (settings.accounts || []).forEach(function(account) {
          if (account.provider !== provider || !account.key) return;
          if (checked) watched.add(account.key);
          else watched.delete(account.key);
        });
        setNotificationWatchedKeys(Array.from(watched));
      }
      async function saveNotificationSettings() {
        var settings = dashboardState.notificationSettings || {};
        var telegramToken = document.getElementById('telegramBotTokenInput').value.trim();
        var googleWebhook = document.getElementById('googleChatWebhookInput').value.trim();
        var body = {
          enabled: document.getElementById('notificationEnabledInput').checked,
          channel: document.getElementById('notificationChannelInput').value,
          telegram: {
            bot_token: telegramToken || undefined,
            chat_id: document.getElementById('telegramChatIdInput').value.trim()
          },
          google_chat: {
            webhook_url: googleWebhook || undefined
          },
          watched_accounts: notificationWatchedKeysFromDom()
        };
        setText('notificationStatus', 'Saving notification settings...');
        const res = await adminFetch('/notifications/settings', {
          method: 'POST',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify(body)
        });
        if (!res) return;
        const data = await res.json();
        if (!data.ok) {
          updateNotificationStatusText(data.message || 'Failed to save notification settings');
          return;
        }
        dashboardState.notificationSettings = data.settings || settings;
        renderNotificationSettings();
        updateNotificationStatusText('Notification settings saved');
      }
      async function sendTestNotification() {
        setText('notificationStatus', 'Sending test notification...');
        const res = await adminFetch('/notifications/test', { method: 'POST' });
        if (!res) return;
        const data = await res.json();
        updateNotificationStatusText(data.message || (data.ok ? 'Test notification sent' : 'Test notification failed'));
      }
      function testApiModelEntries() {
        var entries = [];
        var seen = new Set();
        function addEntry(id, label, provider) {
          id = String(id || '').trim();
          if (!id || seen.has(id)) return;
          seen.add(id);
          entries.push({
            id: id,
            label: String(label || id).trim() || id,
            provider: String(provider || '').trim()
          });
        }
        var fullCatalog = dashboardState.testApiModelOptions || [];
        if (fullCatalog.length) {
          fullCatalog.forEach(function(option) {
            var provider = String(option && option.provider || '').trim();
            var id = String(option && option.id || '').trim();
            var display = String(option && option.display_name || option && option.model || id).trim();
            addEntry(id, display, provider);
          });
        } else {
          (dashboardState.customModelModelOptions || []).forEach(function(option) {
            var provider = String(option && option.provider || '').trim();
            var model = String(option && option.model || '').trim();
            var id = String(option && option.id || '').trim();
            if (!id && provider && model) id = provider + ':' + model;
            var display = String(option && option.display_name || model || id).trim();
            addEntry(id, display, provider);
          });
          (dashboardState.customModels || []).forEach(function(model) {
            var alias = normalizeCustomAlias(model && model.alias);
            if (!alias) return;
            addEntry('ctm:' + alias, model.display_name || alias, 'ctm');
          });
        }
        entries.sort(function(left, right) {
          return (left.provider || '').localeCompare(right.provider || '')
            || left.id.localeCompare(right.id);
        });
        return entries;
      }
      function renderTestApiModelOptions() {
        var list = document.getElementById('testApiModelList');
        var input = document.getElementById('testApiModelInput');
        if (!list || !input) return;
        var entries = testApiModelEntries();
        list.innerHTML = entries.map(function(entry) {
          var providerLabel = providerLabels[entry.provider] || entry.provider || 'Model';
          var label = providerLabel + (entry.label && entry.label !== entry.id ? ' - ' + entry.label : '');
          return '<option value="' + escapeHtml(entry.id) + '" label="' + escapeHtml(label) + '"></option>';
        }).join('');
        if (!input.value.trim() && entries.length) {
          input.value = entries[0].id;
        }
      }
      async function prepareTestApiPanel() {
        if (!dashboardState.testApiModelOptions.length && !dashboardState.customModelModelOptions.length && !dashboardState.customModels.length) {
          setText('testApiStatus', 'Loading model catalog...');
          await refreshCustomModels();
        }
        renderTestApiModelOptions();
        if (document.getElementById('testApiStatus')?.textContent === 'Loading model catalog...') {
          setText('testApiStatus', 'Ready');
        }
      }
      function setTestApiBusy(busy) {
        var button = document.getElementById('sendTestApiBtn');
        if (button) {
          button.disabled = !!busy;
          button.textContent = busy ? 'Sending...' : 'Send Test';
        }
      }
      function renderTestApiResult(data) {
        var result = document.getElementById('testApiResult');
        var output = document.getElementById('testApiOutput');
        var raw = document.getElementById('testApiRaw');
        var meta = document.getElementById('testApiResultMeta');
        if (!result || !output || !raw || !meta) return;
        var status = data && data.status ? 'HTTP ' + data.status : 'HTTP ?';
        var ms = data && data.duration_ms != null ? ' · ' + data.duration_ms + ' ms' : '';
        var model = data && data.model ? ' · ' + data.model : '';
        meta.textContent = (data && data.ok ? 'Success' : 'Failed') + ' · ' + status + ms + model;
        output.textContent = (data && data.output_text) || (data && data.message) || 'No text output returned.';
        raw.textContent = JSON.stringify((data && data.response) || data || {}, null, 2);
        result.hidden = false;
      }
      async function sendTestApi() {
        var model = document.getElementById('testApiModelInput')?.value.trim() || '';
        var prompt = document.getElementById('testApiPromptInput')?.value.trim() || '';
        var instructions = document.getElementById('testApiInstructionsInput')?.value.trim() || '';
        var maxOutput = Number(document.getElementById('testApiMaxOutputInput')?.value || 512);
        if (!model) {
          setText('testApiStatus', 'Model is required.');
          return;
        }
        if (!prompt) {
          setText('testApiStatus', 'Prompt is required.');
          return;
        }
        setTestApiBusy(true);
        setText('testApiStatus', 'Sending test request...');
        try {
          const res = await adminFetch('/admin/test-api', {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({
              model: model,
              prompt: prompt,
              instructions: instructions || undefined,
              max_output_tokens: Number.isFinite(maxOutput) ? maxOutput : 512
            })
          });
          if (!res) return;
          const text = await res.text();
          let data;
          try {
            data = JSON.parse(text);
          } catch (_) {
            data = {
              ok: false,
              status: res.status,
              model: model,
              message: text || ('Test API returned HTTP ' + res.status),
              response: { body: text }
            };
          }
          renderTestApiResult(data);
          setText('testApiStatus', data.message || (data.ok ? 'Test completed' : 'Test failed'));
          if (data.ok) {
            invalidateDashboardSnapshot();
            refreshDashboardSnapshot(true).then(function(snapshot) {
              if (snapshot) renderDashboardSnapshot();
            });
            refreshContextChart();
          }
        } catch (err) {
          var message = err && err.message ? err.message : String(err);
          setText('testApiStatus', 'Test request failed: ' + message);
          renderTestApiResult({
            ok: false,
            status: 0,
            model: model,
            message: message,
            response: { error: message }
          });
        } finally {
          setTestApiBusy(false);
        }
      }
      function clearTestApi() {
        var prompt = document.getElementById('testApiPromptInput');
        var instructions = document.getElementById('testApiInstructionsInput');
        var result = document.getElementById('testApiResult');
        if (prompt) prompt.value = '';
        if (instructions) instructions.value = '';
        if (result) result.hidden = true;
        setText('testApiStatus', 'Ready');
      }
      async function copyTestApiOutput() {
        var value = document.getElementById('testApiOutput')?.textContent || '';
        await copyTextToClipboard(value, 'Test output copied');
      }
      function parseDateTimeValue(value) {
        if (value == null || value === '') return null;
        if (value instanceof Date) {
          return Number.isFinite(value.getTime()) ? value : null;
        }
        var date;
        if (typeof value === 'number') {
          var millis = Math.abs(value) < 100000000000 ? value * 1000 : value;
          date = new Date(millis);
        } else {
          var text = String(value).trim();
          if (!text) return null;
          if (/^-?\d+(?:\.\d+)?$/.test(text)) {
            var numeric = Number(text);
            var numericMillis = Math.abs(numeric) < 100000000000 ? numeric * 1000 : numeric;
            date = new Date(numericMillis);
          } else {
            date = new Date(text);
          }
        }
        return Number.isFinite(date.getTime()) ? date : null;
      }
      function formatDashboardDate(value, fallback) {
        if (value == null || value === '') return fallback || '';
        var date = parseDateTimeValue(value);
        return date ? dashboardDateFormatter.format(date) : String(value);
      }
      function formatDashboardTime(value, fallback) {
        if (value == null || value === '') return fallback || '';
        var date = parseDateTimeValue(value);
        return date ? dashboardTimeFormatter.format(date) : String(value);
      }
      function formatDashboardDateTime(value, fallback) {
        if (value == null || value === '') return fallback || '';
        var date = parseDateTimeValue(value);
        return date ? dashboardDateTimeFormatter.format(date) : String(value);
      }
      function formatSettingsDateTime(value, fallback) {
        return formatDashboardDateTime(value, fallback || 'Never');
      }
	      function apiKeySourceLabel(source) {
	        return source === 'legacy_config' ? 'Legacy config' : 'Managed';
	      }
	      function normalizePromptTokenLimit(value) {
	        var n = Number(value);
	        return Number.isFinite(n) && n > 0 ? Math.floor(n) : null;
	      }
	      function normalizeApiKeyAccess(access) {
	        if (!access || access.all !== false) {
	          return {
	            all: true,
	            prompt_token_limit: normalizePromptTokenLimit(access && access.prompt_token_limit),
	            providers: []
	          };
	        }
	        var providers = Array.isArray(access.providers) ? access.providers : [];
	        return {
	          all: false,
	          prompt_token_limit: normalizePromptTokenLimit(access.prompt_token_limit),
	          providers: providers.map(function(rule) {
	            var accountLimits = Array.isArray(rule && rule.account_limits) ? rule.account_limits : [];
	            return {
	              provider: String(rule && rule.provider || ''),
	              accounts: Array.isArray(rule && rule.accounts) ? rule.accounts.map(String) : [],
	              prompt_token_limit: normalizePromptTokenLimit(rule && rule.prompt_token_limit),
	              account_limits: accountLimits.map(function(limit) {
	                return {
	                  account: String(limit && limit.account || ''),
	                  prompt_token_limit: normalizePromptTokenLimit(limit && limit.prompt_token_limit)
	                };
	              }).filter(function(limit) { return !!limit.account && limit.prompt_token_limit != null; })
	            };
	          }).filter(function(rule) { return !!rule.provider; })
	        };
	      }
      function apiKeyAccessRule(access, provider) {
        return normalizeApiKeyAccess(access).providers.find(function(rule) {
          return rule.provider === provider;
        }) || null;
      }
      function apiKeyAccessSummary(access) {
        access = normalizeApiKeyAccess(access);
        if (access.all) return 'All providers and accounts';
        if (!access.providers.length) return 'No access';
	        return access.providers.map(function(rule) {
	          var label = providerLabels[rule.provider] || rule.provider;
	          var limit = rule.prompt_token_limit ? ', limit ' + rule.prompt_token_limit + ' tok' : '';
	          var accountLimits = rule.account_limits.length
	            ? ', ' + rule.account_limits.length + ' account limit' + (rule.account_limits.length === 1 ? '' : 's')
	            : '';
	          return rule.accounts.length
	            ? label + ' (' + rule.accounts.length + ' account' + (rule.accounts.length === 1 ? '' : 's') + limit + accountLimits + ')'
	            : label + ' (all accounts' + limit + accountLimits + ')';
	        }).join(', ');
	      }
	      function renderApiKeyAccessEditor(access) {
	        access = normalizeApiKeyAccess(access);
	        var mode = document.getElementById('apiKeyAccessModeInput');
	        var groups = document.getElementById('apiKeyAccessGroups');
	        var wholeLimit = document.getElementById('apiKeyPromptLimitInput');
	        if (!mode || !groups) return;
	        if (wholeLimit) wholeLimit.value = access.prompt_token_limit || '';
	        mode.value = access.all ? 'all' : 'restricted';
	        var accounts = Array.isArray(dashboardState.apiKeyAccounts) ? dashboardState.apiKeyAccounts : [];
	        var grouped = {};
        accounts.forEach(function(account) {
          var provider = String(account && account.provider || '');
          if (!provider) return;
          if (!grouped[provider]) grouped[provider] = [];
          grouped[provider].push(account);
        });
	        groups.innerHTML = dashboardProviderKeys.map(function(provider) {
	          var rule = apiKeyAccessRule(access, provider);
	          var allAccounts = !!rule && rule.accounts.length === 0;
	          var allowed = new Set(rule ? rule.accounts : []);
	          var providerLimit = rule && rule.prompt_token_limit ? rule.prompt_token_limit : '';
	          var accountLimits = new Map((rule && rule.account_limits || []).map(function(limit) {
	            return [String(limit.account || ''), limit.prompt_token_limit];
	          }));
	          var providerValue = escapeHtml(provider);
	          var providerAccounts = grouped[provider] || [];
	          var seenKeys = new Set(providerAccounts.map(function(account) { return String(account.key || ''); }));
          var items = providerAccounts.map(function(account) {
            var key = String(account.key || '');
            if (!key) return '';
            var title = account.label || account.account_id || key;
            var meta = [account.account_id, account.credential_file].filter(Boolean).join(' - ');
	            return '<label class="check-row api-key-access-account">'
	              + '<input type="checkbox" data-api-key-access-account data-provider="' + providerValue + '" value="' + escapeHtml(key) + '"'
	              + (allowed.has(key) ? ' checked' : '') + (allAccounts ? ' disabled' : '') + '> '
	              + '<span class="account-choice-text">'
	              + '<span class="account-choice-title">' + escapeHtml(title) + '</span>'
	              + (meta ? '<small title="' + escapeHtml(meta) + '">' + escapeHtml(compactMiddle(meta, 72)) + '</small>' : '')
	              + '</span>'
	              + '<input class="api-key-account-limit-input" type="number" min="1" step="1" inputmode="numeric" data-api-key-account-prompt-limit data-provider="' + providerValue + '" data-account="' + escapeHtml(key) + '" value="' + escapeHtml(accountLimits.get(key) || '') + '" placeholder="limit">'
	              + '</label>';
	          }).join('');
	          if (rule && rule.accounts.length) {
	            items += rule.accounts.filter(function(key) { return !seenKeys.has(key); }).map(function(key) {
	              return '<label class="check-row api-key-access-account">'
	                + '<input type="checkbox" data-api-key-access-account data-provider="' + providerValue + '" value="' + escapeHtml(key) + '" checked> '
	                + '<span class="account-choice-text"><span class="account-choice-title">Unavailable account</span><small title="' + escapeHtml(key) + '">' + escapeHtml(compactMiddle(key, 72)) + '</small></span>'
	                + '<input class="api-key-account-limit-input" type="number" min="1" step="1" inputmode="numeric" data-api-key-account-prompt-limit data-provider="' + providerValue + '" data-account="' + escapeHtml(key) + '" value="' + escapeHtml(accountLimits.get(key) || '') + '" placeholder="limit">'
	                + '</label>';
	            }).join('');
	          }
	          return '<div class="api-key-access-provider">'
	            + '<label class="check-row">'
	            + '<input type="checkbox" data-api-key-access-provider data-provider="' + providerValue + '"'
	            + (allAccounts ? ' checked' : '') + ' onchange="syncApiKeyAccessProvider(this)"> '
	            + '<span>All ' + escapeHtml(providerLabels[provider] || provider) + ' accounts</span>'
	            + '</label>'
	            + '<div class="api-key-limit-row" style="margin-top:8px;">'
	            + '<span class="muted">Provider prompt token limit</span>'
	            + '<input type="number" min="1" step="1" inputmode="numeric" data-api-key-provider-prompt-limit data-provider="' + providerValue + '" value="' + escapeHtml(providerLimit) + '" placeholder="unlimited">'
	            + '</div>'
	            + '<div class="api-key-access-accounts">'
	            + (items || '<span class="muted">No accounts configured</span>')
	            + '</div>'
            + '</div>';
        }).join('');
        groups.hidden = access.all;
      }
      function syncApiKeyAccessProvider(input) {
        var provider = input && input.getAttribute('data-provider');
        document.querySelectorAll('[data-api-key-access-account][data-provider="' + provider + '"]').forEach(function(accountInput) {
          accountInput.disabled = !!input.checked;
        });
      }
      function updateApiKeyAccessMode() {
        var mode = document.getElementById('apiKeyAccessModeInput');
        var groups = document.getElementById('apiKeyAccessGroups');
        if (mode && groups) groups.hidden = mode.value !== 'restricted';
      }
	      function apiKeyAccessFromDom() {
	        var mode = document.getElementById('apiKeyAccessModeInput');
	        var wholeLimit = normalizePromptTokenLimit(document.getElementById('apiKeyPromptLimitInput')?.value);
	        if (!mode || mode.value !== 'restricted') return { all: true, prompt_token_limit: wholeLimit, providers: [] };
	        var providers = [];
	        dashboardProviderKeys.forEach(function(provider) {
	          var allInput = document.querySelector('[data-api-key-access-provider][data-provider="' + provider + '"]');
	          var providerLimit = normalizePromptTokenLimit(document.querySelector('[data-api-key-provider-prompt-limit][data-provider="' + provider + '"]')?.value);
	          var accountLimits = Array.from(document.querySelectorAll('[data-api-key-account-prompt-limit][data-provider="' + provider + '"]'))
	            .map(function(input) {
	              return {
	                account: input.getAttribute('data-account') || '',
	                prompt_token_limit: normalizePromptTokenLimit(input.value)
	              };
	            })
	            .filter(function(limit) { return !!limit.account && limit.prompt_token_limit != null; });
	          if (allInput && allInput.checked) {
	            providers.push({ provider: provider, accounts: [], prompt_token_limit: providerLimit, account_limits: accountLimits });
	            return;
	          }
	          var accounts = Array.from(document.querySelectorAll('[data-api-key-access-account][data-provider="' + provider + '"]:checked'))
	            .map(function(input) { return input.value; })
	            .filter(Boolean);
	          accountLimits.forEach(function(limit) {
	            if (!accounts.includes(limit.account)) accounts.push(limit.account);
	          });
	          if (accounts.length || providerLimit != null || accountLimits.length) {
	            providers.push({
	              provider: provider,
	              accounts: accounts,
	              prompt_token_limit: providerLimit,
	              account_limits: accountLimits
	            });
	          }
	        });
	        if (!providers.length) return null;
	        return { all: false, prompt_token_limit: wholeLimit, providers: providers };
	      }
      function resetApiKeyEditor() {
        dashboardState.apiKeyEditingId = '';
        var label = document.getElementById('apiKeyLabelInput');
        if (label) {
          label.value = '';
          label.disabled = false;
        }
        setText('apiKeyEditorLabel', 'Create API key');
        setText('createApiKeyBtn', 'Create API Key');
        var cancel = document.getElementById('cancelApiKeyEditBtn');
        if (cancel) cancel.hidden = true;
        renderApiKeyAccessEditor({ all: true, providers: [] });
      }
      function editApiKeyAccess(id) {
        var key = (dashboardState.apiKeys || []).find(function(item) { return item.id === id; });
        if (!key || key.revoked_at) return;
        dashboardState.apiKeyEditingId = id;
        var label = document.getElementById('apiKeyLabelInput');
        if (label) {
          label.value = key.label || '';
          label.disabled = true;
        }
        setText('apiKeyEditorLabel', 'Edit access for ' + (key.label || 'API key'));
        setText('createApiKeyBtn', 'Save Access');
        var cancel = document.getElementById('cancelApiKeyEditBtn');
        if (cancel) cancel.hidden = false;
        renderApiKeyAccessEditor(key.access);
        document.getElementById('apiKeyAccessModeInput')?.focus();
      }
      function updateApiKeyStatusText(message) {
        if (message) {
          setText('apiKeyStatus', message);
          return;
        }
        var keys = Array.isArray(dashboardState.apiKeys) ? dashboardState.apiKeys : [];
        var active = keys.filter(function(key) { return !key.revoked_at; }).length;
        setText('apiKeyStatus', active + ' active API key' + (active === 1 ? '' : 's'));
      }
      function setApiKeyReveal(value) {
        var panel = document.getElementById('apiKeyRevealPanel');
        var code = document.getElementById('apiKeyRevealValue');
        if (panel) panel.hidden = !value;
        if (code) code.textContent = value || '';
      }
      function renderApiKeys() {
        var list = document.getElementById('apiKeysList');
        if (!list) return;
        var keys = Array.isArray(dashboardState.apiKeys) ? dashboardState.apiKeys : [];
        if (!keys.length) {
          list.innerHTML = '<div class="empty-state">No API keys available</div>';
          updateApiKeyStatusText();
          return;
        }
        list.innerHTML = keys.map(function(key) {
          var revoked = !!key.revoked_at;
          var keyArg = escapeHtml(jsString(key.id || ''));
          var actions = revoked
            ? '<span class="account-state disabled">Revoked</span>'
            : '<button type="button" class="mini-btn secondary-button" onclick="editApiKeyAccess(' + keyArg + ')">Edit access</button>'
              + '<button type="button" class="mini-btn secondary-button" onclick="revokeApiKey(' + keyArg + ')">Revoke</button>';
          return '<div class="api-key-row' + (revoked ? ' is-revoked' : '') + '">'
            + '<div class="api-key-main">'
            + '<div class="api-key-title-row">'
            + '<strong>' + escapeHtml(key.label || 'API key') + '</strong>'
            + '<code>' + escapeHtml(key.key_prefix || '') + '</code>'
            + '</div>'
            + '<div class="api-key-meta">'
            + escapeHtml(apiKeySourceLabel(key.source))
            + ' · Created ' + escapeHtml(formatSettingsDateTime(key.created_at, 'Unknown'))
            + ' · Last used ' + escapeHtml(formatSettingsDateTime(key.last_used_at, 'Never'))
            + (revoked ? ' · Revoked ' + escapeHtml(formatSettingsDateTime(key.revoked_at, 'Unknown')) : '')
            + '</div>'
            + '<div class="api-key-meta api-key-access-summary">Access: ' + escapeHtml(apiKeyAccessSummary(key.access)) + '</div>'
            + '</div>'
            + '<div class="api-key-actions">' + actions + '</div>'
            + '</div>';
        }).join('');
        updateApiKeyStatusText();
      }
      async function loadApiKeys() {
        setText('apiKeyStatus', 'Loading API keys...');
        const res = await adminFetch('/admin/api-keys');
        if (!res) return;
        const data = await res.json();
        dashboardState.apiKeys = Array.isArray(data.keys) ? data.keys : [];
        dashboardState.apiKeyAccounts = Array.isArray(data.accounts) ? data.accounts : [];
        if (!dashboardState.apiKeyEditingId) renderApiKeyAccessEditor({ all: true, providers: [] });
        renderApiKeys();
      }
      async function createApiKey() {
        var labelInput = document.getElementById('apiKeyLabelInput');
        var label = labelInput ? labelInput.value.trim() : '';
        var access = apiKeyAccessFromDom();
        if (!access) {
          updateApiKeyStatusText('Select at least one provider or account for restricted access.');
          return;
        }
        var editingId = dashboardState.apiKeyEditingId;
        setText('apiKeyStatus', editingId ? 'Saving API key access...' : 'Creating API key...');
        const res = await adminFetch(editingId ? '/admin/api-keys/access' : '/admin/api-keys/create', {
          method: 'POST',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify(editingId
            ? { id: editingId, access: access }
            : { label: label || undefined, access: access })
        });
        if (!res) return;
        const data = await res.json();
        if (!data.ok) {
          updateApiKeyStatusText(data.message || 'Failed to create API key');
          return;
        }
        dashboardState.apiKeys = Array.isArray(data.keys) ? data.keys : dashboardState.apiKeys;
        dashboardState.apiKeyAccounts = Array.isArray(data.accounts) ? data.accounts : dashboardState.apiKeyAccounts;
        if (!editingId) setApiKeyReveal(data.plain_text_key || '');
        resetApiKeyEditor();
        renderApiKeys();
        updateApiKeyStatusText(editingId
          ? 'API key access updated'
          : 'New API key created. Copy it now; it will not be shown again.');
      }
      async function revokeApiKey(id) {
        if (!id) return;
        setText('apiKeyStatus', 'Revoking API key...');
        const res = await adminFetch('/admin/api-keys/revoke', {
          method: 'POST',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify({ id: id })
        });
        if (!res) return;
        const data = await res.json();
        if (!data.ok) {
          updateApiKeyStatusText(data.message || 'Failed to revoke API key');
          return;
        }
        dashboardState.apiKeys = Array.isArray(data.keys) ? data.keys : dashboardState.apiKeys;
        dashboardState.apiKeyAccounts = Array.isArray(data.accounts) ? data.accounts : dashboardState.apiKeyAccounts;
        if (dashboardState.apiKeyEditingId === id) resetApiKeyEditor();
        renderApiKeys();
        updateApiKeyStatusText('API key revoked');
      }
      async function copyApiKeyReveal() {
        var value = document.getElementById('apiKeyRevealValue')?.textContent || '';
        if (!value) return;
        try {
          await navigator.clipboard.writeText(value);
          updateApiKeyStatusText('API key copied');
        } catch (_) {
          updateApiKeyStatusText('Copy failed. Use manual copy.');
        }
      }
	      function openAppSettingsModal(tab) {
	        setAppSettingsTab(tab || 'dashboard');
	        setApiKeyReveal('');
	        resetApiKeyEditor();
	        renderProviderSettingsList();
	        renderTestApiModelOptions();
        loadNotificationSettings();
        loadApiKeys();
	        setMobileNavOpen(false);
	        openModal('appSettingsModal');
	      }
	      function openTestApiPanel() {
	        openAppSettingsModal('test-api');
	      }
      function closeAppSettingsModal() {
        closeModal('appSettingsModal');
      }
      function formatNumber(value) {
        return Number(value || 0).toLocaleString();
      }
      function jsString(value) {
        return JSON.stringify(String(value || ''));
      }
      function setText(id, value) {
        const el = document.getElementById(id);
        if (el) el.textContent = value;
      }
      function compactMiddle(value, maxLength) {
        var text = String(value || '');
        var limit = Math.max(12, Number(maxLength) || 24);
        if (text.length <= limit) return text;
        var keep = Math.max(4, Math.floor((limit - 3) / 2));
        return text.slice(0, keep) + '...' + text.slice(text.length - keep);
      }
      function previewText(value, maxLength) {
        var text = String(value || '').replace(/\s+/g, ' ').trim();
        var limit = Math.max(24, Number(maxLength) || 120);
        return text.length > limit ? text.slice(0, limit - 1).trimEnd() + '...' : text;
      }
      async function copyTextToClipboard(value, successMessage) {
        if (!value) return;
        try {
          await navigator.clipboard.writeText(String(value));
          notify(successMessage || 'Copied', 'success');
        } catch (_) {
          notify('Copy failed. Use manual copy.', 'error');
        }
      }
      function accountLabel(a) {
        return (a && (a.name || a.label || a.email || a.account_id)) || 'Account';
      }
      function accountKey(a) {
        return (a && (a.file_name || a.label || a.email || a.account_id || a.name)) || '';
      }
      function routingProviderKey(provider) {
        var value = String(provider || '').toLowerCase();
        if (value === 'cod' || value === 'codex') return 'codex';
        if (value === 'agw' || value === 'antigravity') return 'antigravity';
        if (value === 'gem' || value === 'gemini') return 'gemini';
        if (value === 'qwn' || value === 'qwen') return 'qwen';
        if (value === 'dsk' || value === 'deepseek') return 'deepseek';
        if (value === 'grk' || value === 'grok') return 'grok';
        if (value === 'min' || value === 'minimax') return 'minimax';
        if (value === 'cop' || value === 'copilot') return 'copilot';
        if (value === 'cld' || value === 'claude') return 'claude';
        if (value === 'glm') return 'glm';
        return value;
      }
      function priorityAccountKey(provider, a) {
        provider = routingProviderKey(provider);
        a = a || {};
        if (provider === 'codex') {
          if (a.account_id) return 'codex:account_id:' + a.account_id;
          if (a.file_name) return 'codex:file:' + a.file_name;
          return 'codex:label:' + (a.label || '');
        }
        if (provider === 'antigravity') {
          if (a.email) return 'agw:email:' + a.email;
          if (a.file_name) return 'agw:file:' + a.file_name;
          if (a.project_id) return 'agw:project:' + a.project_id;
          return 'agw:label:' + (a.label || '');
        }
        if (provider === 'gemini') {
          if (a.email && a.project_id) return 'gemini:email:' + a.email + '|project:' + a.project_id;
          if (a.email) return 'gemini:email:' + a.email;
          if (a.file_name) return 'gemini:file:' + a.file_name;
          return 'gemini:label:' + (a.label || '');
        }
        if (provider === 'qwen') {
          if (a.subject) return 'qwen:subject:' + a.subject;
          if (a.account_id) return 'qwen:subject:' + a.account_id;
          if (a.email) return 'qwen:email:' + a.email;
          if (a.file_name) return 'qwen:file:' + a.file_name;
          if (a.resource_url) return 'qwen:resource:' + a.resource_url;
          return 'qwen:label:' + (a.label || '');
        }
        if (provider === 'deepseek') {
          if (a.account_id) return 'deepseek:account_id:' + a.account_id;
          if (a.file_name) return 'deepseek:file:' + a.file_name;
          return 'deepseek:label:' + (a.label || '');
        }
        if (provider === 'grok') {
          if (a.user_id) return 'grok:user_id:' + a.user_id;
          if (a.email) return 'grok:email:' + a.email;
          if (a.file_name) return 'grok:file:' + a.file_name;
          return 'grok:label:' + (a.label || '');
        }
        if (provider === 'minimax') {
          if (a.account_id) return 'minimax:account_id:' + a.account_id;
          if (a.file_name) return 'minimax:file:' + a.file_name;
          return 'minimax:label:' + (a.label || '');
        }
        if (provider === 'copilot') {
          if (a.account_id) return 'copilot:account_id:' + a.account_id;
          if (a.login) return 'copilot:login:' + a.login;
          if (a.file_name) return 'copilot:file:' + a.file_name;
          return 'copilot:label:' + (a.label || '');
        }
        if (provider === 'claude') {
          if (a.organization_uuid) return 'claude:organization:' + a.organization_uuid;
          if (a.account_id) return 'claude:account_id:' + a.account_id;
          if (a.email) return 'claude:email:' + a.email;
          if (a.file_name) return 'claude:file:' + a.file_name;
          return 'claude:label:' + (a.label || '');
        }
        if (provider === 'glm') {
          if (a.account_id) return 'glm:account_id:' + a.account_id;
          if (a.file_name) return 'glm:file:' + a.file_name;
          return 'glm:label:' + (a.label || '');
        }
        return accountKey(a);
      }
      function isPriorityAccount(provider, a) {
        var providerKey = routingProviderKey(provider);
        var providerSettings = dashboardState.accountRouting
          && dashboardState.accountRouting.providers
          && dashboardState.accountRouting.providers[providerKey];
        var accounts = providerSettings && Array.isArray(providerSettings.priority_accounts)
          ? providerSettings.priority_accounts
          : [];
        return accounts.indexOf(priorityAccountKey(provider, a)) !== -1;
      }
      function isExpired(value) {
        if (!value) return false;
        const ts = Date.parse(value);
        return Number.isFinite(ts) && ts < Date.now();
      }
      function parseTimestamp(value) {
        if (!value) return NaN;
        const ts = Date.parse(value);
        return Number.isFinite(ts) ? ts : NaN;
      }
      function hasCurrentError(account) {
        if (Number(account && account.errors || 0) <= 0) return false;
        const lastError = parseTimestamp(account && account.last_error_at);
        const lastSuccess = parseTimestamp(account && account.last_success_at);
        if (Number.isFinite(lastError) && Number.isFinite(lastSuccess) && lastSuccess > lastError) {
          return false;
        }
        return true;
      }
      function quotaForAccount(provider, a) {
        const map = dashboardState.quotas[provider];
        if (!map) return null;
        return map.get(accountKey(a)) || map.get(a && a.label) || map.get(a && a.file_name) || map.get(a && a.email) || null;
      }
      function pushQuotaPercent(values, bucket) {
        if (bucket && bucket.used_percent != null) {
          values.push(Number(bucket.used_percent));
        }
      }
      function modelQuotaBucket(model) {
        return model && (model.current || model.quota || model.limit || null);
      }
      function quotaBucketHasValue(bucket) {
        return !!bucket && (bucket.used_percent != null || bucket.limit != null || bucket.remaining != null || bucket.limit_text || bucket.remaining_text || bucket.used_text);
      }
      function geminiModelFamily(model) {
        var raw = model && (model.model_id || model.id || model.slug || model.model || model.model_name || model.name || model.display_name) || '';
        var lower = String(raw).toLowerCase();
        if (lower.indexOf('gemini') === -1) return '';
        if (lower.indexOf('flash-lite') !== -1 || lower.indexOf('flash_lite') !== -1 || lower.indexOf('flash lite') !== -1) return 'Flash Lite';
        if (lower.indexOf('flash') !== -1) return 'Flash';
        if (lower.indexOf('pro') !== -1) return 'Pro';
        return '';
      }
      function geminiFamilySortValue(label) {
        return label === 'Flash' ? 0 : label === 'Flash Lite' ? 1 : label === 'Pro' ? 2 : 99;
      }
      function geminiModelFamilySummaries(quota) {
        if (!quota || !Array.isArray(quota.models)) return [];
        var families = {};
        quota.models.forEach(function(model) {
          var family = geminiModelFamily(model);
          var bucket = modelQuotaBucket(model);
          if (!family || !quotaBucketHasValue(bucket)) return;
          if (!families[family]) {
            families[family] = {
              label: family,
              models: [],
              bucket: {
                used_percent: null,
                remaining_percent: null,
                reset_label: ''
              }
            };
          }
          var summary = families[family];
          summary.models.push(model.model_id || model.id || model.model || model.display_name || family);
          var remaining = Number(bucket.remaining_percent);
          var used = Number(bucket.used_percent);
          if (!Number.isFinite(used) && Number.isFinite(remaining)) {
            used = 100 - remaining;
          }
          if (Number.isFinite(used) && (summary.bucket.used_percent == null || used > summary.bucket.used_percent)) {
            summary.bucket.used_percent = used;
            summary.bucket.reset_label = bucket.reset_label || summary.bucket.reset_label || '';
          } else if (!summary.bucket.reset_label && bucket.reset_label) {
            summary.bucket.reset_label = bucket.reset_label;
          }
          if (Number.isFinite(remaining) && (summary.bucket.remaining_percent == null || remaining < summary.bucket.remaining_percent)) {
            summary.bucket.remaining_percent = remaining;
          }
        });
        return Object.keys(families)
          .map(function(key) { return families[key]; })
          .filter(function(item) { return item.bucket.used_percent != null || item.bucket.remaining_percent != null; })
          .sort(function(a, b) { return geminiFamilySortValue(a.label) - geminiFamilySortValue(b.label); });
      }
      function accountQuotaPercents(provider, quota) {
        const values = [];
        if (!quota) return values;
        if (quota.code_generation) {
          pushQuotaPercent(values, quota.code_generation.five_hour);
          pushQuotaPercent(values, quota.code_generation.weekly);
        }
        if (quota.code_review) {
          pushQuotaPercent(values, quota.code_review.five_hour);
          pushQuotaPercent(values, quota.code_review.weekly);
        }
        if (quota.current_window) pushQuotaPercent(values, quota.current_window);
        if (quota.weekly) pushQuotaPercent(values, quota.weekly);
        if (quota.groups && Array.isArray(quota.groups)) {
          quota.groups.forEach(function(group) {
            pushQuotaPercent(values, group.five_hour);
            pushQuotaPercent(values, group.weekly);
          });
        }
        if (quota.additional_rate_limits && Array.isArray(quota.additional_rate_limits)) {
          quota.additional_rate_limits.forEach(function(limit) {
            pushQuotaPercent(values, limit.five_hour);
            pushQuotaPercent(values, limit.weekly);
          });
        }
        if (provider === 'gemini') {
          geminiModelFamilySummaries(quota).forEach(function(summary) {
            pushQuotaPercent(values, summary.bucket);
          });
        }
        if (quota.limits && Array.isArray(quota.limits)) {
          quota.limits.forEach(function(limit) {
            if (limit && limit.used_percent != null) values.push(Number(limit.used_percent));
          });
        }
        if (provider === 'grok' && quota.kinds) {
          Object.keys(quota.kinds).forEach(function(kindName) {
            const kind = quota.kinds[kindName];
            const limits = kind && kind.rate_limits;
            if (!limits) return;
            Object.keys(limits).forEach(function(limitName) {
              const limit = limits[limitName];
              if (!limit || limit.limit == null || Number(limit.limit) <= 0) return;
              const remaining = limit.remaining != null ? Number(limit.remaining) : Number(limit.limit);
              values.push(100 * (Number(limit.limit) - remaining) / Number(limit.limit));
            });
          });
        }
        return values.filter(Number.isFinite);
      }
      function maxAccountQuotaPercent(provider, a) {
        const quota = quotaForAccount(provider, a);
        const values = accountQuotaPercents(provider, quota);
        return values.length ? Math.max.apply(Math, values) : 0;
      }
      function accountAttentionItems(item) {
        const provider = item.provider;
        const account = item.account || {};
        const items = [];
        if (account.enabled === false) {
          items.push({
            key: 'disabled',
            title: 'Disabled account',
            detail: 'This account is disabled and will not be selected until it is enabled.'
          });
        }
        if (isExpired(account.expired_at)) {
          items.push({
            key: 'expired tokens',
            title: 'Saved token expired',
            detail: 'Saved token expired at ' + formatDashboardDateTime(account.expired_at, account.expired_at) + '. Re-authenticate this account before using it.'
          });
        }
        if (hasCurrentError(account)) {
          var errorDetail = 'Total recorded errors: ' + formatNumber(account.errors || 0) + '.';
          if (account.last_error_at) errorDetail += ' Last error: ' + formatDashboardDateTime(account.last_error_at, account.last_error_at) + '.';
          if (account.last_error_message) errorDetail += ' Detail: ' + account.last_error_message + '.';
          if (account.last_success_at) errorDetail += ' Last success: ' + formatDashboardDateTime(account.last_success_at, account.last_success_at) + '.';
          items.push({
            key: 'errors',
            title: 'Current errors',
            detail: errorDetail
          });
        }
        var maxQuota = maxAccountQuotaPercent(provider, account);
        if (maxQuota >= 75) {
          items.push({
            key: 'near quota',
            title: maxQuota >= 95 ? 'Quota almost exhausted' : 'Quota usage is high',
            detail: 'Highest tracked quota usage is ' + maxQuota.toFixed(1) + '%. Requests may be throttled or fail until the quota resets.'
          });
        }
        return items;
      }
      function accountAttentionReasons(item) {
        return accountAttentionItems(item).map(function(reason) { return reason.key; });
      }
      function allAccountsWithProvider() {
        const out = [];
        Object.keys(dashboardState.providers).forEach(function(provider) {
          (dashboardState.providers[provider] || []).forEach(function(account) {
            out.push({ provider: provider, account: account });
          });
        });
        return out;
      }
      function updateOverview() {
        const accounts = allAccountsWithProvider();
        const totalAccounts = accounts.length;
        const providersWithAccounts = Object.keys(dashboardState.providers)
          .filter(function(provider) { return (dashboardState.providers[provider] || []).length > 0; }).length;
        const attentionItems = accounts.map(function(item) {
          return { item: item, reasons: accountAttentionReasons(item) };
        }).filter(function(entry) { return entry.reasons.length > 0; });
        const errorAccounts = attentionItems.filter(function(entry) { return entry.reasons.indexOf('errors') !== -1; }).length;
        const attentionCount = attentionItems.length;
        setText('overviewRequests', formatNumber(dashboardState.totalRequests));
        setText('overviewErrors', formatNumber(dashboardState.totalErrors));
        setText('overviewErrorNote', errorAccounts ? errorAccounts + ' accounts reporting errors' : 'No account errors reported');
        setText('overviewAccounts', formatNumber(totalAccounts));
        setText('overviewProviderNote', providersWithAccounts + ' active providers');
        setText('overviewAttention', formatNumber(attentionCount));
        setText('overviewAttentionNote', attentionCount ? 'Review highlighted items' : 'No obvious issues');
        setText('pageSubtitle', totalAccounts
          ? totalAccounts + ' accounts across ' + providersWithAccounts + ' providers'
          : 'No accounts loaded yet');
        applyProviderRowArrangement();
      }
      function notify(message, tone, timeoutMs) {
        const toast = document.getElementById('toast');
        if (!toast) return;
        toast.textContent = message || '';
        toast.className = 'toast show'
          + (tone === 'error' ? ' error' : '')
          + (tone === 'success' ? ' success' : '');
        clearTimeout(notify.timer);
        notify.timer = setTimeout(function() {
          toast.className = 'toast';
        }, Number(timeoutMs) || 4200);
      }
      function refreshCredentialViews() {
        invalidateDashboardSnapshot();
        refreshDashboardSnapshot(true).then(function() {
          renderDashboardSnapshot();
        });
        refreshCustomModels();
      }
      function accountActionMenuId(fileName) {
        var value = String(fileName || '');
        var hash = 0;
        for (var i = 0; i < value.length; i++) {
          hash = ((hash * 31) + value.charCodeAt(i)) >>> 0;
        }
        return 'account-action-menu-' + hash.toString(36);
      }
      function closeAccountActionMenus(exceptId) {
        document.querySelectorAll('.account-action-menu').forEach(function(menu) {
          if (exceptId && menu.id === exceptId) return;
          menu.hidden = true;
          var button = menu.parentElement && menu.parentElement.querySelector('.account-menu-button');
          if (button) button.setAttribute('aria-expanded', 'false');
        });
      }
      function toggleAccountActionMenu(event, menuId) {
        if (event) event.stopPropagation();
        var menu = document.getElementById(menuId);
        if (!menu) return;
        var shouldOpen = menu.hidden;
        closeAccountActionMenus(menuId);
        menu.hidden = !shouldOpen;
        var button = menu.parentElement && menu.parentElement.querySelector('.account-menu-button');
        if (button) button.setAttribute('aria-expanded', shouldOpen ? 'true' : 'false');
      }
      function closeCredentialActionConfirm() {
        pendingCredentialAction = null;
        closeModal('confirmActionModal');
      }
      function openCredentialActionConfirm(options) {
        closeAccountActionMenus();
        pendingCredentialAction = options || null;
        var title = document.getElementById('confirmActionTitle');
        var message = document.getElementById('confirmActionMessage');
        var approve = document.getElementById('confirmActionApproveBtn');
        if (title) title.textContent = options.title || 'Confirm action';
        if (message) message.textContent = options.message || '';
        if (approve) {
          approve.textContent = options.approveLabel || 'Approve';
          approve.className = options.danger ? 'danger' : '';
        }
        openModal('confirmActionModal');
      }
      async function approveCredentialAction() {
        var action = pendingCredentialAction;
        if (!action || typeof action.run !== 'function') {
          closeCredentialActionConfirm();
          return;
        }
        var approve = document.getElementById('confirmActionApproveBtn');
        var reject = document.getElementById('confirmActionRejectBtn');
        if (approve) approve.disabled = true;
        if (reject) reject.disabled = true;
        try {
          await action.run();
          closeCredentialActionConfirm();
        } finally {
          if (approve) approve.disabled = false;
          if (reject) reject.disabled = false;
        }
      }
      function normalizeTheme(theme) {
        return theme === 'light' ? 'light' : 'dark';
      }
      function readStoredTheme() {
        try {
          return localStorage.getItem(THEME_KEY);
        } catch (_) {
          return null;
        }
      }
      function writeStoredTheme(theme) {
        try {
          localStorage.setItem(THEME_KEY, theme);
        } catch (_) {}
      }
      function closeProviderMenu() {
        const menu = document.getElementById('providerMenu');
        const button = document.getElementById('addProviderBtn');
        if (menu) {
          menu.hidden = true;
        }
        if (button) {
          button.setAttribute('aria-expanded', 'false');
        }
      }
      function setMobileNavOpen(open) {
        const header = document.querySelector('.page-header');
        const button = document.getElementById('mobileMenuBtn');
        if (header) header.classList.toggle('mobile-nav-open', !!open);
        if (button) {
          button.setAttribute('aria-expanded', open ? 'true' : 'false');
          button.setAttribute('aria-label', open ? 'Close navigation menu' : 'Open navigation menu');
        }
        if (!open) closeProviderMenu();
      }
      function configureMobileNav() {
        const button = document.getElementById('mobileMenuBtn');
        if (!button) return;
        button.addEventListener('click', function() {
          const expanded = button.getAttribute('aria-expanded') === 'true';
          setMobileNavOpen(!expanded);
        });
        window.addEventListener('resize', function() {
          if (!window.matchMedia('(max-width: 768px)').matches) {
            setMobileNavOpen(false);
          }
        });
      }
      function setDashboardInactive(inactive) {
        const content = document.getElementById('dashboardContent');
        if (!content) return;
        if (inactive) {
          content.setAttribute('aria-hidden', 'true');
          content.setAttribute('inert', '');
        } else {
          content.removeAttribute('aria-hidden');
          content.removeAttribute('inert');
        }
      }
      function anyVisibleModal() {
        return modalIds.some(function(id) {
          const el = document.getElementById(id);
          return el && el.style.display !== 'none';
        });
      }
      function adminGateVisible() {
        const gate = document.getElementById('adminLoginGate');
        return !!gate && gate.style.display !== 'none';
      }
      function ensureModalCloseButton(id) {
        if (id === 'confirmActionModal') return;
        const modal = document.getElementById(id);
        const card = modal ? modal.querySelector('.modal-card') : null;
        if (!card || card.querySelector('.modal-close-button')) return;
        const button = document.createElement('button');
        button.type = 'button';
        button.className = 'modal-close-button secondary-button';
        button.setAttribute('aria-label', 'Close dialog');
        button.textContent = '\u00d7';
        button.addEventListener('click', function() {
          closeModal(id);
        });
        card.insertBefore(button, card.firstChild);
      }
      function openModal(id) {
        closeProviderMenu();
        modalIds.forEach(function(modalId) {
          if (modalId !== id) closeModal(modalId, true);
        });
        const modal = document.getElementById(id);
        if (!modal) return;
        ensureModalCloseButton(id);
        modal.style.display = 'block';
        modal.setAttribute('aria-hidden', 'false');
        setDashboardInactive(true);
        setTimeout(function() {
          const focusTarget = modal.querySelector('input, textarea, select, button:not(.modal-close-button), [tabindex]:not([tabindex="-1"])');
          if (focusTarget) focusTarget.focus();
        }, 0);
      }
      function closeModal(id, keepBackgroundInactive) {
        const modal = document.getElementById(id);
        if (!modal) return;
        modal.style.display = 'none';
        modal.setAttribute('aria-hidden', 'true');
        if (!keepBackgroundInactive && !anyVisibleModal() && !adminGateVisible()) {
          setDashboardInactive(false);
        }
      }
      function showAdminLogin(message) {
        adminAuthenticated = false;
        adminAuthEpoch += 1;
        closeProviderMenu();
        const gate = document.getElementById('adminLoginGate');
        gate.style.display = 'block';
        gate.setAttribute('aria-hidden', 'false');
        setDashboardInactive(true);
        document.getElementById('logoutBtn').style.display = 'none';
        document.getElementById('adminLoginStatus').textContent = message || '';
      }
      function hideAdminLogin() {
        const gate = document.getElementById('adminLoginGate');
        gate.style.display = 'none';
        gate.setAttribute('aria-hidden', 'true');
        document.getElementById('adminLoginStatus').textContent = '';
        if (!anyVisibleModal() && !adminGateVisible()) {
          setDashboardInactive(false);
        }
        if (adminAuthEnabled) {
          document.getElementById('logoutBtn').style.display = 'inline-block';
        }
      }
      function dashboardSnapshotPayload(url) {
        if (!dashboardSnapshot) return undefined;
        const providers = dashboardSnapshot.providers || {};
        const quotas = dashboardSnapshot.quotas || {};
        const routeMap = {
          '/dashboard.json': dashboardSnapshot.dashboard,
          '/quota.json': quotas.codex,
          '/agw/accounts.json': providers.agw,
          '/agw/quota.json': quotas.agw,
          '/gemini/accounts.json': providers.gemini,
          '/gemini/quota.json': quotas.gemini,
          '/qwen/accounts.json': providers.qwen,
          '/qwen/quota.json': quotas.qwen,
          '/deepseek/accounts.json': providers.deepseek,
          '/deepseek/quota.json': quotas.deepseek,
          '/minimax/accounts.json': providers.minimax,
          '/minimax/quota.json': quotas.minimax,
          '/grok/accounts.json': providers.grok,
          '/grok/quota.json': quotas.grok,
          '/copilot/accounts.json': providers.copilot,
          '/copilot/quota.json': quotas.copilot,
          '/claude/accounts.json': providers.claude,
          '/claude/quota.json': quotas.claude,
          '/glm/accounts.json': providers.glm,
          '/glm/quota.json': quotas.glm,
          '/admin/account-routing': dashboardSnapshot.account_routing
        };
        return routeMap[url];
      }
      function invalidateDashboardSnapshot() {
        dashboardSnapshot = null;
        dashboardSnapshotAt = 0;
      }
      async function refreshDashboardSnapshot(force) {
        if (!force && dashboardSnapshot && Date.now() - dashboardSnapshotAt < 8000) {
          return dashboardSnapshot;
        }
        if (dashboardSnapshotRequest) return dashboardSnapshotRequest;
        const requestEpoch = adminAuthEpoch;
        dashboardSnapshotRequest = fetch('/dashboard/snapshot.json', { credentials: 'same-origin' })
          .then(async function(res) {
            if (res.status === 401) {
              if (requestEpoch === adminAuthEpoch) showAdminLogin('Session expired. Log in again.');
              return null;
            }
            if (!res.ok) throw new Error('Dashboard snapshot returned ' + res.status);
            const data = await res.json();
            dashboardSnapshot = data;
            dashboardSnapshotAt = Date.now();
            return data;
          })
          .catch(function(err) {
            console.error('dashboard snapshot refresh failed', err);
            return null;
          })
          .finally(function() {
            dashboardSnapshotRequest = null;
          });
        return dashboardSnapshotRequest;
      }
      async function adminFetch(url, options) {
        const method = String(options && options.method || 'GET').toUpperCase();
        const cached = method === 'GET' ? dashboardSnapshotPayload(url) : undefined;
        if (cached !== undefined) {
          return new Response(JSON.stringify(cached), {
            status: 200,
            headers: { 'Content-Type': 'application/json' }
          });
        }
        const requestEpoch = adminAuthEpoch;
        const res = await fetch(url, Object.assign({ credentials: 'same-origin' }, options || {}));
        if (res.status === 401) {
          if (requestEpoch === adminAuthEpoch) {
            showAdminLogin('Session expired. Log in again.');
          }
          return null;
        }
        return res;
      }
      async function bootstrapAdmin() {
        const res = await fetch('/admin/session', { credentials: 'same-origin' });
        const data = await res.json();
        adminAuthEnabled = !!data.enabled;
        adminAuthenticated = !!data.authenticated || !adminAuthEnabled;
        if (!adminAuthEnabled) {
          hideAdminLogin();
          startDashboard();
          return;
        }
        if (!data.configured) {
          showAdminLogin('Admin auth is enabled but not configured. Set admin_auth.totp_secret or ADMIN_AUTH_TOTP_SECRET.');
          return;
        }
        if (adminAuthenticated) {
          hideAdminLogin();
          startDashboard();
          return;
        }
        showAdminLogin('Enter your current Google Authenticator code.');
      }
      function setThemeToggleLabel(theme) {
        const btn = document.getElementById('themeToggleBtn');
        if (!btn) return;
        btn.textContent = theme === 'light' ? 'Theme: Light' : 'Theme: Dark';
      }
      function applyTheme(theme) {
        const resolved = normalizeTheme(theme);
        document.documentElement.setAttribute('data-theme', resolved);
        setThemeToggleLabel(resolved);
        return resolved;
      }
      function loadTheme() {
        return applyTheme(readStoredTheme());
      }
      function toggleTheme() {
        const current = normalizeTheme(document.documentElement.getAttribute('data-theme'));
        const next = current === 'dark' ? 'light' : 'dark';
        writeStoredTheme(next);
        applyTheme(next);
      }
      loadTheme();
      function showTapTip(el, ev) {
        if (ev) {
          ev.preventDefault();
          ev.stopPropagation();
        }
        const text = el.getAttribute('data-tip') || el.getAttribute('title') || '';
        if (!text) return;
        if (activeTipEl) {
          activeTipEl.remove();
          activeTipEl = null;
        }
        const tip = document.createElement('div');
        tip.className = 'tap-tip';
        tip.textContent = text;
        document.body.appendChild(tip);
        const rect = el.getBoundingClientRect();
        const margin = 8;
        let left = rect.left + (rect.width / 2) - (tip.offsetWidth / 2);
        left = Math.max(margin, Math.min(left, window.innerWidth - tip.offsetWidth - margin));
        let top = rect.bottom + 8;
        if (top + tip.offsetHeight > window.innerHeight - margin) {
          top = rect.top - tip.offsetHeight - 8;
        }
        tip.style.left = (left + window.scrollX) + 'px';
        tip.style.top = (top + window.scrollY) + 'px';
        activeTipEl = tip;
        if (activeTipTimer) clearTimeout(activeTipTimer);
        activeTipTimer = setTimeout(() => {
          if (activeTipEl) {
            activeTipEl.remove();
            activeTipEl = null;
          }
        }, 2500);
      }
      document.addEventListener('click', () => {
        if (activeTipEl) {
          activeTipEl.remove();
          activeTipEl = null;
        }
      });
      function toggleAgwModelRow(key) {
        if (openAgwRows.has(key)) {
          openAgwRows.delete(key);
        } else {
          openAgwRows.add(key);
        }
        refreshAgwAccounts();
      }
      function toggleGeminiModelRow(key) {
        if (openGeminiRows.has(key)) {
          openGeminiRows.delete(key);
        } else {
          openGeminiRows.add(key);
        }
        refreshGeminiAccounts();
      }
      function toggleQwenModelRow(key) {
        if (openQwenRows.has(key)) {
          openQwenRows.delete(key);
        } else {
          openQwenRows.add(key);
        }
        refreshQwenAccounts();
      }
      function renderQuotaBars(quota, options) {
        if (!quota) return '';
        options = options || {};
        function fmtQ(b, fallback) {
          return b && b.used_percent != null ? b.used_percent.toFixed(1) + '% ' + (b.reset_label || '') : (fallback || '...');
        }
        function resetHint(label) {
          var value = String(label || '').trim();
          if (!value || value === '\u2014') return 'resets in \u2014';
          return /^resets?\s+in\b/i.test(value) ? value : ('resets in ' + value);
        }
        function pctValue(value) {
          var pct = Number(value);
          if (!Number.isFinite(pct)) return 0;
          return Math.max(0, Math.min(100, pct));
        }
        function pctClass(pct) {
          return pct > 80 ? 'high' : pct > 50 ? 'mid' : 'low';
        }
        function mixChannel(start, end, ratio) {
          return Math.round(start + (end - start) * ratio);
        }
        function mixRgb(from, to, ratio) {
          return 'rgb('
            + mixChannel(from[0], to[0], ratio) + ', '
            + mixChannel(from[1], to[1], ratio) + ', '
            + mixChannel(from[2], to[2], ratio) + ')';
        }
        function quotaUsageColor(pct) {
          var value = pctValue(pct);
          var green = [34, 197, 94];
          var amber = [245, 158, 11];
          var red = [239, 68, 68];
          if (value <= 50) return mixRgb(green, amber, value / 50);
          return mixRgb(amber, red, (value - 50) / 50);
        }
        function quotaToneColor(tone, pct) {
          if (tone === 'low') return '#22c55e';
          if (tone === 'mid') return '#f59e0b';
          if (tone === 'high') return '#ef4444';
          return quotaUsageColor(pct);
        }
        function bucketPct(bucket) {
          return bucket && bucket.used_percent != null ? bucket.used_percent : 0;
        }
        function renderProgressBar(label, hint, pct, tone, detailTitle) {
          var safeLabel = escapeHtml(label || 'Usage');
          var safeHint = escapeHtml(hint || '...');
          var safeTitle = safeLabel + ' ' + safeHint + (detailTitle ? ' - ' + escapeHtml(detailTitle) : '');
          var width = pctValue(pct);
          var cls = tone || pctClass(width);
          var color = quotaToneColor(tone, width);
          return '<div class="quota-bar-wrap"><div class="quota-bar" title="' + safeTitle + '">'
            + '<div class="quota-bar-fill ' + cls + '" style="width:' + width + '%; --quota-bar-color:' + color + ';"></div>'
            + '<div class="quota-bar-text"><span>' + safeLabel + '</span><span>' + safeHint + '</span></div>'
            + '</div></div>';
        }
        function renderProgressPair(label, fiveHour, weekly) {
          if (!fiveHour && !weekly) return '';
          return '<div class="quota-pair-wrap">'
            + renderProgressBar((label || 'Usage') + ' 5h', fmtQ(fiveHour, 'N/A'), bucketPct(fiveHour), 'five-hour')
            + renderProgressBar((label || 'Usage') + ' Weekly', fmtQ(weekly, 'N/A'), bucketPct(weekly), 'weekly')
            + '</div>';
        }
        function renderModelLimitDetails() {
          if (!quota.models || !quota.models.length) return '';
          var modelBars = '';
          quota.models.forEach(function(m) {
            var b = modelQuotaBucket(m);
            if (!quotaBucketHasValue(b)) {
              return;
            }
            modelBars += renderProgressBar(m.display_name || m.model_id || 'Model', fmtQ(b, 'N/A'), bucketPct(b));
          });
          return modelBars ? '<div class="model-quota-details">' + modelBars + '</div>' : '';
        }
        function renderModelLimitToggle(expanded) {
          if (!quota.models || !quota.models.length) return '';
          var toggleFn = options.provider === 'gemini' ? 'toggleGeminiModelRow' : 'toggleAgwModelRow';
          var key = options.key || '';
          var label = expanded ? 'Hide model limits' : 'Show model limits (' + quota.models.length + ')';
          return '<div class="model-limit-toggle"><button class="mini-btn secondary-button" onclick="' + toggleFn + '(' + escapeHtml(JSON.stringify(key)) + ')">' + label + '</button></div>';
        }
        function renderGeminiFamilyLimits() {
          var summaries = geminiModelFamilySummaries(quota);
          if (!summaries.length) return '';
          return summaries.map(function(summary) {
            var title = summary.models.length ? summary.models.join(', ') : summary.label;
            return renderProgressBar('Gemini ' + summary.label, fmtQ(summary.bucket, 'N/A'), bucketPct(summary.bucket), null, title);
          }).join('');
        }
        var bars = '';
        var provider = options.provider || '';
        var hideProviderModels = provider === 'agw' || provider === 'gemini';
        var expanded = provider === 'agw'
          ? openAgwRows.has(options.key || '')
          : provider === 'gemini'
            ? openGeminiRows.has(options.key || '')
            : false;
        // Codex-style quota: keep 5h and weekly stacked together.
        if (quota.code_generation) {
          var cg5 = quota.code_generation.five_hour, cgw = quota.code_generation.weekly;
          var cr5 = quota.code_review?.five_hour, crw = quota.code_review?.weekly;
          bars += renderProgressPair('Code Gen', cg5, cgw);
          bars += renderProgressPair('Code Review', cr5, crw);
        }
        if (quota.additional_rate_limits) {
          quota.additional_rate_limits.forEach(function(limit) {
            bars += renderProgressPair(limit.display_name || limit.limit_name || 'Model limit', limit.five_hour, limit.weekly);
          });
        }
        // Provider model limits. For Antigravity/Gemini these are hidden
        // by default and can be expanded per account.
        if (quota.models && !hideProviderModels) {
          quota.models.forEach(function(m) {
            var b = modelQuotaBucket(m);
            if (!quotaBucketHasValue(b)) {
              return;
            }
            bars += renderProgressBar(m.display_name || m.model_id || 'Model', fmtQ(b, 'N/A'), bucketPct(b));
          });
        }
        if (provider === 'gemini') {
          bars += renderGeminiFamilyLimits();
        }
        // Provider-style group quota (AGW/Gemini): default view keeps
        // the 5h + weekly pairs for each quota group.
        if (quota.groups) {
          quota.groups.forEach(function(g) {
            bars += renderProgressPair(g.display_name || 'Group', g.five_hour, g.weekly);
          });
        }
        if (hideProviderModels) {
          bars += renderModelLimitToggle(expanded);
          if (expanded) bars += renderModelLimitDetails();
        }
        // Qwen-style rate limits
        if (quota.limits) {
          quota.limits.forEach(function(l) {
            var label = l.label || l.scope || 'Limit';
            var pct = l.used_percent != null ? l.used_percent : 0;
            var hint = (l.used_text || l.used || '') + '/' + (l.limit_text || l.limit || '') + ' ' + (l.reset_label || '');
            bars += renderProgressBar(label, hint, pct);
          });
        }
        // Grok-style "kinds" snapshot from /grok/quota.json. Mirrors the
        // JoshuaWang2211/grok-usage-watch data model (requestKind + modelName +
        // remaining/total) and adds the cost_in_usd_ticks from the probe.
        if (quota.kinds) {
          function grokBar(kind, opts) {
            if (!kind) return '';
            var rl = kind.rate_limits || {};
            var pieces = [];
            if (opts && opts.showRequests !== false) {
              var rq = rl.requests;
              if (rq && rq.limit != null) {
                pieces.push({ label: 'requests', hint: (rq.remaining != null ? rq.remaining : '?') + '/' + rq.limit + ' (no reset info)', pct: rq.limit > 0 ? (100 * (rq.limit - (rq.remaining != null ? rq.remaining : rq.limit)) / rq.limit) : 0 });
              }
            }
            if (opts && opts.showTokens) {
              var tk = rl.tokens;
              if (tk && tk.limit != null) {
                pieces.push({ label: 'tokens', hint: (tk.remaining != null ? Math.round(tk.remaining) : '?') + '/' + tk.limit + ' (no reset info)', pct: tk.limit > 0 ? (100 * (tk.limit - (tk.remaining != null ? tk.remaining : tk.limit)) / tk.limit) : 0 });
              }
            }
            var parts = pieces.map(function(p) {
              return renderProgressBar(p.label, p.hint, pctValue(p.pct));
            }).join('');
            return parts;
          }
          var t = quota.kinds.DEFAULT_TEXT, i = quota.kinds.DEFAULT_IMAGE, v = quota.kinds.DEFAULT_VIDEO;
          if (t) bars += '<div class="quota-kind-group">' + grokBar(t, { showRequests: true, showTokens: true }) + '</div>';
          if (i) bars += '<div class="quota-kind-group">' + grokBar(i, { showRequests: true }) + '</div>';
          if (v) bars += '<div class="quota-kind-group">' + grokBar(v, { showRequests: true }) + '</div>';
          // Show a compact cost summary
          var costPieces = [];
          if (t && t.cost_in_usd_ticks != null) costPieces.push('text ' + (t.cost_in_usd_ticks / 1e6).toFixed(4) + ' ¢');
          if (i && i.cost_in_usd_ticks != null) costPieces.push('image ' + (i.cost_in_usd_ticks / 1e6).toFixed(4) + ' ¢');
          if (v && v.cost_in_usd_ticks != null) costPieces.push('video ' + (v.cost_in_usd_ticks / 1e6).toFixed(4) + ' ¢');
          var noteLines = [];
          if (costPieces.length) {
            noteLines.push('probe cost: ' + costPieces.join(' | '));
          }
          // xAI's consumer OAuth does not send x-ratelimit-reset-* headers, so
          // we can't show a "resets in" countdown the way the Codex / MiniMax
          // cards do. Surface that limitation once at the bottom of the
          // per-kind block instead of leaving the user wondering.
          noteLines.push('xAI OAuth does not expose reset time; quota resets on an opaque schedule.');
          if (quota.status_msg) noteLines.push(quota.status_msg);
          bars += '<details class="quota-notes muted"><summary>Quota notes</summary>'
            + noteLines.map(function(line) { return '<div class="quota-cost-line">' + escapeHtml(line) + '</div>'; }).join('')
            + '</details>';
        }
        // MiniMax top-level current_window / weekly (matches the
        // platform.minimax.io/console/usage layout: two big bars per
        // account: "5h" and "Weekly" with a "resets in" countdown).
        if (quota.current_window) {
          var cw = quota.current_window;
          var cwPct = cw.used_percent != null ? cw.used_percent : 0;
          var cwHint = (cw.used_percent != null ? cw.used_percent.toFixed(1) + '%' : '\u2014')
            + ' used \u00b7 ' + resetHint(cw.reset_label);
          bars += renderProgressBar('5h window', cwHint, cwPct);
        }
        if (quota.weekly) {
          var wk = quota.weekly;
          var wkPct = wk.used_percent != null ? wk.used_percent : 0;
          var wkHint = (wk.used_percent != null ? wk.used_percent.toFixed(1) + '%' : '\u2014')
            + ' used \u00b7 ' + resetHint(wk.reset_label);
          bars += renderProgressBar('Weekly window', wkHint, wkPct);
        }
        // Balance tile: providers expose a balances list when they have a
        // balance endpoint (DeepSeek, GLM api_usage when Z.AI exposes one).
        // When balances is empty but a balance_note is present, we still
        // render a tile so the dashboard surfaces the precise reason
        // (e.g. Z.AI does not expose a public balance endpoint for
        // pay-as-you-go keys) instead of leaving the account looking
        // identical to providers that don't track balance at all.
        var hasBalances = quota.balances && quota.balances.length;
        var hasBalanceNote = !!quota.balance_note;
        if (hasBalances || hasBalanceNote) {
          var accountType = options && options.provider ? String(options.provider).toLowerCase() : '';
          var balanceHtml = '';
          if (hasBalances) {
            quota.balances.forEach(function(b) {
              if (b.total_balance == null) return;
              var label = 'Balance ' + (b.currency || 'USD');
              var detail = b.total_balance + ' ' + (b.currency || 'USD');
              if (b.topped_up_balance && b.topped_up_balance !== '0.00' && b.topped_up_balance !== '0') {
                detail += ' (topped up ' + b.topped_up_balance + ')';
              } else if (b.granted_balance && b.granted_balance !== '0.00' && b.granted_balance !== '0') {
                detail += ' (granted ' + b.granted_balance + ')';
              }
              balanceHtml += renderProgressBar(label, detail, 100, 'low');
            });
          }
          if (hasBalanceNote) {
            balanceHtml += '<div class="muted quota-status">'
              + escapeHtml(quota.balance_note)
              + '</div>';
          }
          if (balanceHtml) {
            var tileLabel = hasBalances
              ? 'Balance'
              : ('Balance (' + (accountType === 'glm' ? 'api_usage' : accountType || 'account') + ')');
            bars += '<div class="quota-balance-block">'
              + '<div class="quota-balance-label">' + escapeHtml(tileLabel) + '</div>'
              + balanceHtml
              + '</div>';
          }
        }
        if (quota.status_msg && !quota.kinds && !(hasBalances || hasBalanceNote)) {
          bars += '<div class="muted quota-status">' + escapeHtml(quota.status_msg) + '</div>';
        }
        return bars ? '<div class="card-quota">' + bars + '</div>' : '';
      }
      function formatResetCreditExpiry(credit) {
        if (!credit || !credit.expires_at) return 'No expiration';
        var formatted = formatDashboardDateTime(credit.expires_at, credit.expires_at);
        return 'Expires at ' + formatted;
      }
      function resetCreditDisplayName(credit) {
        if (!credit) return 'Usage limit reset';
        return credit.title || credit.description || credit.id || 'Usage limit reset';
      }
      function renderCodexResetCredits(a, quota) {
        var summary = quota && quota.rate_limit_reset_credits;
        if (!summary) return '';
        var label = accountLabel(a);
        var fileArg = escapeHtml(jsString(a && a.file_name || ''));
        var labelArg = escapeHtml(jsString(label));
        var accountArg = escapeHtml(jsString(a && a.account_id || ''));
        var key = accountDetailStateKey('codex', a, a && (a.file_name || a.label || a.account_id) || label);
        var credits = Array.isArray(summary.credits) ? summary.credits.filter(function(credit) {
          var status = String(credit && credit.status || 'available').toLowerCase();
          return credit && status === 'available';
        }) : [];
        var count = Number(summary.available_count);
        if (!Number.isFinite(count)) count = credits.length;
        count = Math.max(0, Math.max(count, credits.length));
        var html = '<details class="reset-credit-details muted"' + detailToggleAttrs('resetCredits', key) + '><summary>Available reset limit <span class="reset-credit-count">(' + count + ')</span></summary>';
        if (count <= 0) {
          return html + '<div class="quota-status reset-credit-empty">No usage limit resets available</div></details>';
        }
        if (!credits.length) {
          return html
            + '<div class="reset-credit-list"><div class="reset-credit-item">'
            + '<div class="reset-credit-main"><div class="reset-credit-title">Next available reset credit</div><div class="reset-credit-meta">Credit details are not available from the upstream summary.</div></div>'
            + '<button type="button" class="mini-btn secondary-button" onclick="redeemCodexReset(' + fileArg + ', ' + labelArg + ', ' + accountArg + ', \'\', \'next available reset\')">Reset limit</button>'
            + '</div></div></details>';
        }
        html += '<div class="reset-credit-list">';
        html += credits.map(function(credit) {
          var title = resetCreditDisplayName(credit);
          var expiry = formatResetCreditExpiry(credit);
          var id = String(credit.id || '');
          var idArg = escapeHtml(jsString(id));
          var idHtml = id
            ? '<div class="reset-credit-id-row">'
              + '<code title="' + escapeHtml(id) + '">' + escapeHtml(compactMiddle(id, 34)) + '</code>'
              + '<button type="button" class="mini-btn secondary-button" onclick="copyTextToClipboard(' + idArg + ', \'Reset credit ID copied\')">Copy ID</button>'
              + '</div>'
            : '';
          return '<div class="reset-credit-item">'
            + '<div class="reset-credit-main"><div class="reset-credit-title">' + escapeHtml(title) + '</div><div class="reset-credit-meta">' + escapeHtml(expiry) + '</div>' + idHtml + '</div>'
            + '<button type="button" class="mini-btn secondary-button" onclick="redeemCodexReset(' + fileArg + ', ' + labelArg + ', ' + accountArg + ', ' + escapeHtml(jsString(credit.id || '')) + ', ' + escapeHtml(jsString(title)) + ')">Reset limit</button>'
            + '</div>';
        }).join('');
        if (count > credits.length) {
          html += '<div class="quota-status reset-credit-empty">+' + (count - credits.length) + ' more available reset credits are not listed by the upstream response.</div>';
        }
        return html + '</div></details>';
      }
      function escapeHtml(value) {
        return String(value).replace(/[&<>"']/g, function(ch) {
          return ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' })[ch];
        });
      }
      function renderAccountState(a) {
        const enabled = !a || a.enabled !== false;
        const runtime = a && a.runtime;
        const runtimeStatus = runtime && String(runtime.status || '').toLowerCase();
        const isCooldown = enabled && runtimeStatus === 'cooldown';
        const cls = isCooldown ? 'cooldown' : (enabled ? 'enabled' : 'disabled');
        const label = isCooldown ? 'Cooldown' : (enabled ? 'Enabled' : 'Disabled');
        var title = '';
        if (isCooldown) {
          title = 'Router cooldown';
          if (runtime.cooldown_remaining_seconds != null) {
            title += ' - ' + runtime.cooldown_remaining_seconds + 's remaining';
          }
        }
        const attrs = title ? ' title="' + escapeHtml(title) + '" aria-label="' + escapeHtml(title) + '"' : '';
        return '<span class="account-state ' + cls + '"' + attrs + '>'
          + label
          + '</span>';
      }
      function renderAttentionBadge(provider, a) {
        const reasons = accountAttentionReasons({ provider: provider, account: a }).filter(function(reason) {
          return reason !== 'disabled';
        });
        if (!reasons.length) return '';
        const label = reasons.join(' / ');
        const title = 'Attention: ' + label;
        return '<span class="attention-account-badge" title="' + escapeHtml(title) + '" aria-label="' + escapeHtml(title) + '">' + escapeHtml(label) + '</span>';
      }
      function renderPriorityBadge(provider, a) {
        if (!isPriorityAccount(provider, a)) return '';
        return '<span class="account-state enabled" title="Priority routing" aria-label="Priority routing">Priority</span>';
      }
      function renderCardHeader(title, badgesHtml, actionsHtml) {
        return '<div class="card-header">'
          + '<div class="card-identity">'
          + '<span class="card-email">' + escapeHtml(title) + '</span>'
          + '<div class="card-badges">' + badgesHtml + '</div>'
          + '</div>'
          + '<span class="card-actions">' + (actionsHtml || '') + '</span>'
          + '</div>';
      }
      function renderAccountHeader(title, provider, a) {
        return renderCardHeader(
          title,
          renderAccountState(a) + renderPriorityBadge(provider, a) + renderAttentionBadge(provider, a),
          renderAccountActions(provider, a)
        );
      }
      function renderAccountActions(provider, a) {
        if (!a) {
          return '';
        }
        const label = accountLabel(a);
        const isEnabled = a.enabled !== false;
        const isCooldown = isEnabled && a.runtime && String(a.runtime.status || '').toLowerCase() === 'cooldown';
        const toggleLabel = isEnabled ? 'Disable' : 'Enable';
        const toggleClass = isEnabled ? 'danger' : 'is-enabled';
        const accountKey = priorityAccountKey(provider, a);
        if (!accountKey) {
          return '';
        }
        const hasFile = !!a.file_name;
        const menuId = accountActionMenuId(a.file_name || (String(provider || '') + ':' + accountKey));
        const menuArg = escapeHtml(jsString(menuId));
        const fileArg = escapeHtml(jsString(a.file_name || ''));
        const labelArg = escapeHtml(jsString(label));
        const providerArg = escapeHtml(jsString(provider || ''));
        const accountKeyArg = escapeHtml(jsString(accountKey));
        const isPriority = isPriorityAccount(provider, a);
        const priorityAction = isEnabled
          ? '<button type="button" role="menuitem" aria-label="' + escapeHtml((isPriority ? 'Remove priority from ' : 'Use first ') + label) + '" onclick="toggleAccountPriority(' + providerArg + ', ' + accountKeyArg + ', ' + (isPriority ? 'false' : 'true') + ')" class="mini-btn action-btn">' + (isPriority ? 'Remove priority' : 'Use first') + '</button>'
          : '';
        const refreshAction = '<button type="button" role="menuitem" aria-label="' + escapeHtml('Refresh models and quota for ' + label) + '" onclick="refreshModelCatalog(' + providerArg + ', ' + (hasFile ? fileArg : "''") + ')" class="mini-btn action-btn">Refresh models + quota</button>';
        const forceEnableAction = hasFile && isCooldown
          ? '<button type="button" role="menuitem" aria-label="' + escapeHtml('Force enable ' + label) + '" onclick="toggleCred(' + fileArg + ', true, ' + labelArg + ')" class="mini-btn action-btn is-enabled">Force enable</button>'
          : '';
        const fileActions = hasFile
          ? '<button type="button" role="menuitem" aria-label="' + escapeHtml(toggleLabel + ' ' + label) + '" onclick="toggleCred(' + fileArg + ', ' + (isEnabled ? 'false' : 'true') + ', ' + labelArg + ')" class="mini-btn action-btn ' + toggleClass + '">' + toggleLabel + '</button>'
            + '<button type="button" role="menuitem" aria-label="' + escapeHtml('Delete ' + label) + '" onclick="deleteCred(' + fileArg + ', ' + labelArg + ')" class="mini-btn action-btn danger">Delete</button>'
          : '';
        return '<span class="account-menu-wrap">'
          + '<button type="button" class="mini-btn account-menu-button" aria-label="' + escapeHtml('Open actions for ' + label) + '" aria-haspopup="menu" aria-expanded="false" aria-controls="' + escapeHtml(menuId) + '" onclick="toggleAccountActionMenu(event, ' + menuArg + ')">&#8942;</button>'
          + '<span id="' + escapeHtml(menuId) + '" class="account-action-menu" role="menu" hidden onclick="event.stopPropagation()">'
          + refreshAction
          + priorityAction
          + forceEnableAction
          + fileActions
          + '</span>'
          + '</span>';
      }
      function renderCustomModelActions(alias, title) {
        const normalizedAlias = normalizeCustomAlias(alias);
        const label = title || ('ctm:' + normalizedAlias);
        const menuId = accountActionMenuId('custom-model:' + normalizedAlias);
        const menuArg = escapeHtml(jsString(menuId));
        const aliasArg = escapeHtml(jsString(normalizedAlias));
        return '<span class="account-menu-wrap">'
          + '<button type="button" class="mini-btn account-menu-button" aria-label="' + escapeHtml('Open actions for ' + label) + '" aria-haspopup="menu" aria-expanded="false" aria-controls="' + escapeHtml(menuId) + '" onclick="toggleAccountActionMenu(event, ' + menuArg + ')">&#8942;</button>'
          + '<span id="' + escapeHtml(menuId) + '" class="account-action-menu" role="menu" hidden onclick="event.stopPropagation()">'
          + '<button type="button" role="menuitem" aria-label="' + escapeHtml('Edit ' + label) + '" onclick="openCustomModelModal(' + aliasArg + ')" class="mini-btn action-btn">Edit</button>'
          + '<button type="button" role="menuitem" aria-label="' + escapeHtml('Delete ' + label) + '" onclick="deleteCustomModel(' + aliasArg + ')" class="mini-btn action-btn danger">Delete</button>'
          + '</span>'
          + '</span>';
      }
      function renderCustomModelHeader(title, model) {
        return renderCardHeader(
          title,
          renderAccountState({ enabled: model && model.enabled }),
          renderCustomModelActions(model && model.alias, title)
        );
      }
      function accountDetailStateKey(provider, a, fallback) {
        return provider + ':' + (accountKey(a) || fallback || accountLabel(a));
      }
      function isAccountDetailOpen(kind, key) {
        const store = openAccountDetails[kind];
        return !!store && store.has(key);
      }
      function trackAccountDetail(el, kind, key) {
        const store = openAccountDetails[kind];
        if (!store) return;
        if (el.open) {
          store.add(key);
        } else {
          store.delete(key);
        }
      }
      function detailToggleAttrs(kind, key) {
        if (!kind || !key) return '';
        return (isAccountDetailOpen(kind, key) ? ' open' : '')
          + ' ontoggle="trackAccountDetail(this, ' + escapeHtml(jsString(kind)) + ', ' + escapeHtml(jsString(key)) + ')"';
      }
      function renderMetaDetails(rows, title, provider, a, fallbackKey) {
        if (!rows || !rows.length) return '';
        var key = accountDetailStateKey(provider || 'account', a, fallbackKey);
        return '<details class="account-meta muted"' + detailToggleAttrs('connection', key) + '><summary>' + escapeHtml(title || 'Details') + '</summary><div class="meta-list">'
          + rows.join('')
          + '</div></details>';
      }
      function renderAttentionDetails(provider, a, fallbackKey) {
        var key = accountDetailStateKey(provider || 'account', a, fallbackKey);
        var items = accountAttentionItems({ provider: provider, account: a });
        var html = '<details class="account-attention-details muted"' + detailToggleAttrs('attention', key) + '><summary>Attention needed <span class="attention-count">(' + items.length + ')</span></summary>';
        if (!items.length) {
          return html + '<div class="attention-empty">No account attention needed from the available dashboard data.</div></details>';
        }
        html += '<div class="attention-list">';
        html += items.map(function(item) {
          return '<div class="attention-item">'
            + '<div class="attention-title">' + escapeHtml(item.title) + '</div>'
            + '<div class="attention-detail">' + escapeHtml(item.detail) + '</div>'
            + '</div>';
        }).join('');
        return html + '</div></details>';
      }
      function renderMetaLine(label, value, code) {
        if (value == null || value === '') return '';
        return '<div><span>' + escapeHtml(label) + ': </span>'
          + (code ? '<code>' + escapeHtml(value) + '</code>' : escapeHtml(value))
          + '</div>';
      }
      function renderDateTimeMetaLine(label, value) {
        if (value == null || value === '') return '';
        return renderMetaLine(label, formatDashboardDateTime(value, ''), false);
      }
      function renderLongMetaLine(label, value) {
        if (value == null || value === '') return '';
        var text = String(value);
        return '<details class="meta-raw-details">'
          + '<summary>' + escapeHtml(label) + '<span class="meta-raw-preview">' + escapeHtml(previewText(text, 120)) + '</span></summary>'
          + '<pre class="meta-raw-block">' + escapeHtml(text) + '</pre>'
          + '</details>';
      }
      function firstPresent(object, keys) {
        if (!object) return '';
        for (var i = 0; i < keys.length; i++) {
          var value = object[keys[i]];
          if (value != null && value !== '') return value;
        }
        return '';
      }
      function connectionRows(provider, a, quota) {
        var rows = [];
        var label = providerLabels[provider] || provider || 'Provider';
        rows.push(renderMetaLine('provider', label, false));
        rows.push(renderMetaLine('credential file', firstPresent(a, ['file_name']), true));
        rows.push(renderMetaLine('label', firstPresent(a, ['label', 'name']), false));
        rows.push(renderMetaLine('login', firstPresent(a, ['login']), false));
        rows.push(renderMetaLine('email', firstPresent(a, ['email']), false));
        rows.push(renderMetaLine('account id', firstPresent(a, ['account_id', 'subject']), true));
        rows.push(renderMetaLine('organization uuid', firstPresent(a, ['organization_uuid']), true));
        rows.push(renderMetaLine('package', firstPresent(quota, ['plan_type']), false));
        rows.push(renderMetaLine('account type', firstPresent(a, ['account_type']), false));
        rows.push(renderMetaLine('project id', firstPresent(a, ['project_id']), true));
        rows.push(renderMetaLine('resource URL', firstPresent(a, ['resource_url', 'base_url', 'api_base_url']), true));
        rows.push(renderMetaLine('openai base URL', firstPresent(a, ['openai_base_url']), true));
        rows.push(renderMetaLine('anthropic base URL', firstPresent(a, ['anthropic_base_url']), true));
        rows.push(renderDateTimeMetaLine('saved token expiry', firstPresent(a, ['expired_at'])));
        rows.push(renderDateTimeMetaLine('copilot token expiry', firstPresent(a, ['copilot_expires_at'])));
        rows.push(renderDateTimeMetaLine('last success', firstPresent(a, ['last_success_at'])));
        rows.push(renderDateTimeMetaLine('last error', firstPresent(a, ['last_error_at'])));
        rows.push(renderLongMetaLine('last error detail', firstPresent(a, ['last_error_message'])));
        rows.push(renderMetaLine('user id', firstPresent(a, ['user_id']), true));
        rows.push(renderMetaLine('team id', a && a.team_id ? a.team_id + (a.team_blocked ? ' (blocked)' : '') : '', true));
        rows.push(renderMetaLine('zdr', firstPresent(a, ['zdr_status']), true));
        return rows.filter(Boolean);
      }
      function modelLabel(model) {
        if (!model) return '';
        if (typeof model === 'string') return model.trim();
        var id = model.model_id || model.id || model.slug || model.model || model.model_name || model.name || '';
        return String(id).trim();
      }
      function modelEntry(model) {
        var label = modelLabel(model);
        if (!label) return null;
        if (typeof model === 'string') {
          return { label: label };
        }
        return {
          label: label,
          displayName: model.display_name || model.name || label,
          vendor: model.vendor || '',
          preview: model.preview === true,
          billingTier: String(model.billing_tier || model.billing_class || '').trim(),
          premium: typeof model.premium === 'boolean' ? model.premium : null,
          utilityModel: model.utility_model === true,
          category: String(model.model_picker_category || '').trim(),
          policyState: String(model.policy_state || '').trim()
        };
      }
      function appendModelEntries(out, seen, models) {
        if (!models || !models.length) return;
        models.forEach(function(model) {
          var entry = modelEntry(model);
          if (!entry || seen.has(entry.label)) return;
          seen.add(entry.label);
          out.push(entry);
        });
      }
      function copilotModelGroup(entry) {
        var tier = String(entry.billingTier || '').toLowerCase();
        if (tier === 'premium' || entry.premium === true) return 'premium';
        if (entry.utilityModel || tier === 'non_premium' || tier === 'non-premium' || entry.premium === false) return 'non_premium';
        return 'unknown';
      }
      function renderModelBadge(text, cls) {
        return text ? '<span class="model-badge ' + cls + '">' + escapeHtml(text) + '</span>' : '';
      }
      function renderCopilotModelChip(entry) {
        var group = copilotModelGroup(entry);
        var titleParts = [];
        if (entry.displayName && entry.displayName !== entry.label) titleParts.push(entry.displayName);
        if (entry.vendor) titleParts.push(entry.vendor);
        if (entry.category) titleParts.push(entry.category);
        if (entry.policyState) titleParts.push('policy ' + entry.policyState);
        var badge = group === 'premium'
          ? renderModelBadge('premium', 'model-badge-premium')
          : group === 'non_premium'
            ? renderModelBadge('non-premium', 'model-badge-non-premium')
            : renderModelBadge('unclassified', 'model-badge-unknown');
        if (entry.utilityModel) badge += renderModelBadge('utility', 'model-badge-non-premium');
        if (entry.category) badge += renderModelBadge(entry.category, 'model-badge-category');
        if (entry.policyState) badge += renderModelBadge(entry.policyState, 'model-badge-policy');
        if (entry.preview) badge += renderModelBadge('preview', 'model-badge-category');
        return '<span class="model-chip" title="' + escapeHtml(titleParts.join(' | ')) + '"><span class="model-chip-name">' + escapeHtml(entry.label) + '</span>' + badge + '</span>';
      }
      function renderCopilotModelGroups(entries) {
        var groups = [
          { key: 'premium', title: 'Premium' },
          { key: 'non_premium', title: 'Non-premium' },
          { key: 'unknown', title: 'Unclassified' }
        ];
        var html = '<div class="model-list model-list-grouped">';
        groups.forEach(function(group) {
          var items = entries.filter(function(entry) { return copilotModelGroup(entry) === group.key; });
          if (!items.length) return;
          html += '<div class="model-group"><div class="model-group-title">' + group.title + ' (' + items.length + ')</div><div class="model-chip-list">'
            + items.map(renderCopilotModelChip).join('')
            + '</div></div>';
        });
        return html + '</div>';
      }
      function modelSummaryCounts(entries, provider) {
        if (provider !== 'copilot') return '';
        var premium = 0;
        var nonPremium = 0;
        var unknown = 0;
        entries.forEach(function(entry) {
          var group = copilotModelGroup(entry);
          if (group === 'premium') premium += 1;
          else if (group === 'non_premium') nonPremium += 1;
          else unknown += 1;
        });
        var parts = [];
        if (premium) parts.push(premium + ' premium');
        if (nonPremium) parts.push(nonPremium + ' non-premium');
        if (unknown) parts.push(unknown + ' unclassified');
        return parts.length ? ' - ' + parts.join(', ') : '';
      }
      function renderModelList(entries, provider) {
        if (provider === 'copilot') {
          return renderCopilotModelGroups(entries);
        }
        return '<span class="model-list">' + entries.map(function(entry) {
          return escapeHtml(entry.label);
        }).join(' | ') + '</span>';
      }
      function renderAccountModels(a, quota, provider, fallbackKey) {
        var entries = [];
        var seen = new Set();
        appendModelEntries(entries, seen, a && a.models);
        appendModelEntries(entries, seen, quota && quota.available_models);
        appendModelEntries(entries, seen, quota && quota.models);
        appendModelEntries(entries, seen, quota && quota.data);
        var key = accountDetailStateKey(provider || 'account', a, fallbackKey);
        return entries.length
          ? '<details class="muted account-models"' + detailToggleAttrs('models', key) + '><summary>Models <span class="model-count">(' + entries.length + modelSummaryCounts(entries, provider) + ')</span></summary>' + renderModelList(entries, provider) + '</details>'
          : '';
      }
      function buildCard(a, quota) {
        var key = a.file_name || a.label;
        return '<div class="card">'
          + renderAccountHeader(accountLabel(a), 'codex', a)
          + '<div class="stat-pills">'
          + '<span class="stat-pill"><span class="stat-pill-value">' + (a.requests || 0) + '</span><span class="stat-pill-label">req</span></span>'
          + '<span class="stat-pill"><span class="stat-pill-value">' + (a.errors || 0) + '</span><span class="stat-pill-label">err</span></span>'
          + '</div>'
          + renderQuotaBars(quota, { provider: 'codex', key: key })
          + renderCodexResetCredits(a, quota)
          + renderAccountModels(a, quota, 'codex', key)
          + renderMetaDetails(connectionRows('codex', a, quota), 'Connection details', 'codex', a, key)
          + renderAttentionDetails('codex', a, key)
          + '</div>';
      }
      async function refresh() {
        const res = await adminFetch('/dashboard.json');
        if (!res) return;
        const data = await res.json();
        const accounts = data.accounts || [];
        dashboardState.totalRequests = data.total_requests || 0;
        dashboardState.totalErrors = data.total_errors || 0;
        dashboardState.providers.codex = accounts;
        var cards = accounts.map(function(a) { return buildCard(a, lastQuota.get(a.file_name || a.label)); }).join('');
        renderProviderCards('codex', 'codexCards', 'codexBadgeCount', accounts, cards, 'No Codex accounts');
        updateOverview();
      }
      async function refreshQuota() {
        const res = await adminFetch('/quota.json');
        if (!res) return;
        const quota = await res.json();
        const quotaMap = new Map();
        (quota.accounts || []).forEach(q => {
          const key = q.file_name || q.label;
          quotaMap.set(key, q);
        });
        lastQuota = quotaMap;
        dashboardState.quotas.codex = quotaMap;
        updateOverview();
        refresh();
      }
      function buildProviderCard(a, quota, provider) {
        var key = a.file_name || a.label || a.email || a.account_id || '';
        var extra = '';
        if (a.email) extra += '<span class="stat-pill"><span class="stat-pill-label">email</span><span class="stat-pill-value">' + escapeHtml(a.email) + '</span></span>';
        if (a.project_id) extra += '<span class="stat-pill"><span class="stat-pill-label">project</span><span class="stat-pill-value"><code>' + escapeHtml(a.project_id) + '</code></span></span>';
        return '<div class="card">'
          + renderAccountHeader(a.label || a.email || a.account_id || 'N/A', provider, a)
          + '<div class="stat-pills">'
          + '<span class="stat-pill"><span class="stat-pill-value">' + (a.requests || 0) + '</span><span class="stat-pill-label">req</span></span>'
          + '<span class="stat-pill"><span class="stat-pill-value">' + (a.errors || 0) + '</span><span class="stat-pill-label">err</span></span>'
          + extra + '</div>'
          + renderQuotaBars(quota, { provider: provider, key: key })
          + renderAccountModels(a, quota, provider, key)
          + renderMetaDetails(connectionRows(provider, a, quota), 'Connection details', provider, a, key)
          + renderAttentionDetails(provider, a, key)
          + '</div>';
      }
      function buildQwenCard(a, quota) {
        var usage = '';
        usage += '<span class="stat-pill"><span class="stat-pill-value">' + (a.requests || 0) + '</span><span class="stat-pill-label">req</span></span>';
        usage += '<span class="stat-pill"><span class="stat-pill-value">' + (a.errors || 0) + '</span><span class="stat-pill-label">err</span></span>';
        usage += '<span class="stat-pill"><span class="stat-pill-value">' + (a.prompt_total || 0) + '</span><span class="stat-pill-label">prompt</span></span>';
        usage += '<span class="stat-pill"><span class="stat-pill-value">' + (a.input_tokens || 0) + '</span><span class="stat-pill-label">in tok</span></span>';
        usage += '<span class="stat-pill"><span class="stat-pill-value">' + (a.output_tokens || 0) + '</span><span class="stat-pill-label">out tok</span></span>';
        usage += '<span class="stat-pill"><span class="stat-pill-value">' + (a.total_tokens || 0) + '</span><span class="stat-pill-label">total tok</span></span>';
        if (a.cache_tokens) {
          usage += '<span class="stat-pill"><span class="stat-pill-value">' + a.cache_tokens + '</span><span class="stat-pill-label">cache tok</span></span>';
        }
        if (a.reasoning_tokens) {
          usage += '<span class="stat-pill"><span class="stat-pill-value">' + a.reasoning_tokens + '</span><span class="stat-pill-label">reason tok</span></span>';
        }
        if (a.email) {
          usage += '<span class="stat-pill"><span class="stat-pill-label">email</span><span class="stat-pill-value">' + escapeHtml(a.email) + '</span></span>';
        }
        return '<div class="card">'
          + renderAccountHeader(a.label || a.email || a.account_id || 'N/A', 'qwen', a)
          + '<div class="stat-pills">' + usage + '</div>'
          + renderQuotaBars(quota, { provider: 'qwen', key: a.file_name || a.label || a.email || a.account_id || '' })
          + renderAccountModels(a, quota, 'qwen', a.file_name || a.label || a.email || a.account_id || '')
          + renderMetaDetails(connectionRows('qwen', a, quota), 'Connection details', 'qwen', a, a.file_name || a.label || a.email || a.account_id || '')
          + renderAttentionDetails('qwen', a, a.file_name || a.label || a.email || a.account_id || '')
          + '</div>';
      }
      function buildGrokCard(a, quota) {
        var usage = '';
        usage += '<span class="stat-pill"><span class="stat-pill-value">' + (a.requests || 0) + '</span><span class="stat-pill-label">req</span></span>';
        usage += '<span class="stat-pill"><span class="stat-pill-value">' + (a.errors || 0) + '</span><span class="stat-pill-label">err</span></span>';
        usage += '<span class="stat-pill"><span class="stat-pill-value">' + (a.prompt_total || 0) + '</span><span class="stat-pill-label">prompt</span></span>';
        usage += '<span class="stat-pill"><span class="stat-pill-value">' + (a.input_tokens || 0) + '</span><span class="stat-pill-label">in tok</span></span>';
        usage += '<span class="stat-pill"><span class="stat-pill-value">' + (a.output_tokens || 0) + '</span><span class="stat-pill-label">out tok</span></span>';
        usage += '<span class="stat-pill"><span class="stat-pill-value">' + (a.total_tokens || 0) + '</span><span class="stat-pill-label">total tok</span></span>';
        if (a.reasoning_tokens) {
          usage += '<span class="stat-pill"><span class="stat-pill-value">' + a.reasoning_tokens + '</span><span class="stat-pill-label">reason tok</span></span>';
        }
        if (a.cache_tokens) {
          usage += '<span class="stat-pill"><span class="stat-pill-value">' + a.cache_tokens + '</span><span class="stat-pill-label">cache tok</span></span>';
        }
        if (a.email) {
          usage += '<span class="stat-pill"><span class="stat-pill-label">email</span><span class="stat-pill-value">' + escapeHtml(a.email) + '</span></span>';
        }
        if (a.last_effective_model) {
          usage += '<span class="stat-pill"><span class="stat-pill-label">model</span><span class="stat-pill-value"><code>' + escapeHtml(a.last_effective_model) + '</code></span></span>';
        }
        // Live per-kind snapshots when /grok/quota.json is available.
        if (quota && quota.kinds) {
          function safeTone(pct) { return pct > 80 ? 'high' : pct > 50 ? 'mid' : 'low'; }
          function livePill(kind, key, label) {
            if (!kind || !kind.rate_limits || !kind.rate_limits[key]) return '';
            var r = kind.rate_limits[key];
            if (r.limit == null) return '';
            var rem = r.remaining != null ? r.remaining : r.limit;
            var pct = r.limit > 0 ? Math.max(0, Math.min(100, 100 * (r.limit - rem) / r.limit)) : 0;
            var cls = 'quota-mini-pill ' + safeTone(pct);
            return '<span class="' + cls + '" title="' + label + ' ' + rem + '/' + r.limit + '">'
              + '<span class="quota-mini-pill-label">' + label + '</span>'
              + '<span class="quota-mini-pill-value">' + rem + '/' + r.limit + '</span>'
              + '</span>';
          }
          var livePills = '';
          livePills += livePill(quota.kinds.DEFAULT_TEXT, 'requests', 'text req');
          livePills += livePill(quota.kinds.DEFAULT_TEXT, 'tokens',   'text tok');
          livePills += livePill(quota.kinds.DEFAULT_IMAGE, 'requests', 'img req');
          livePills += livePill(quota.kinds.DEFAULT_VIDEO, 'requests', 'vid req');
          if (livePills) {
            usage += '<span class="stat-pill-divider"></span>' + livePills;
          }
        }
        // Pass both the static rate_limits (from /grok/accounts.json, captured
        // at last token refresh) and the live kinds snapshot (from
        // /grok/quota.json, refreshed on every poll).
        var quotaPayload = { limits: a.rate_limits || [] };
        if (quota && quota.kinds) {
          quotaPayload.kinds = quota.kinds;
          if (quota.note) quotaPayload.status_msg = quota.note;
        }
        return '<div class="card">'
          + renderAccountHeader(a.name || a.label || a.email || a.account_id || 'N/A', 'grok', a)
          + '<div class="stat-pills">' + usage + '</div>'
          + renderQuotaBars(quotaPayload, { provider: 'grok', key: a.file_name || a.label || a.email || a.account_id || '' })
          + renderMetaDetails(connectionRows('grok', a, quota), 'Connection details', 'grok', a, a.file_name || a.label || a.email || a.account_id || '')
          + renderAttentionDetails('grok', a, a.file_name || a.label || a.email || a.account_id || '')
          + renderAccountModels(a, null, 'grok', a.file_name || a.label || a.email || a.account_id || '')
          + '</div>';
      }
      async function refreshAgwAccounts() {
        var res = await adminFetch('/agw/accounts.json');
        if (!res) return;
        var data = await res.json();
        var accounts = data.accounts || [];
        dashboardState.providers.agw = accounts;
        var cards = accounts.map(function(a) { return buildProviderCard(a, lastAgwQuota.get(a.file_name || a.label), 'agw'); }).join('');
        renderProviderCards('agw', 'agwCards', 'agwBadgeCount', accounts, cards, 'No Antigravity accounts');
        updateOverview();
      }
      async function refreshAgwQuota() {
        const res = await adminFetch('/agw/quota.json');
        if (!res) return;
        const quota = await res.json();
        const quotaMap = new Map();
        (quota.accounts || []).forEach(q => { quotaMap.set(q.file_name || q.label, q); });
        lastAgwQuota = quotaMap;
        dashboardState.quotas.agw = quotaMap;
        updateOverview();
        refreshAgwAccounts();
      }
      async function refreshGeminiAccounts() {
        var res = await adminFetch('/gemini/accounts.json');
        if (!res) return;
        var data = await res.json();
        var accounts = data.accounts || [];
        dashboardState.providers.gemini = accounts;
        var cards = accounts.map(function(a) { return buildProviderCard(a, lastGeminiQuota.get(a.file_name || a.label), 'gemini'); }).join('');
        renderProviderCards('gemini', 'geminiCards', 'geminiBadgeCount', accounts, cards, 'No Gemini accounts');
        updateOverview();
      }
      async function refreshGeminiQuota() {
        const res = await adminFetch('/gemini/quota.json');
        if (!res) return;
        const quota = await res.json();
        const quotaMap = new Map();
        (quota.accounts || []).forEach(q => { quotaMap.set(q.file_name || q.label, q); });
        lastGeminiQuota = quotaMap;
        dashboardState.quotas.gemini = quotaMap;
        updateOverview();
        refreshGeminiAccounts();
      }
      async function refreshQwenAccounts() {
        var res = await adminFetch('/qwen/accounts.json');
        if (!res) return;
        var data = await res.json();
        var accounts = data.accounts || [];
        dashboardState.providers.qwen = accounts;
        var cards = accounts.map(function(a) { return buildQwenCard(a, lastQwenQuota.get(a.file_name || a.label)); }).join('');
        renderProviderCards('qwen', 'qwenCards', 'qwenBadgeCount', accounts, cards, 'No Qwen accounts');
        updateOverview();
      }
      async function refreshQwenQuota() {
        const res = await adminFetch('/qwen/quota.json');
        if (!res) return;
        const quota = await res.json();
        const quotaMap = new Map();
        (quota.accounts || []).forEach(q => { quotaMap.set(q.file_name || q.label, q); });
        lastQwenQuota = quotaMap;
        dashboardState.quotas.qwen = quotaMap;
        updateOverview();
        refreshQwenAccounts();
      }
      async function refreshDeepSeekAccounts() {
        var res = await adminFetch('/deepseek/accounts.json');
        if (!res) return;
        var data = await res.json();
        var accounts = data.accounts || [];
        dashboardState.providers.deepseek = accounts;
        var cards = accounts.map(function(a) { return buildProviderCard(a, lastDeepSeekQuota.get(a.file_name || a.label), 'deepseek'); }).join('');
        renderProviderCards('deepseek', 'deepseekCards', 'deepseekBadgeCount', accounts, cards, 'No DeepSeek accounts');
        updateOverview();
      }
      async function refreshDeepSeekQuota() {
        const res = await adminFetch('/deepseek/quota.json');
        if (!res) return;
        const quota = await res.json();
        const quotaMap = new Map();
        (quota.accounts || []).forEach(q => { quotaMap.set(q.file_name || q.label, q); });
        lastDeepSeekQuota = quotaMap;
        dashboardState.quotas.deepseek = quotaMap;
        updateOverview();
        refreshDeepSeekAccounts();
      }
      function buildMiniMaxCard(a, quota) {
        var usage = '';
        usage += '<span class="stat-pill"><span class="stat-pill-value">' + (a.requests || 0) + '</span><span class="stat-pill-label">req</span></span>';
        usage += '<span class="stat-pill"><span class="stat-pill-value">' + (a.errors || 0) + '</span><span class="stat-pill-label">err</span></span>';
        usage += '<span class="stat-pill"><span class="stat-pill-value">' + (a.prompt_total || 0) + '</span><span class="stat-pill-label">prompt</span></span>';
        usage += '<span class="stat-pill"><span class="stat-pill-value">' + (a.input_tokens || 0) + '</span><span class="stat-pill-label">in tok</span></span>';
        usage += '<span class="stat-pill"><span class="stat-pill-value">' + (a.output_tokens || 0) + '</span><span class="stat-pill-label">out tok</span></span>';
        usage += '<span class="stat-pill"><span class="stat-pill-value">' + (a.total_tokens || 0) + '</span><span class="stat-pill-label">total tok</span></span>';
        return '<div class="card">'
          + renderAccountHeader(a.label || a.account_id || 'MiniMax', 'minimax', a)
          + '<div class="stat-pills">' + usage + '</div>'
          + renderQuotaBars(quota, { provider: 'minimax', key: a.file_name || a.label || a.account_id || '' })
          + renderAccountModels(a, quota, 'minimax', a.file_name || a.label || a.account_id || '')
          + renderMetaDetails(connectionRows('minimax', a, quota), 'Connection details', 'minimax', a, a.file_name || a.label || a.account_id || '')
          + renderAttentionDetails('minimax', a, a.file_name || a.label || a.account_id || '')
          + '</div>';
      }
      let lastMiniMaxQuota = new Map();
      let lastDeepSeekQuota = new Map();
      async function refreshMiniMaxAccounts() {
        var res = await adminFetch('/minimax/accounts.json');
        if (!res) return;
        var data = await res.json();
        var accounts = data.accounts || [];
        dashboardState.providers.minimax = accounts;
        var cards = accounts.map(function(a) { return buildMiniMaxCard(a, lastMiniMaxQuota.get(a.file_name || a.label)); }).join('');
        renderProviderCards('minimax', 'minimaxCards', 'minimaxBadgeCount', accounts, cards, 'No MiniMax accounts');
        updateOverview();
      }
      async function refreshMiniMaxQuota() {
        const res = await adminFetch('/minimax/quota.json');
        if (!res) return;
        const quota = await res.json();
        const quotaMap = new Map();
        (quota.accounts || []).forEach(q => { quotaMap.set(q.file_name || q.label, q); });
        lastMiniMaxQuota = quotaMap;
        dashboardState.quotas.minimax = quotaMap;
        updateOverview();
        refreshMiniMaxAccounts();
      }
      function buildCopilotCard(a, quota) {
        var usage = '';
        usage += '<span class="stat-pill"><span class="stat-pill-value">' + (a.requests || 0) + '</span><span class="stat-pill-label">req</span></span>';
        usage += '<span class="stat-pill"><span class="stat-pill-value">' + (a.errors || 0) + '</span><span class="stat-pill-label">err</span></span>';
        usage += '<span class="stat-pill"><span class="stat-pill-value">' + (a.prompt_total || 0) + '</span><span class="stat-pill-label">prompt</span></span>';
        usage += '<span class="stat-pill"><span class="stat-pill-value">' + (a.input_tokens || 0) + '</span><span class="stat-pill-label">in tok</span></span>';
        usage += '<span class="stat-pill"><span class="stat-pill-value">' + (a.output_tokens || 0) + '</span><span class="stat-pill-label">out tok</span></span>';
        usage += '<span class="stat-pill"><span class="stat-pill-value">' + (a.total_tokens || 0) + '</span><span class="stat-pill-label">total tok</span></span>';
        if (a.login) {
          usage += '<span class="stat-pill"><span class="stat-pill-label">login</span><span class="stat-pill-value">' + escapeHtml(a.login) + '</span></span>';
        }
        if (a.account_type) {
          usage += '<span class="stat-pill"><span class="stat-pill-label">type</span><span class="stat-pill-value">' + escapeHtml(a.account_type) + '</span></span>';
        }
        return '<div class="card">'
          + renderAccountHeader(a.label || a.login || a.account_id || 'GitHub Copilot', 'copilot', a)
          + '<div class="stat-pills">' + usage + '</div>'
          + renderQuotaBars(quota, { provider: 'copilot', key: a.file_name || a.label || a.login || a.account_id || '' })
          + renderAccountModels(a, quota, 'copilot', a.file_name || a.label || a.login || a.account_id || '')
          + renderMetaDetails(connectionRows('copilot', a, quota), 'Connection details', 'copilot', a, a.file_name || a.label || a.login || a.account_id || '')
          + renderAttentionDetails('copilot', a, a.file_name || a.label || a.login || a.account_id || '')
          + '</div>';
      }
      async function refreshCopilotAccounts() {
        var res = await adminFetch('/copilot/accounts.json');
        if (!res) return;
        var data = await res.json();
        var accounts = data.accounts || [];
        dashboardState.providers.copilot = accounts;
        var cards = accounts.map(function(a) {
          return buildCopilotCard(a, lastCopilotQuota.get(a.file_name || a.label) || lastCopilotQuota.get(a.login) || null);
        }).join('');
        renderProviderCards('copilot', 'copilotCards', 'copilotBadgeCount', accounts, cards, 'No GitHub Copilot accounts');
        updateOverview();
      }
      async function refreshCopilotQuota() {
        const res = await adminFetch('/copilot/quota.json');
        if (!res) return;
        const quota = await res.json();
        const quotaMap = new Map();
        (quota.accounts || []).forEach(q => {
          if (q.file_name || q.label) quotaMap.set(q.file_name || q.label, q);
          if (q.label) quotaMap.set(q.label, q);
          if (q.login) quotaMap.set(q.login, q);
        });
        lastCopilotQuota = quotaMap;
        dashboardState.quotas.copilot = quotaMap;
        updateOverview();
        refreshCopilotAccounts();
      }
      function buildClaudeCard(a, quota) {
        var usage = '';
        usage += '<span class="stat-pill"><span class="stat-pill-value">' + (a.requests || 0) + '</span><span class="stat-pill-label">req</span></span>';
        usage += '<span class="stat-pill"><span class="stat-pill-value">' + (a.errors || 0) + '</span><span class="stat-pill-label">err</span></span>';
        usage += '<span class="stat-pill"><span class="stat-pill-value">' + (a.prompt_total || 0) + '</span><span class="stat-pill-label">prompt</span></span>';
        usage += '<span class="stat-pill"><span class="stat-pill-value">' + (a.input_tokens || 0) + '</span><span class="stat-pill-label">in tok</span></span>';
        usage += '<span class="stat-pill"><span class="stat-pill-value">' + (a.output_tokens || 0) + '</span><span class="stat-pill-label">out tok</span></span>';
        usage += '<span class="stat-pill"><span class="stat-pill-value">' + (a.total_tokens || 0) + '</span><span class="stat-pill-label">total tok</span></span>';
        if (a.cache_tokens) {
          usage += '<span class="stat-pill"><span class="stat-pill-value">' + a.cache_tokens + '</span><span class="stat-pill-label">cache tok</span></span>';
        }
        if (a.reasoning_tokens) {
          usage += '<span class="stat-pill"><span class="stat-pill-value">' + a.reasoning_tokens + '</span><span class="stat-pill-label">reason tok</span></span>';
        }
        if (a.email) {
          usage += '<span class="stat-pill"><span class="stat-pill-label">email</span><span class="stat-pill-value">' + escapeHtml(a.email) + '</span></span>';
        }
        return '<div class="card">'
          + renderAccountHeader(a.label || a.email || a.organization_uuid || a.account_id || 'Claude', 'claude', a)
          + '<div class="stat-pills">' + usage + '</div>'
          + renderQuotaBars(quota, { provider: 'claude', key: a.file_name || a.label || a.organization_uuid || a.account_id || '' })
          + renderAccountModels(a, quota, 'claude', a.file_name || a.label || a.organization_uuid || a.account_id || '')
          + renderMetaDetails(connectionRows('claude', a, quota), 'Connection details', 'claude', a, a.file_name || a.label || a.organization_uuid || a.account_id || '')
          + renderAttentionDetails('claude', a, a.file_name || a.label || a.organization_uuid || a.account_id || '')
          + '</div>';
      }
      async function refreshClaudeAccounts() {
        var res = await adminFetch('/claude/accounts.json');
        if (!res) return;
        var data = await res.json();
        var accounts = data.accounts || [];
        dashboardState.providers.claude = accounts;
        var cards = accounts.map(function(a) {
          return buildClaudeCard(a, lastClaudeQuota.get(a.file_name || a.label) || lastClaudeQuota.get(a.organization_uuid) || lastClaudeQuota.get(a.account_id) || lastClaudeQuota.get(a.email) || null);
        }).join('');
        renderProviderCards('claude', 'claudeCards', 'claudeBadgeCount', accounts, cards, 'No Claude accounts');
        updateOverview();
      }
      async function refreshClaudeQuota() {
        const res = await adminFetch('/claude/quota.json');
        if (!res) return;
        const quota = await res.json();
        const quotaMap = new Map();
        (quota.accounts || []).forEach(q => {
          if (q.file_name || q.label) quotaMap.set(q.file_name || q.label, q);
          if (q.label) quotaMap.set(q.label, q);
          if (q.organization_uuid) quotaMap.set(q.organization_uuid, q);
          if (q.account_id) quotaMap.set(q.account_id, q);
          if (q.email) quotaMap.set(q.email, q);
        });
        lastClaudeQuota = quotaMap;
        dashboardState.quotas.claude = quotaMap;
        updateOverview();
        refreshClaudeAccounts();
      }
      function buildGlmCard(a, quota) {
        var usage = '';
        if (a.account_type) {
          usage += '<span class="stat-pill"><span class="stat-pill-label">type</span><span class="stat-pill-value">' + escapeHtml(a.account_type) + '</span></span>';
        }
        usage += '<span class="stat-pill"><span class="stat-pill-value">' + (a.requests || 0) + '</span><span class="stat-pill-label">req</span></span>';
        usage += '<span class="stat-pill"><span class="stat-pill-value">' + (a.errors || 0) + '</span><span class="stat-pill-label">err</span></span>';
        usage += '<span class="stat-pill"><span class="stat-pill-value">' + (a.prompt_total || 0) + '</span><span class="stat-pill-label">prompt</span></span>';
        usage += '<span class="stat-pill"><span class="stat-pill-value">' + (a.input_tokens || 0) + '</span><span class="stat-pill-label">in tok</span></span>';
        usage += '<span class="stat-pill"><span class="stat-pill-value">' + (a.output_tokens || 0) + '</span><span class="stat-pill-label">out tok</span></span>';
        usage += '<span class="stat-pill"><span class="stat-pill-value">' + (a.total_tokens || 0) + '</span><span class="stat-pill-label">total tok</span></span>';
        return '<div class="card">'
          + renderAccountHeader(a.label || a.account_id || 'GLM', 'glm', a)
          + '<div class="stat-pills">' + usage + '</div>'
          + renderQuotaBars(quota, { provider: 'glm', key: a.file_name || a.label || a.account_id || '' })
          + renderAccountModels(a, quota, 'glm', a.file_name || a.label || a.account_id || '')
          + renderMetaDetails(connectionRows('glm', a, quota), 'Connection details', 'glm', a, a.file_name || a.label || a.account_id || '')
          + renderAttentionDetails('glm', a, a.file_name || a.label || a.account_id || '')
          + '</div>';
      }
      async function refreshGlmAccounts() {
        var res = await adminFetch('/glm/accounts.json');
        if (!res) return;
        var data = await res.json();
        var accounts = data.accounts || [];
        dashboardState.providers.glm = accounts;
        var cards = accounts.map(function(a) {
          return buildGlmCard(a, lastGlmQuota.get(a.file_name || a.label) || lastGlmQuota.get(a.account_id) || null);
        }).join('');
        renderProviderCards('glm', 'glmCards', 'glmBadgeCount', accounts, cards, 'No GLM accounts');
        updateOverview();
      }
      async function refreshGlmQuota() {
        const res = await adminFetch('/glm/quota.json');
        if (!res) return;
        const quota = await res.json();
        const quotaMap = new Map();
        (quota.accounts || []).forEach(q => {
          if (q.file_name || q.label) quotaMap.set(q.file_name || q.label, q);
          if (q.label) quotaMap.set(q.label, q);
          if (q.account_id) quotaMap.set(q.account_id, q);
        });
        lastGlmQuota = quotaMap;
        dashboardState.quotas.glm = quotaMap;
        updateOverview();
        refreshGlmAccounts();
      }
      function normalizeCustomAlias(alias) {
        return String(alias || '').trim().replace(/^ctm:/i, '');
      }
      function findCustomModel(alias) {
        var normalized = normalizeCustomAlias(alias).toLowerCase();
        return (dashboardState.customModels || []).find(function(model) {
          return normalizeCustomAlias(model.alias).toLowerCase() === normalized;
        }) || null;
      }
      function customModelPublicId(model) {
        return model && model.id ? model.id : 'ctm:' + normalizeCustomAlias(model && model.alias);
      }
      function enabledCustomTargets(targets) {
        return (targets || []).filter(function(target) {
          return target && target.enabled !== false && String(target.model || '').trim();
        });
      }
      function customTargetLabel(target) {
        var model = String(target && target.model || '').trim();
        var account = String(target && target.account || '').trim();
        var condition = String(target && target.account_condition || target && target.accountCondition || '').toLowerCase();
        return account ? model + (condition === 'except' ? '@!' : '@') + account : model;
      }
      function customRoutes(model) {
        if (model && Array.isArray(model.routes)) return model.routes;
        var routes = [];
        var primary = enabledCustomTargets(model && model.primary_models);
        if (primary.length) routes.push({ targets: primary });
        enabledCustomTargets(model && model.fallback_models).forEach(function(target) {
          routes.push({ targets: [target] });
        });
        return routes;
      }
      function customRoutesTextarea(routes) {
        return (routes || []).map(function(group) {
          return enabledCustomTargets(group && group.targets)
            .map(customTargetLabel)
            .join(', ');
        }).filter(Boolean).join('\n');
      }
      function renderCustomTargetRow(label, targets, emptyText) {
        var enabledTargets = enabledCustomTargets(targets);
        var body = enabledTargets.length
          ? '<span class="custom-model-targets">' + enabledTargets.map(function(target) {
              var weight = Number(target.weight || 1);
              var suffix = enabledTargets.length > 1 && weight > 1 ? ' x' + weight : '';
              return '<span class="custom-model-chip"><code>' + escapeHtml(customTargetLabel(target) + suffix) + '</code></span>';
            }).join('') + '</span>'
          : '<span class="muted">' + escapeHtml(emptyText || 'None') + '</span>';
        return '<div class="custom-model-route-row">'
          + '<span class="custom-model-route-label">' + escapeHtml(label) + '</span>'
          + body
          + '</div>';
      }
      function renderCustomModelCard(model) {
        var alias = normalizeCustomAlias(model.alias);
        var publicId = customModelPublicId(model);
        var title = model.display_name || alias || publicId;
        var routes = customRoutes(model);
        var enabledGroups = routes.filter(function(group) {
          return enabledCustomTargets(group && group.targets).length > 0;
        });
        var targetCount = model.target_count != null
          ? model.target_count
          : enabledGroups.reduce(function(total, group) {
              return total + enabledCustomTargets(group.targets).length;
            }, 0);
        var routeGroupCount = model.route_group_count != null ? model.route_group_count : enabledGroups.length;
        var routeRows = enabledGroups.length
          ? enabledGroups.map(function(group, index) {
              return renderCustomTargetRow('Step ' + (index + 1), group.targets, 'No targets');
            }).join('')
          : '<div class="muted">No enabled route targets</div>';
        return '<div class="card custom-model-card">'
          + renderCustomModelHeader(title, model)
          + '<div class="stat-pills">'
          + '<span class="stat-pill"><span class="stat-pill-label">id</span><span class="stat-pill-value"><code>' + escapeHtml(publicId) + '</code></span></span>'
          + '<span class="stat-pill"><span class="stat-pill-label">steps</span><span class="stat-pill-value">' + routeGroupCount + '</span></span>'
          + '<span class="stat-pill"><span class="stat-pill-label">targets</span><span class="stat-pill-value">' + targetCount + '</span></span>'
          + '</div>'
          + '<div class="custom-model-route-list">'
          + routeRows
          + '</div>'
          + '</div>';
      }
      function renderCustomModels(models) {
        var cards = document.getElementById('customModelCards');
        var note = document.getElementById('customModelsNote');
        if (!cards) return;
        cards.innerHTML = models.length
          ? models.map(renderCustomModelCard).join('')
          : '<div class="empty-state">No custom models</div>';
        if (note) {
          note.textContent = models.length
            ? models.length + ' custom routes available'
            : 'No custom routes configured';
        }
      }
      async function refreshCustomModels() {
        const res = await adminFetch('/custom-models.json');
        if (!res) return;
        const data = await res.json();
        const models = data.models || [];
        dashboardState.customModels = models;
        dashboardState.customModelAccounts = data.accounts || [];
        dashboardState.customModelModelOptions = data.model_options || [];
        dashboardState.testApiModelOptions = data.test_model_options || [];
        renderCustomModels(models);
        renderTestApiModelOptions();
      }
      // ---------- Custom model route editor ----------
      const customModelProviderCatalog = [
        { prefix: 'agw', label: 'Antigravity', placeholder: 'gemini-2.5-pro' },
        { prefix: 'gem', label: 'Gemini', placeholder: 'gemini-2.5-pro' },
        { prefix: 'qwn', label: 'Qwen', placeholder: 'qwen3-coder-plus' },
        { prefix: 'dsk', label: 'DeepSeek', placeholder: 'deepseek-chat' },
        { prefix: 'grk', label: 'Grok', placeholder: 'grok-2' },
        { prefix: 'min', label: 'MiniMax', placeholder: 'MiniMax-M3' },
        { prefix: 'cop', label: 'GitHub Copilot', placeholder: 'gpt-5.1' },
        { prefix: 'cld', label: 'Claude', placeholder: 'claude-sonnet-4-5' },
        { prefix: 'glm', label: 'GLM (Z.AI)', placeholder: 'glm-4.6' },
        { prefix: 'cod', label: 'Codex', placeholder: 'gpt-5' }
      ];
      const customModelProviderPrefixes = customModelProviderCatalog.map(function(p) { return p.prefix; });
      const customModelPrefixAliases = {
        agw: 'agw', antigravity: 'agw', 'anti-gravity': 'agw',
        gem: 'gem', gemini: 'gem',
        qwn: 'qwn', qwen: 'qwn',
        dsk: 'dsk', deepseek: 'dsk',
        grk: 'grk', grok: 'grk', xai: 'grk',
        min: 'min', minimax: 'min',
        cop: 'cop', copilot: 'cop', 'github-copilot': 'cop', github_copilot: 'cop',
        cld: 'cld', claude: 'cld', anthropic: 'cld',
        glm: 'glm', zai: 'glm', 'z-ai': 'glm',
        cod: 'cod', codex: 'cod',
        ctm: 'ctm', custom: 'ctm'
      };
      function customModelCanonicalizePrefix(prefix) {
        if (!prefix) return '';
        return customModelPrefixAliases[String(prefix).toLowerCase()] || '';
      }
      function customModelSplitTargetSpec(spec) {
        var raw = String(spec || '').trim();
        var at = raw.indexOf('@');
        var modelPart = at < 0 ? raw : raw.slice(0, at).trim();
        var account = at < 0 ? '' : raw.slice(at + 1).trim();
        var accountCondition = 'only';
        if (account.charAt(0) === '!') {
          accountCondition = 'except';
          account = account.slice(1).trim();
        }
        var colon = modelPart.indexOf(':');
        var prefix = colon < 0 ? '' : modelPart.slice(0, colon).trim();
        var model = colon < 0 ? modelPart.trim() : modelPart.slice(colon + 1).trim();
        var canonical = customModelCanonicalizePrefix(prefix);
        return { provider: canonical || prefix.toLowerCase(), model: model, account: account, accountCondition: account ? accountCondition : 'all' };
      }
      const customModelEditorState = { steps: [], lastFocusedTargetId: null };
      let customModelEditorCounter = 0;
      function customModelNewId(prefix) {
        customModelEditorCounter += 1;
        return prefix + '-' + Date.now().toString(36) + '-' + customModelEditorCounter.toString(36);
      }
      function customModelEmptyTarget(partial) {
        partial = partial || {};
        var provider = partial.provider || customModelProviderCatalog[0].prefix;
        return {
          id: customModelNewId('t'),
          provider: provider,
          model: partial.model || '',
          account: partial.account || '',
          accountCondition: partial.accountCondition || partial.account_condition || (partial.account ? 'only' : 'all'),
          weight: Number(partial.weight) > 0 ? Number(partial.weight) : 1,
          enabled: partial.enabled !== false
        };
      }
      function customModelEmptyStep() {
        return { id: customModelNewId('s'), targets: [customModelEmptyTarget()] };
      }
      function customModelHydrateEditor(model) {
        var steps = [];
        var routes = customRoutes(model);
        routes.forEach(function (group) {
          var enabledTargets = enabledCustomTargets(group && group.targets);
          if (!enabledTargets.length) return;
          var stepTargets = [];
          enabledTargets.forEach(function (raw) {
            var split = customModelSplitTargetSpec(raw.model || '');
            var weight = Number(raw.weight) > 0 ? Number(raw.weight) : 1;
            var enabled = raw.enabled !== false;
            var account = String(raw.account || split.account || '').trim();
            var rawCondition = String(raw.account_condition || raw.accountCondition || split.accountCondition || '').toLowerCase();
            var accountCondition = account ? (rawCondition === 'except' ? 'except' : 'only') : 'all';
            stepTargets.push(customModelEmptyTarget({
              provider: split.provider || customModelProviderCatalog[0].prefix,
              model: split.model,
              account: account,
              accountCondition: accountCondition,
              weight: weight,
              enabled: enabled
            }));
          });
          steps.push({ id: customModelNewId('s'), targets: stepTargets });
        });
        if (!steps.length) steps.push(customModelEmptyStep());
        return { steps: steps };
      }
      function renderCustomModelEditor() {
        var stepsEl = document.getElementById('customModelSteps');
        if (!stepsEl) return;
        var state = customModelEditorState;
        if (!state.steps.length) {
          stepsEl.innerHTML = '<div class="custom-model-preview-empty">No steps yet. Click "+ Add fallback step" below.</div>';
        } else {
          stepsEl.innerHTML = state.steps.map(function (step, stepIdx) {
            var enabledCount = step.targets.filter(function (t) { return t.enabled !== false && String(t.model || '').trim(); }).length;
            var strategy = step.targets.length > 1 ? 'load-balanced' : 'fallback only';
            var upDisabled = stepIdx === 0 ? ' disabled' : '';
            var downDisabled = stepIdx === state.steps.length - 1 ? ' disabled' : '';
            return ''
              + '<div class="custom-model-step" data-step-id="' + escapeHtml(step.id) + '">'
              +   '<div class="custom-model-step-header">'
              +     '<span class="custom-model-step-index">Step ' + (stepIdx + 1) + '</span>'
              +     '<span class="custom-model-step-summary">' + enabledCount + ' of ' + step.targets.length + ' enabled · ' + strategy + '</span>'
              +     '<span class="custom-model-step-toolbar">'
              +       '<button type="button" class="mini-btn" data-step-action="up" data-step-id="' + escapeHtml(step.id) + '"' + upDisabled + ' aria-label="Move step up">↑</button>'
              +       '<button type="button" class="mini-btn" data-step-action="down" data-step-id="' + escapeHtml(step.id) + '"' + downDisabled + ' aria-label="Move step down">↓</button>'
              +       '<button type="button" class="mini-btn danger" data-step-action="remove" data-step-id="' + escapeHtml(step.id) + '">Remove step</button>'
              +     '</span>'
              +   '</div>'
              +   '<div class="custom-model-step-targets">'
              +     step.targets.map(function (t) { return renderCustomModelTargetRow(step.id, t, step.targets.length > 1); }).join('')
              +   '</div>'
              +   '<div class="custom-model-step-footer">'
              +     '<button type="button" class="mini-btn" data-step-action="add-target" data-step-id="' + escapeHtml(step.id) + '">+ Add target to step</button>'
              +   '</div>'
              +   '<div class="custom-model-field-error" data-step-error="' + escapeHtml(step.id) + '"></div>'
              + '</div>';
          }).join('');
        }
        renderCustomModelPreview();
      }
      function renderCustomModelTargetRow(stepId, target, showTrafficShare) {
        var providerOptions = customModelProviderCatalog.map(function (p) {
          return '<option value="' + escapeHtml(p.prefix) + '"' + (target.provider === p.prefix ? ' selected' : '') + '>' + escapeHtml(p.label) + '</option>';
        }).join('');
        var modelOptions = customModelModelOptions(target.provider, target.model);
        var accountCondition = target.accountCondition === 'except' ? 'except' : target.accountCondition === 'only' ? 'only' : 'all';
        var accountConditionOptions = [
          { value: 'all', label: 'All accounts' },
          { value: 'only', label: 'Only account' },
          { value: 'except', label: 'Except account' }
        ].map(function(option) {
          return '<option value="' + option.value + '"' + (accountCondition === option.value ? ' selected' : '') + '>' + option.label + '</option>';
        }).join('');
        var accountOptions = customModelAccountOptions(target.provider, target.account);
        var accountSelectAttrs = accountCondition === 'all' ? ' hidden disabled' : '';
        var weight = Math.max(1, Math.floor(Number(target.weight) || 1));
        var shareOptions = customModelTrafficShareOptions(weight);
        var shareAttrs = showTrafficShare ? '' : ' hidden';
        var shareSelectAttrs = showTrafficShare ? '' : ' disabled';
        var enabled = target.enabled !== false;
        return ''
          + '<div class="custom-model-target' + (enabled ? '' : ' disabled') + '" data-step-id="' + escapeHtml(stepId) + '" data-target-id="' + escapeHtml(target.id) + '">'
          +   '<select class="custom-model-target-provider" data-target-field="provider" data-target-id="' + escapeHtml(target.id) + '" aria-label="Provider">' + providerOptions + '</select>'
          +   '<select class="custom-model-target-model" data-target-field="model" data-target-id="' + escapeHtml(target.id) + '" aria-label="Model">' + modelOptions + '</select>'
          +   '<select class="custom-model-target-account-condition" data-target-field="account_condition" data-target-id="' + escapeHtml(target.id) + '" aria-label="Account condition">' + accountConditionOptions + '</select>'
          +   '<select class="custom-model-target-account" data-target-field="account" data-target-id="' + escapeHtml(target.id) + '" aria-label="Account"' + accountSelectAttrs + '>' + accountOptions + '</select>'
          +   '<label class="custom-model-target-share" title="Relative traffic share inside this fallback step"' + shareAttrs + '><span>Share</span><select data-target-field="weight" data-target-id="' + escapeHtml(target.id) + '" aria-label="Traffic share"' + shareSelectAttrs + '>' + shareOptions + '</select></label>'
          +   '<label class="check-row" title="Enabled"><input type="checkbox" data-target-field="enabled" data-target-id="' + escapeHtml(target.id) + '"' + (enabled ? ' checked' : '') + '> on</label>'
          +   '<span class="custom-model-target-toolbar">'
          +     '<button type="button" class="mini-btn danger" data-target-action="remove" data-target-id="' + escapeHtml(target.id) + '">Remove</button>'
          +   '</span>'
          + '</div>';
      }
      function customModelTrafficShareOptions(currentWeight) {
        var current = Math.max(1, Math.floor(Number(currentWeight) || 1));
        var values = [1, 2, 3, 5, 10];
        if (values.indexOf(current) < 0) {
          values.push(current);
          values.sort(function (a, b) { return a - b; });
        }
        return values.map(function (value) {
          return '<option value="' + value + '"' + (value === current ? ' selected' : '') + '>' + value + 'x</option>';
        }).join('');
      }
      function customModelModelOptions(provider, currentModel) {
        var canonical = customModelCanonicalizePrefix(provider) || String(provider || '').toLowerCase();
        var options = (dashboardState.customModelModelOptions || []).filter(function(option) {
          return String(option.provider || '').toLowerCase() === canonical;
        });
        var current = String(currentModel || '').trim();
        var seen = new Set();
        var html = current ? '' : '<option value="" selected>Choose model</option>';
        options.forEach(function(option) {
          var value = String(option.model || '').trim();
          if (!value || seen.has(value)) return;
          seen.add(value);
          var label = option.display_name && option.display_name !== value
            ? option.display_name + ' (' + value + ')'
            : value;
          html += '<option value="' + escapeHtml(value) + '"' + (value === current ? ' selected' : '') + '>' + escapeHtml(label) + '</option>';
        });
        if (current && !seen.has(current)) {
          html = '<option value="' + escapeHtml(current) + '" selected>' + escapeHtml(current) + '</option>' + html;
        }
        if (!current && !seen.size) {
          var fallbackModel = (customModelProviderCatalog.find(function (p) { return p.prefix === canonical; }) || {}).placeholder || 'model-id';
          html += '<option value="' + escapeHtml(fallbackModel) + '">' + escapeHtml(fallbackModel) + '</option>';
        }
        if (!html) {
          var fallback = (customModelProviderCatalog.find(function (p) { return p.prefix === canonical; }) || {}).placeholder || 'model-id';
          html = current
            ? '<option value="' + escapeHtml(current) + '" selected>' + escapeHtml(current) + '</option>'
            : '<option value="" selected>Choose model</option><option value="' + escapeHtml(fallback) + '">' + escapeHtml(fallback) + '</option>';
        }
        return html;
      }
      function customModelAccountOptions(provider, currentAccount) {
        var canonical = customModelCanonicalizePrefix(provider) || String(provider || '').toLowerCase();
        var accounts = (dashboardState.customModelAccounts || []).filter(function (a) {
          var accountProvider = customModelCanonicalizePrefix(a.provider) || String(a.provider || '').toLowerCase();
          return !canonical || accountProvider === canonical;
        });
        var current = String(currentAccount || '').trim();
        var seen = new Set();
        var html = '<option value="">Any account</option>';
        accounts.forEach(function (a) {
          var key = String(a.key || '').trim();
          if (!key || seen.has(key)) return;
          seen.add(key);
          var title = a.label || a.account_id || key;
          html += '<option value="' + escapeHtml(key) + '"' + (key === current ? ' selected' : '') + '>' + escapeHtml(title) + '</option>';
        });
        if (current && !seen.has(current)) {
          html += '<option value="' + escapeHtml(current) + '" selected>' + escapeHtml(current) + '</option>';
        }
        return html;
      }
      function renderCustomModelPreview() {
        var previewEl = document.getElementById('customModelPreview');
        if (!previewEl) return;
        var model = customModelBuildPreviewModel();
        if (!model) {
          previewEl.innerHTML = '<div class="custom-model-preview-empty">Fill in alias and at least one route target to see a preview.</div>';
          return;
        }
        previewEl.innerHTML = renderCustomModelCard(model);
      }
      function customModelBuildPreviewModel() {
        var form = document.getElementById('customModelForm');
        if (!form) return null;
        var alias = form.querySelector('input[name="alias"]').value.trim();
        if (!alias) return null;
        var display = form.querySelector('input[name="display_name"]').value.trim();
        var enabled = form.querySelector('input[name="enabled"]').checked;
        var serialized = serializeCustomModelEditor(customModelEditorState);
        if (!serialized.routes.length) return null;
        return {
          alias: alias,
          display_name: display || null,
          enabled: enabled,
          routes: serialized.routes,
          target_count: serialized.routes.reduce(function (s, g) { return s + g.targets.length; }, 0),
          route_group_count: serialized.routes.length,
          id: 'ctm:' + alias
        };
      }
      // ---------- State mutators ----------
      function addCustomModelStep() {
        customModelEditorState.steps.push(customModelEmptyStep());
        renderCustomModelEditor();
        renderCustomModelFieldErrors();
      }
      function removeCustomModelStep(stepId) {
        customModelEditorState.steps = customModelEditorState.steps.filter(function (s) { return s.id !== stepId; });
        if (!customModelEditorState.steps.length) customModelEditorState.steps.push(customModelEmptyStep());
        renderCustomModelEditor();
        renderCustomModelFieldErrors();
      }
      function moveCustomModelStep(stepId, dir) {
        var arr = customModelEditorState.steps;
        var idx = arr.findIndex(function (s) { return s.id === stepId; });
        if (idx < 0) return;
        var j = idx + dir;
        if (j < 0 || j >= arr.length) return;
        var tmp = arr[idx]; arr[idx] = arr[j]; arr[j] = tmp;
        renderCustomModelEditor();
        renderCustomModelFieldErrors();
      }
      function addCustomModelTarget(stepId, partial) {
        var step = customModelEditorState.steps.find(function (s) { return s.id === stepId; });
        if (!step) return;
        var fresh = customModelEmptyTarget(partial || {});
        if (partial && partial.provider) fresh.provider = partial.provider;
        step.targets.push(fresh);
        renderCustomModelEditor();
        renderCustomModelFieldErrors();
      }
      function removeCustomModelTarget(stepId, targetId) {
        var step = customModelEditorState.steps.find(function (s) { return s.id === stepId; });
        if (!step) return;
        step.targets = step.targets.filter(function (t) { return t.id !== targetId; });
        if (!step.targets.length) step.targets.push(customModelEmptyTarget());
        renderCustomModelEditor();
        renderCustomModelFieldErrors();
      }
      function patchCustomModelTarget(targetId, patch) {
        for (var i = 0; i < customModelEditorState.steps.length; i++) {
          var step = customModelEditorState.steps[i];
          var t = step.targets.find(function (x) { return x.id === targetId; });
          if (t) {
            if (patch.provider != null) t.provider = patch.provider;
            if (patch.model != null) t.model = patch.model;
            if (patch.account != null) t.account = patch.account;
            if (patch.accountCondition != null) t.accountCondition = patch.accountCondition;
            if (patch.weight != null) t.weight = Math.max(1, Math.floor(Number(patch.weight) || 1));
            if (patch.enabled != null) t.enabled = !!patch.enabled;
            return;
          }
        }
      }
      // ---------- Account picker ----------
      let customModelAccountPickerOpenId = null;
      function openCustomModelAccountPicker(targetId) {
        var popover = document.querySelector('[data-target-picker="' + targetId + '"]');
        if (!popover) return;
        var wasOpen = !popover.hidden;
        closeCustomModelAccountPickers();
        if (wasOpen) return;
        renderCustomModelAccountPicker(targetId, popover);
        popover.hidden = false;
        customModelAccountPickerOpenId = targetId;
      }
      function closeCustomModelAccountPickers() {
        document.querySelectorAll('.custom-model-account-picker-popover').forEach(function (el) { el.hidden = true; });
        customModelAccountPickerOpenId = null;
      }
      function renderCustomModelAccountPicker(targetId, popover) {
        var accounts = dashboardState.customModelAccounts || [];
        var target = null;
        for (var i = 0; i < customModelEditorState.steps.length && !target; i++) {
          target = customModelEditorState.steps[i].targets.find(function (t) { return t.id === targetId; });
        }
        if (!target) return;
        var canonical = customModelCanonicalizePrefix(target.provider) || (target.provider || '').toLowerCase();
        var filtered = accounts.filter(function (a) {
          var accountProvider = customModelCanonicalizePrefix(a.provider) || String(a.provider || '').toLowerCase();
          return !canonical || accountProvider === canonical;
        });
        var html = ''
          + '<div class="custom-model-account-picker-head">'
          +   '<button type="button" class="mini-btn" data-picker-action="select-all" data-target-id="' + escapeHtml(targetId) + '">All accounts (*)</button>'
          +   '<button type="button" class="mini-btn secondary-button" data-picker-action="clear" data-target-id="' + escapeHtml(targetId) + '">Clear</button>'
          + '</div>';
        if (!filtered.length) {
          html += '<div class="muted">No account keys match this provider.</div>';
        } else {
          var selected = new Set(target.accounts || []);
          html += '<div class="custom-model-account-provider">';
          filtered.forEach(function (a) {
            var key = a.key || '';
            var title = a.label || a.account_id || key;
            var checked = selected.has(key) ? ' checked' : '';
            html += '<label class="custom-model-account-row" style="cursor:pointer;">'
              + '<div><strong>' + escapeHtml(title) + '</strong><br><code>' + escapeHtml(key) + '</code></div>'
              + '<input type="checkbox" data-picker-checkbox="' + escapeHtml(key) + '" data-target-id="' + escapeHtml(targetId) + '"' + checked + '>'
              + '</label>';
          });
          html += '</div>';
        }
        popover.innerHTML = html;
      }
      // ---------- Serialize / validate ----------
      function serializeCustomModelEditor(state) {
        var routes = [];
        state.steps.forEach(function (step) {
          var targets = [];
          step.targets.forEach(function (t) {
            var provider = (t.provider || '').toLowerCase();
            var model = String(t.model || '').trim();
            if (!provider || !model) return;
            var enabled = t.enabled !== false;
            if (!enabled) return;
            var weight = Math.max(1, Math.floor(Number(t.weight) || 1));
            var account = String(t.account || '').trim();
            var condition = account ? (t.accountCondition === 'except' ? 'except' : 'only') : 'all';
            var target = {
              model: provider + ':' + model,
              weight: weight,
              enabled: enabled
            };
            if (account && condition !== 'all') {
              target.account = account;
              if (condition === 'except') target.account_condition = 'except';
            }
            targets.push(target);
          });
          if (targets.length) {
            routes.push({ targets: targets });
          }
        });
        var text = routes.map(function (g) {
          var showWeights = g.targets.length > 1;
          return g.targets.map(function (t) {
            return customTargetLabel(t) + (showWeights && Number(t.weight || 1) > 1 ? ' x' + t.weight : '');
          }).join(', ');
        }).join('\n');
        return { routes: routes, text: text };
      }
      function validateCustomModelEditor() {
        var errors = { alias: '', steps: {} };
        var aliasInput = document.getElementById('customModelAliasInput');
        var alias = aliasInput ? aliasInput.value.trim() : '';
        if (!alias) errors.alias = 'Alias is required.';
        else if (/[\s:/\\]/.test(alias)) errors.alias = 'Alias must not contain whitespace, colon, slash, or backslash.';
        var totalEnabled = 0;
        customModelEditorState.steps.forEach(function (step) {
          var messages = [];
          var stepEnabled = 0;
          step.targets.forEach(function (t) {
            var provider = (t.provider || '').toLowerCase();
            var model = String(t.model || '').trim();
            if (!model) return;
            if (t.enabled === false) return;
            stepEnabled++; totalEnabled++;
            var canonical = customModelCanonicalizePrefix(provider);
            if (canonical === 'ctm') {
              messages.push('Custom models cannot target another custom model.');
            } else if (!canonical || customModelProviderPrefixes.indexOf(canonical) < 0) {
              messages.push('Unsupported provider prefix "' + provider + '" for "' + model + '".');
            }
            var condition = t.accountCondition === 'except' ? 'except' : t.accountCondition === 'only' ? 'only' : 'all';
            if (condition !== 'all' && !String(t.account || '').trim()) {
              messages.push('Choose an account for "' + model + '" or use All accounts.');
            }
          });
          if (!stepEnabled) messages.push('At least one enabled target is required in this step.');
          if (messages.length) errors.steps[step.id] = messages.join(' ');
        });
        if (!totalEnabled && !Object.keys(errors.steps).length) {
          var first = customModelEditorState.steps[0];
          if (first) errors.steps[first.id] = 'At least one route target is required.';
        }
        return errors;
      }
      function renderCustomModelFieldErrors() {
        var errors = validateCustomModelEditor();
        var aliasErrEl = document.getElementById('customModelAliasError');
        if (aliasErrEl) aliasErrEl.textContent = errors.alias || '';
        document.querySelectorAll('[data-step-error]').forEach(function (el) {
          var sid = el.getAttribute('data-step-error');
          el.textContent = (errors.steps && errors.steps[sid]) || '';
        });
      }
      // ---------- Modal open/close + submit ----------
      function openCustomModelModal(alias) {
        var model = alias ? findCustomModel(alias) : null;
        var title = document.getElementById('customModelTitle');
        var form = document.getElementById('customModelForm');
        if (!form) return;
        if (title) title.textContent = model ? 'Edit Custom Model' : 'Add Custom Model';
        form.querySelector('input[name="original_alias"]').value = model ? normalizeCustomAlias(model.alias) : '';
        form.querySelector('input[name="alias"]').value = model ? normalizeCustomAlias(model.alias) : '';
        form.querySelector('input[name="display_name"]').value = model && model.display_name ? model.display_name : '';
        form.querySelector('input[name="enabled"]').checked = !model || model.enabled !== false;
        customModelEditorState.steps = customModelHydrateEditor(model).steps;
        customModelEditorState.lastFocusedTargetId = null;
        closeCustomModelAccountPickers();
        renderCustomModelEditor();
        renderCustomModelFieldErrors();
        setText('customModelStatus', '');
        openModal('customModelModal');
        var aliasInput = document.getElementById('customModelAliasInput');
        if (aliasInput) setTimeout(function () { aliasInput.focus(); aliasInput.select && aliasInput.select(); }, 30);
      }
      function closeCustomModelModal() {
        closeModal('customModelModal');
        closeCustomModelAccountPickers();
        setText('customModelStatus', '');
      }
      async function submitCustomModelForm(e) {
        e.preventDefault();
        var form = e.target;
        var serialized = serializeCustomModelEditor(customModelEditorState);
        var textarea = form.querySelector('textarea[name="routes"]');
        if (textarea) textarea.value = serialized.text;
        var errors = validateCustomModelEditor();
        renderCustomModelFieldErrors();
        if (errors.alias) {
          setText('customModelStatus', errors.alias);
          var aliasInput = document.getElementById('customModelAliasInput');
          if (aliasInput) aliasInput.focus();
          return;
        }
        var stepErrorKeys = Object.keys(errors.steps || {});
        if (stepErrorKeys.length) {
          setText('customModelStatus', errors.steps[stepErrorKeys[0]]);
          return;
        }
        var totalEnabled = 0;
        customModelEditorState.steps.forEach(function (s) {
          s.targets.forEach(function (t) {
            if (t.enabled !== false && String(t.model || '').trim()) totalEnabled++;
          });
        });
        if (!totalEnabled) {
          setText('customModelStatus', 'At least one route target is required.');
          return;
        }
        var payload = {
          original_alias: form.querySelector('input[name="original_alias"]').value.trim() || undefined,
          alias: form.querySelector('input[name="alias"]').value.trim(),
          display_name: form.querySelector('input[name="display_name"]').value.trim() || undefined,
          enabled: form.querySelector('input[name="enabled"]').checked,
          routes: serialized.routes
        };
        setText('customModelStatus', 'Saving custom model...');
        var res = await adminFetch('/custom-models/save', {
          method: 'POST',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify(payload)
        });
        if (!res) return;
        var data = await res.json();
        setText('customModelStatus', data.message || (data.ok ? 'Saved.' : 'Save failed.'));
        notify(data.message || (data.ok ? 'Custom model saved' : 'Save failed'), data.ok === false ? 'error' : '');
        if (!data.ok) return;
        closeCustomModelModal();
        refreshCustomModels();
      }
      function bindCustomModelEditorEvents() {
        var addBtn = document.getElementById('addCustomStepBtn');
        if (addBtn) addBtn.addEventListener('click', addCustomModelStep);
        var stepsEl = document.getElementById('customModelSteps');
        if (!stepsEl) return;
        stepsEl.addEventListener('click', function (e) {
          var stepBtn = e.target.closest('[data-step-action]');
          if (stepBtn) {
            var sid = stepBtn.getAttribute('data-step-id');
            var action = stepBtn.getAttribute('data-step-action');
            if (action === 'remove') { removeCustomModelStep(sid); return; }
            if (action === 'up') { moveCustomModelStep(sid, -1); return; }
            if (action === 'down') { moveCustomModelStep(sid, 1); return; }
            if (action === 'add-target') { addCustomModelTarget(sid); return; }
          }
          var targetBtn = e.target.closest('[data-target-action]');
          if (targetBtn) {
            var tid = targetBtn.getAttribute('data-target-id');
            var host = targetBtn.closest('.custom-model-target');
            var stepId = host ? host.getAttribute('data-step-id') : null;
            var taction = targetBtn.getAttribute('data-target-action');
            if (taction === 'remove') {
              if (stepId) removeCustomModelTarget(stepId, tid);
              return;
            }
            if (taction === 'open-picker') {
              if (host) customModelEditorState.lastFocusedTargetId = tid;
              openCustomModelAccountPicker(tid);
              return;
            }
          }
          var pickerBtn = e.target.closest('[data-picker-action]');
          if (pickerBtn) {
            var paction = pickerBtn.getAttribute('data-picker-action');
            var ptid = pickerBtn.getAttribute('data-target-id');
            if (paction === 'select-all' || paction === 'clear') {
              patchCustomModelTarget(ptid, { account: '', accountCondition: 'all' });
              closeCustomModelAccountPickers();
              renderCustomModelEditor();
              return;
            }
          }
        });
        stepsEl.addEventListener('change', function (e) {
          var el = e.target;
          if (!el || !el.getAttribute) return;
          if (el.hasAttribute && el.hasAttribute('data-picker-checkbox')) {
            var key = el.getAttribute('data-picker-checkbox');
            var pickTid = el.getAttribute('data-target-id');
            for (var i = 0; i < customModelEditorState.steps.length; i++) {
              var t = customModelEditorState.steps[i].targets.find(function (x) { return x.id === pickTid; });
              if (t) {
                var set = new Set(t.accounts);
                if (el.checked) set.add(key); else set.delete(key);
                patchCustomModelTarget(pickTid, { account: Array.from(set)[0] || '', accountCondition: set.size ? 'only' : 'all' });
                renderCustomModelEditor();
                return;
              }
            }
            return;
          }
          var tid = el.getAttribute('data-target-id');
          if (!tid) return;
          var field = el.getAttribute('data-target-field');
          if (field === 'provider') {
            patchCustomModelTarget(tid, { provider: el.value, model: '', account: '', accountCondition: 'all' });
            closeCustomModelAccountPickers();
            renderCustomModelEditor();
            renderCustomModelFieldErrors();
            return;
          }
          if (field === 'model') {
            patchCustomModelTarget(tid, { model: el.value });
            renderCustomModelPreview();
            renderCustomModelFieldErrors();
            return;
          }
          if (field === 'account_condition') {
            var condition = el.value === 'except' ? 'except' : el.value === 'only' ? 'only' : 'all';
            var patch = { accountCondition: condition };
            if (condition === 'all') patch.account = '';
            patchCustomModelTarget(tid, patch);
            renderCustomModelEditor();
            renderCustomModelFieldErrors();
            return;
          }
          if (field === 'account') {
            var account = String(el.value || '').trim();
            var currentCondition = 'all';
            for (var ci = 0; ci < customModelEditorState.steps.length; ci++) {
              var currentTarget = customModelEditorState.steps[ci].targets.find(function (x) { return x.id === tid; });
              if (currentTarget) {
                currentCondition = currentTarget.accountCondition === 'except' ? 'except' : currentTarget.accountCondition === 'only' ? 'only' : 'all';
                break;
              }
            }
            patchCustomModelTarget(tid, {
              account: account,
              accountCondition: account ? (currentCondition === 'all' ? 'only' : currentCondition) : 'all'
            });
            renderCustomModelEditor();
            renderCustomModelFieldErrors();
            return;
          }
          if (field === 'weight') {
            var w = parseInt(el.value, 10);
            if (!Number.isFinite(w) || w < 1) w = 1;
            el.value = String(w);
            patchCustomModelTarget(tid, { weight: w });
            renderCustomModelPreview();
            return;
          }
          if (field === 'enabled') {
            patchCustomModelTarget(tid, { enabled: el.checked });
            renderCustomModelEditor();
            renderCustomModelFieldErrors();
            return;
          }
        });
        stepsEl.addEventListener('input', function (e) {
          var el = e.target;
          if (!el || !el.getAttribute) return;
          if (el.getAttribute('data-target-field') !== 'model') return;
          var tid = el.getAttribute('data-target-id');
          if (!tid) return;
          patchCustomModelTarget(tid, { model: el.value });
          renderCustomModelPreview();
        });
        stepsEl.addEventListener('focusin', function (e) {
          var el = e.target;
          var tid = el.getAttribute && el.getAttribute('data-target-id');
          if (tid) customModelEditorState.lastFocusedTargetId = tid;
        });
        var aliasInput = document.getElementById('customModelAliasInput');
        if (aliasInput) aliasInput.addEventListener('input', function () {
          var errEl = document.getElementById('customModelAliasError');
          if (!errEl) return;
          var v = aliasInput.value.trim();
          if (!v) errEl.textContent = 'Alias is required.';
          else if (/[\s:/\\]/.test(v)) errEl.textContent = 'Alias must not contain whitespace, colon, slash, or backslash.';
          else errEl.textContent = '';
          renderCustomModelPreview();
        });
        var displayInput = document.getElementById('customModelDisplayNameInput');
        if (displayInput) displayInput.addEventListener('input', renderCustomModelPreview);
        var enabledInput = document.getElementById('customModelEnabledInput');
        if (enabledInput) enabledInput.addEventListener('change', renderCustomModelPreview);
        document.addEventListener('click', function (e) {
          if (!customModelAccountPickerOpenId) return;
          var popover = document.querySelector('[data-target-picker="' + customModelAccountPickerOpenId + '"]');
          if (!popover) return;
          if (popover.contains(e.target)) return;
          var trigger = document.querySelector('[data-target-action="open-picker"][data-target-id="' + customModelAccountPickerOpenId + '"]');
          if (trigger && trigger.contains(e.target)) return;
          closeCustomModelAccountPickers();
        }, true);
      }
      function deleteCustomModel(alias) {
        var model = findCustomModel(alias);
        var display = model && (model.display_name || customModelPublicId(model)) || ('ctm:' + normalizeCustomAlias(alias));
        openCredentialActionConfirm({
          title: 'Delete custom model?',
          message: 'Delete ' + display + '?',
          approveLabel: 'Delete',
          danger: true,
          run: function() { return performDeleteCustomModel(alias); }
        });
      }
      async function performDeleteCustomModel(alias) {
        const res = await adminFetch('/custom-models/delete', {
          method: 'POST',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify({ alias: alias })
        });
        if (!res) return;
        const data = await res.json();
        notify(data.message || 'Custom model deleted', data.ok === false ? 'error' : '');
        refreshCustomModels();
      }
      let lastGrokQuota = new Map();
      const providerCardSignatures = new Map();
      function renderProviderCards(provider, cardsId, badgeId, accounts, cards, emptyText) {
        const quota = dashboardState.quotas[provider];
        const quotaValue = quota instanceof Map ? Array.from(quota.entries()) : quota;
        const signature = JSON.stringify({ accounts: accounts, quota: quotaValue });
        if (providerCardSignatures.get(provider) === signature) return;
        providerCardSignatures.set(provider, signature);
        document.getElementById(cardsId).innerHTML = cards || '<div class="empty-state">' + emptyText + '</div>';
        document.getElementById(badgeId).textContent = accounts.length + ' accounts';
      }
      // Pick a stable key that matches whatever the accounts card uses first.
      function grokQuotaKey(a) {
        return a && (a.email || a.label || a.file_name) || '__grok__';
      }
      function findGrokQuota(accounts) {
        // Try the email/label/file_name of any account. The quota endpoint
        // returns a single snapshot (one xAI account), so we pick whichever
        // account's key matches the snapshot's account.
        for (var i = 0; i < accounts.length; i++) {
          var k = grokQuotaKey(accounts[i]);
          if (lastGrokQuota.has(k)) return lastGrokQuota.get(k);
        }
        // Fallback: any entry in the map (only one, when there's one account).
        var first = null;
        lastGrokQuota.forEach(function(v) { if (first === null) first = v; });
        return first;
      }
      async function refreshGrokAccounts() {
        var res;
        try { res = await adminFetch('/grok/accounts.json'); }
        catch (e) { console.error('refreshGrokAccounts fetch failed', e); return; }
        if (!res) return;
        var data;
        try { data = await res.json(); } catch (e) { console.error('refreshGrokAccounts parse failed', e); return; }
        var accounts = (data && data.accounts) || [];
        dashboardState.providers.grok = accounts;
        var quotaSnap = findGrokQuota(accounts);
        var cards = accounts.map(function(a) {
          // Per-card quota lookup: prefer the per-account key, but fall back
          // to any snapshot we have so the live overlay still shows up.
          var q = lastGrokQuota.get(grokQuotaKey(a)) || quotaSnap;
          return buildGrokCard(a, q);
        }).join('');
        renderProviderCards('grok', 'grokCards', 'grokBadgeCount', accounts, cards, 'No Grok accounts');
        updateOverview();
      }
      async function refreshGrokQuota() {
        let res;
        try { res = await adminFetch('/grok/quota.json'); }
        catch (e) { console.error('refreshGrokQuota fetch failed', e); return; }
        if (!res) return;
        let data;
        try { data = await res.json(); } catch (e) { console.error('refreshGrokQuota parse failed', e); return; }
        if (!data || !data.account) return;
        // Single-account snapshot. Key by the same priority the lookup uses.
        const key = data.account.email || data.account.label || data.account.file_name || '__grok__';
        lastGrokQuota = new Map([[key, data]]);
        dashboardState.quotas.grok = lastGrokQuota;
        updateOverview();
        // Re-render the cards so the live overlay shows up immediately.
        // (refreshGrokAccounts is also chained via .then() in startDashboard;
        //  call it here too so the overlay appears even on the 60s poll
        //  when no fresh accounts refresh is scheduled.)
        refreshGrokAccounts();
      }
      let contextChart = null;
      const chartColors = {
        input: '#3b82f6',
        output: '#22c55e',
        cache: '#f59e0b',
        reasoning: '#a855f7'
      };
      const contextPresets = {
        hour: { preset: 'hour', label: '1h', hours: 1, bucketMinutes: 1 },
        day: { preset: 'day', label: '24h', hours: 24, bucketMinutes: 5 },
        week: { preset: 'week', label: '7d', hours: 168, bucketMinutes: 60 }
      };
      let contextRange = readContextRange();
      function clampInteger(value, min, max, fallback) {
        var parsed = Number.parseInt(value, 10);
        if (!Number.isFinite(parsed)) parsed = fallback;
        return Math.max(min, Math.min(max, parsed));
      }
      function defaultBucketForHours(hours) {
        if (hours <= 2) return 1;
        if (hours <= 48) return 5;
        return 60;
      }
      function contextRangeLabel(hours) {
        if (hours === 1) return '1h';
        if (hours % 24 === 0) return (hours / 24) + 'd';
        return hours + 'h';
      }
      function normalizeContextRange(value) {
        var preset = value && value.preset;
        if (preset && preset !== 'custom' && contextPresets[preset]) {
          return Object.assign({}, contextPresets[preset]);
        }
        if (preset !== 'custom') {
          return Object.assign({}, contextPresets.day);
        }
        var hours = clampInteger(value && value.hours, 1, 720, 24);
        var bucketMinutes = clampInteger(value && (value.bucketMinutes || value.bucket_minutes), 1, 60, defaultBucketForHours(hours));
        return { preset: 'custom', label: contextRangeLabel(hours), hours: hours, bucketMinutes: bucketMinutes };
      }
      function readContextRange() {
        try {
          var saved = JSON.parse(localStorage.getItem(CONTEXT_RANGE_KEY) || 'null');
          return normalizeContextRange(saved);
        } catch (_) {
          return Object.assign({}, contextPresets.day);
        }
      }
      function writeContextRange(range) {
        try {
          localStorage.setItem(CONTEXT_RANGE_KEY, JSON.stringify(range));
        } catch (_) {}
      }
      function updateContextTitle() {
        setText('contextChartTitle', 'Context Usage (' + contextRange.label + ')');
      }
      function syncContextRangeControls() {
        updateContextTitle();
        var select = document.getElementById('contextRangeSelect');
        var customControls = document.getElementById('contextCustomControls');
        var hoursInput = document.getElementById('contextCustomHours');
        var bucketSelect = document.getElementById('contextBucketMinutes');
        if (select) select.value = contextRange.preset;
        if (customControls) customControls.hidden = contextRange.preset !== 'custom';
        if (hoursInput) hoursInput.value = contextRange.hours;
        if (bucketSelect) bucketSelect.value = String(contextRange.bucketMinutes);
      }
      function applyContextRange(range, refreshNow) {
        contextRange = normalizeContextRange(range);
        writeContextRange(contextRange);
        syncContextRangeControls();
        if (refreshNow) refreshContextChart();
      }
      function configureContextRangeControls() {
        syncContextRangeControls();
        var select = document.getElementById('contextRangeSelect');
        var applyButton = document.getElementById('contextApplyRangeBtn');
        if (select) {
          select.addEventListener('change', function() {
            var preset = select.value;
            if (preset === 'custom') {
              applyContextRange({
                preset: 'custom',
                hours: document.getElementById('contextCustomHours')?.value || contextRange.hours,
                bucketMinutes: document.getElementById('contextBucketMinutes')?.value || contextRange.bucketMinutes
              }, true);
            } else {
              applyContextRange({ preset: preset }, true);
            }
          });
        }
        if (applyButton) {
          applyButton.addEventListener('click', function() {
            applyContextRange({
              preset: 'custom',
              hours: document.getElementById('contextCustomHours')?.value || 24,
              bucketMinutes: document.getElementById('contextBucketMinutes')?.value || 5
            }, true);
          });
        }
      }
      function configureChartDisclosure() {
        const details = document.getElementById('chartDetails');
        if (!details) return;
        if (window.matchMedia('(max-width: 768px)').matches) {
          details.removeAttribute('open');
        }
        details.addEventListener('toggle', function() {
          if (details.open) {
            refreshContextChart();
          } else {
            ensureChartDestroyed();
          }
        });
      }
      function ensureChartDestroyed() {
        if (contextChart) {
          contextChart.destroy();
          contextChart = null;
        }
      }
      function formatCompact(value) {
        try {
          return new Intl.NumberFormat(undefined, { notation: 'compact', maximumFractionDigits: 1 }).format(Number(value || 0));
        } catch (_) {
          return formatNumber(value);
        }
      }
      function sumBucketField(buckets, field) {
        return buckets.reduce(function(total, bucket) {
          return total + Number(bucket && bucket[field] || 0);
        }, 0);
      }
      function updateContextSummary(buckets) {
        var summary = document.getElementById('contextUsageSummary');
        if (!summary) return;
        var input = sumBucketField(buckets, 'input_tokens');
        var output = sumBucketField(buckets, 'output_tokens');
        var cache = sumBucketField(buckets, 'cache_tokens');
        var reasoning = sumBucketField(buckets, 'reasoning_tokens');
        if (!input && !output && !cache && !reasoning) {
          summary.textContent = contextRange.label + ': no recorded usage';
          return;
        }
        summary.textContent = contextRange.label + ': '
          + formatCompact(input) + ' input, '
          + formatCompact(output) + ' output, '
          + formatCompact(cache) + ' cache, '
          + formatCompact(reasoning) + ' reasoning';
      }
      async function refreshContextChart() {
        try {
          const chartDetails = document.getElementById('chartDetails');
          const params = new URLSearchParams({
            hours: String(contextRange.hours),
            bucket_minutes: String(contextRange.bucketMinutes)
          });
          const res = await adminFetch('/usage/context-history.json?' + params.toString());
          if (!res) return;
          const data = await res.json();
          const labels = data.labels || [];
          const buckets = data.buckets || [];
          updateContextSummary(buckets);
          updateContextTitle();

          if (chartDetails && !chartDetails.open) {
            ensureChartDestroyed();
            return;
          }

          const inputData = [];
          const outputData = [];
          const cacheData = [];
          const reasoningData = [];
          for (const b of buckets) {
            inputData.push(b.input_tokens || 0);
            outputData.push(b.output_tokens || 0);
            cacheData.push(b.cache_tokens || 0);
            reasoningData.push(b.reasoning_tokens || 0);
          }

          const canvas = document.getElementById('contextChart');
          if (!canvas) return;
          if (typeof Chart === 'undefined') {
            ensureChartDestroyed();
            return;
          }

          ensureChartDestroyed();
          const ctx = canvas.getContext('2d');
          contextChart = new Chart(ctx, {
            type: 'line',
            data: {
              labels: labels,
              datasets: [
                {
                  label: 'Input',
                  data: inputData,
                  borderColor: chartColors.input,
                  backgroundColor: chartColors.input + '18',
                  borderWidth: 2,
                  pointRadius: 0,
                  pointHitRadius: 6,
                  tension: 0.2,
                  fill: true
                },
                {
                  label: 'Output',
                  data: outputData,
                  borderColor: chartColors.output,
                  backgroundColor: chartColors.output + '18',
                  borderWidth: 2,
                  pointRadius: 0,
                  pointHitRadius: 6,
                  tension: 0.2,
                  fill: true
                },
                {
                  label: 'Cache',
                  data: cacheData,
                  borderColor: chartColors.cache,
                  borderWidth: 1.5,
                  borderDash: [5, 3],
                  pointRadius: 0,
                  pointHitRadius: 6,
                  tension: 0.2,
                  fill: false
                },
                {
                  label: 'Reasoning',
                  data: reasoningData,
                  borderColor: chartColors.reasoning,
                  borderWidth: 1.5,
                  borderDash: [5, 3],
                  pointRadius: 0,
                  pointHitRadius: 6,
                  tension: 0.2,
                  fill: false
                }
              ]
            },
            options: {
              responsive: true,
              maintainAspectRatio: false,
              animation: false,
              interaction: {
                mode: 'index',
                intersect: false
              },
              plugins: {
                legend: { display: false },
                tooltip: {
                  callbacks: {
                    label: function(ctx) {
                      return ctx.dataset.label + ': ' + ctx.parsed.y.toLocaleString() + ' tokens';
                    }
                  }
                }
              },
              scales: {
                x: {
                  display: true,
                  ticks: { maxTicksLimit: 24, color: getComputedStyle(document.documentElement).getPropertyValue('--muted').trim() || '#94a3b8' },
                  grid: { display: false }
                },
                y: {
                  display: true,
                  beginAtZero: true,
                  ticks: {
                    callback: function(v) { return v >= 1000000 ? (v/1000000).toFixed(1)+'M' : v >= 1000 ? (v/1000).toFixed(0)+'K' : v; },
                    color: getComputedStyle(document.documentElement).getPropertyValue('--muted').trim() || '#94a3b8'
                  },
                  grid: { color: (getComputedStyle(document.documentElement).getPropertyValue('--border').trim() || '#25314a') + '40' }
                }
              }
            }
          });

          const legendDiv = document.getElementById('chartLegend');
          if (legendDiv) {
            legendDiv.innerHTML = [
              { key: 'input', label: 'Input' },
              { key: 'output', label: 'Output' },
              { key: 'cache', label: 'Cache' },
              { key: 'reasoning', label: 'Reasoning' }
            ].map(function(item) {
              return '<span class="chart-legend-item" data-key="' + item.key + '">'
                + '<span class="chart-legend-dot" style="background:' + chartColors[item.key] + ';"></span>'
                + item.label
                + '</span>';
            }).join('');
            legendDiv.querySelectorAll('.chart-legend-item').forEach(function(el) {
              el.addEventListener('click', function() {
                const key = el.getAttribute('data-key');
                const meta = contextChart.getDatasetMeta(
                  ['input','output','cache','reasoning'].indexOf(key)
                );
                meta.hidden = !meta.hidden;
                el.style.opacity = meta.hidden ? '0.4' : '1';
                contextChart.update();
              });
            });
          }
        } catch (_) {
          // Chart refresh is best-effort
        }
      }
    </script>
    <div id="addModal" class="modal" role="dialog" aria-modal="true" aria-labelledby="addCodexTitle" aria-hidden="true" style="display:none;">
      <div class="modal-card">
        <h2 id="addCodexTitle" style="margin-top:0;">Add Codex Account</h2>
        <p>Click start, open the URL in a new tab, complete login, then paste the callback URL below.</p>
        <button onclick="startLogin()">Start Login</button>
        <div id="status" class="muted" style="margin-top:8px;"></div>
        <pre id="authUrl" class="auth-url"></pre>
        <form id="loginForm" style="margin-top:16px;">
          <label for="codexRedirectInput">Callback URL</label>
          <input id="codexRedirectInput" name="redirect_url" placeholder="http://localhost:1455/auth/callback?code=...&state=...">
          <div class="modal-actions" style="margin-top:8px;">
            <button type="submit">Submit</button>
            <button type="button" id="closeModalBtn" class="secondary-button">Close</button>
          </div>
        </form>
      </div>
    </div>
    <div id="addAgwModal" class="modal" role="dialog" aria-modal="true" aria-labelledby="addAgwTitle" aria-hidden="true" style="display:none;">
      <div class="modal-card">
        <h2 id="addAgwTitle" style="margin-top:0;">Add Antigravity Account</h2>
        <p>Click start, log in with Google, then paste the callback URL below.</p>
        <button onclick="startAgwLogin()">Start Login</button>
        <div id="agwStatus" class="muted" style="margin-top:8px;"></div>
        <pre id="agwAuthUrl" class="auth-url"></pre>
        <form id="agwLoginForm" style="margin-top:16px;">
          <label for="agwRedirectInput">Callback URL</label>
          <input id="agwRedirectInput" name="redirect_url" placeholder="http://localhost:51121/oauth-callback?code=...&state=...">
          <div class="modal-actions" style="margin-top:8px;">
            <button type="submit">Submit</button>
            <button type="button" id="closeAgwModalBtn" class="secondary-button">Close</button>
          </div>
        </form>
      </div>
    </div>
    <div id="addGeminiModal" class="modal" role="dialog" aria-modal="true" aria-labelledby="addGeminiTitle" aria-hidden="true" style="display:none;">
      <div class="modal-card">
        <h2 id="addGeminiTitle" style="margin-top:0;">Add Gemini Account</h2>
        <p>Click start, complete Google OAuth, then paste the final callback URL below. Individual accounts should leave Project ID empty so Google can provide a managed Code Assist project.</p>
        <button onclick="startGeminiLogin()">Start Login</button>
        <div id="geminiStatus" class="muted" style="margin-top:8px;"></div>
        <pre id="geminiAuthUrl" class="auth-url"></pre>
        <form id="geminiLoginForm" style="margin-top:16px;">
          <label for="geminiRedirectInput">Callback URL</label>
          <input id="geminiRedirectInput" name="redirect_url" placeholder="http://localhost:8085/oauth2callback?code=...&state=...">
          <label for="geminiProjectInput" style="margin-top:12px;">Project ID</label>
          <input id="geminiProjectInput" name="project_id" placeholder="required only for Workspace or subscription accounts">
          <div class="muted" style="margin-top:8px;">Provide a Project ID only when your organization or Gemini Code Assist subscription requires one. The project must already have the required API and IAM access.</div>
          <div class="modal-actions" style="margin-top:8px;">
            <button type="submit">Submit</button>
            <button type="button" id="closeGeminiModalBtn" class="secondary-button">Close</button>
          </div>
        </form>
      </div>
    </div>
    <div id="addQwenModal" class="modal" role="dialog" aria-modal="true" aria-labelledby="addQwenTitle" aria-hidden="true" style="display:none;">
      <div class="modal-card">
        <h2 id="addQwenTitle" style="margin-top:0;">Add Qwen Account</h2>
        <p>Open the local Qwen token helper first. It explains the same browser-token flow used by <code>qwen-api</code> and gives you the extractor snippet for <code>chat.qwen.ai</code>.</p>
        <div class="modal-actions" style="margin-top:8px;">
          <button type="button" onclick="startQwenLogin()">Open Token Helper</button>
        </div>
        <p class="muted" style="margin-top:12px;">Direct fallback: open <code>chat.qwen.ai</code>, copy <code>localStorage.token</code> from the browser console, and paste it here.</p>
        <label for="qwenTokenInput" style="margin-top:12px;">Browser Token</label>
        <textarea id="qwenTokenInput" rows="6" placeholder="Paste chat.qwen.ai token here"></textarea>
        <div id="qwenStatus" class="muted" style="margin-top:8px;"></div>
        <div class="modal-actions" style="margin-top:16px;">
          <button type="button" onclick="submitQwenToken()">Save Token</button>
          <button type="button" id="closeQwenModalBtn" class="secondary-button">Close</button>
        </div>
      </div>
    </div>
    <div id="addDeepSeekModal" class="modal" role="dialog" aria-modal="true" aria-labelledby="addDeepSeekTitle" aria-hidden="true" style="display:none;">
      <div class="modal-card">
        <h2 id="addDeepSeekTitle" style="margin-top:0;">Add DeepSeek Account</h2>
        <p>Paste a DeepSeek API key. The gateway validates it against <code>/models</code> before saving it.</p>
        <div class="modal-actions" style="margin-top:8px;">
          <button type="button" onclick="window.open('/login/deepseek/start', '_blank', 'noopener')">Open Helper</button>
        </div>
        <label for="deepseekKeyInput" style="margin-top:12px;">API Key</label>
        <textarea id="deepseekKeyInput" rows="6" placeholder="Paste DeepSeek API key here"></textarea>
        <label for="deepseekLabelInput" style="margin-top:12px;">Label</label>
        <input id="deepseekLabelInput" placeholder="optional label">
        <label for="deepseekBaseUrlInput" style="margin-top:12px;">Base URL</label>
        <input id="deepseekBaseUrlInput" placeholder="https://api.deepseek.com">
        <div id="deepseekStatus" class="muted" style="margin-top:8px;"></div>
        <div class="modal-actions" style="margin-top:16px;">
          <button type="button" onclick="submitDeepSeekKey()">Save Key</button>
          <button type="button" id="closeDeepSeekModalBtn" class="secondary-button">Close</button>
        </div>
      </div>
    </div>
    <div id="addMiniMaxModal" class="modal" role="dialog" aria-modal="true" aria-labelledby="addMiniMaxTitle" aria-hidden="true" style="display:none;">
      <div class="modal-card">
        <h2 id="addMiniMaxTitle" style="margin-top:0;">Add MiniMax Account</h2>
        <p>Paste a MiniMax API key. The gateway validates it against <code>/v1/models</code> before saving it.</p>
        <div class="modal-actions" style="margin-top:8px;">
          <button type="button" onclick="window.open('/login/minimax/start', '_blank', 'noopener')">Open Helper</button>
        </div>
        <label for="minimaxKeyInput" style="margin-top:12px;">API Key</label>
        <textarea id="minimaxKeyInput" rows="6" placeholder="Paste MiniMax API key here"></textarea>
        <label for="minimaxLabelInput" style="margin-top:12px;">Label</label>
        <input id="minimaxLabelInput" placeholder="optional label">
        <label for="minimaxBaseUrlInput" style="margin-top:12px;">Base URL</label>
        <input id="minimaxBaseUrlInput" placeholder="https://api.minimax.io">
        <div id="minimaxStatus" class="muted" style="margin-top:8px;"></div>
        <div class="modal-actions" style="margin-top:16px;">
          <button type="button" onclick="submitMiniMaxKey()">Save Key</button>
          <button type="button" id="closeMiniMaxModalBtn" class="secondary-button">Close</button>
        </div>
      </div>
    </div>
    <div id="addGlmModal" class="modal" role="dialog" aria-modal="true" aria-labelledby="addGlmTitle" aria-hidden="true" style="display:none;">
      <div class="modal-card">
        <h2 id="addGlmTitle" style="margin-top:0;">Add GLM Account</h2>
        <p>Paste a Z.AI GLM API key. Pick the account type that matches your billing. The gateway probes the upstream balance endpoint on save and surfaces live balances when Z.AI exposes them.</p>
        <p class="muted" style="margin-top:4px;">Tip: pasting an OpenAI/Codex base URL with <code>/coding/</code> auto-selects <code>subscription</code>; <code>/paas/</code> auto-selects <code>api_usage</code>.</p>
        <div class="modal-actions" style="margin-top:8px;">
          <button type="button" onclick="window.open('/login/glm/start', '_blank', 'noopener')">Open Helper</button>
        </div>
        <label for="glmKeyInput" style="margin-top:12px;">API Key</label>
        <textarea id="glmKeyInput" rows="6" placeholder="Paste Z.AI API key here"></textarea>
        <label for="glmLabelInput" style="margin-top:12px;">Label</label>
        <input id="glmLabelInput" placeholder="optional label">
        <label for="glmAccountTypeInput" style="margin-top:12px;">Account Type</label>
        <select id="glmAccountTypeInput">
          <option value="api_usage" selected>API usage</option>
          <option value="subscription">Subscription</option>
        </select>
        <label for="glmOpenAiBaseUrlInput" style="margin-top:12px;">OpenAI/Codex Base URL</label>
        <input id="glmOpenAiBaseUrlInput" placeholder="API usage: https://api.z.ai/api/paas/v4">
        <label for="glmAnthropicBaseUrlInput" style="margin-top:12px;">Claude Code Base URL</label>
        <input id="glmAnthropicBaseUrlInput" placeholder="Subscription only: https://api.z.ai/api/anthropic">
        <div id="glmStatus" class="muted" style="margin-top:8px;"></div>
        <div class="modal-actions" style="margin-top:16px;">
          <button type="button" onclick="submitGlmKey()">Save Key</button>
          <button type="button" id="closeGlmModalBtn" class="secondary-button">Close</button>
        </div>
      </div>
    </div>
    <div id="addGrokModal" class="modal" role="dialog" aria-modal="true" aria-labelledby="addGrokTitle" aria-hidden="true" style="display:none;">
      <div class="modal-card">
        <h2 id="addGrokTitle" style="margin-top:0;">Add Grok Account</h2>
        <p>Click start, open the URL in a new tab, complete login with your SuperGrok or X Premium+ account, then paste the callback URL, the <code>?code=...&amp;state=...</code> fragment, or just the authorization code if xAI shows a completion page instead of redirecting.</p>
        <button onclick="startGrokLogin()">Start Login</button>
        <div id="grokStatus" class="muted" style="margin-top:8px;"></div>
        <pre id="grokAuthUrl" class="auth-url"></pre>
        <form id="grokLoginForm" style="margin-top:16px;">
          <label for="grokRedirectInput">Callback URL or Authorization Code</label>
          <input id="grokRedirectInput" name="redirect_url" placeholder="http://127.0.0.1:56121/callback?code=...&state=... or paste bare code">
          <input type="hidden" name="state" value="">
          <div class="modal-actions" style="margin-top:8px;">
            <button type="submit">Submit</button>
            <button type="button" id="closeGrokModalBtn" class="secondary-button">Close</button>
          </div>
        </form>
      </div>
    </div>
    <div id="addCopilotModal" class="modal" role="dialog" aria-modal="true" aria-labelledby="addCopilotTitle" aria-hidden="true" style="display:none;">
      <div class="modal-card">
        <h2 id="addCopilotTitle" style="margin-top:0;">Add GitHub Copilot Account</h2>
        <p>Use GitHub device login. The gateway will save the account after GitHub confirms the code. Direct token paste is available as a fallback.</p>
        <label for="copilotLabelInput" style="margin-top:12px;">Label</label>
        <input id="copilotLabelInput" placeholder="optional label">
        <label for="copilotAccountTypeInput" style="margin-top:12px;">Account Type</label>
        <select id="copilotAccountTypeInput">
          <option value="individual">Individual</option>
          <option value="business">Business</option>
          <option value="enterprise">Enterprise</option>
        </select>
        <button type="button" onclick="startCopilotLogin()" style="margin-top:12px;">Start Device Login</button>
        <div id="copilotStatus" class="muted" style="margin-top:8px;"></div>
        <pre id="copilotDeviceInfo" class="auth-url"></pre>
        <form id="copilotDeviceForm" style="margin-top:16px;">
          <input type="hidden" name="device_code" value="">
          <div class="modal-actions" style="margin-top:8px;">
            <button type="submit" id="copilotDeviceSubmitBtn" disabled>Check Now</button>
          </div>
        </form>
        <p class="muted" style="margin-top:16px;">Direct fallback: paste a GitHub token that can fetch <code>/copilot_internal/v2/token</code>.</p>
        <label for="copilotTokenInput" style="margin-top:12px;">GitHub Token</label>
        <textarea id="copilotTokenInput" rows="5" placeholder="Paste GitHub token here"></textarea>
        <div class="modal-actions" style="margin-top:16px;">
          <button type="button" onclick="submitCopilotToken()">Save Token</button>
          <button type="button" id="closeCopilotModalBtn" class="secondary-button">Close</button>
        </div>
      </div>
    </div>
    <div id="addClaudeModal" class="modal" role="dialog" aria-modal="true" aria-labelledby="addClaudeTitle" aria-hidden="true" style="display:none;">
      <div class="modal-card">
        <h2 id="addClaudeTitle" style="margin-top:0;">Add Claude Account</h2>
        <p>Use Claude OAuth login. Open the generated Claude login URL, finish login in the browser, then paste the displayed code or callback URL here.</p>
        <label for="claudeLabelInput" style="margin-top:12px;">Label</label>
        <input id="claudeLabelInput" placeholder="optional label">
        <label for="claudeOrganizationInput" style="margin-top:12px;">Organization UUID</label>
        <input id="claudeOrganizationInput" placeholder="optional label for saved account">
        <label for="claudeBaseUrlInput" style="margin-top:12px;">API Base URL</label>
        <input id="claudeBaseUrlInput" placeholder="https://api.anthropic.com">
        <button type="button" onclick="startClaudeLogin()" style="margin-top:12px;">Start Login</button>
        <div id="claudeStatus" class="muted" style="margin-top:8px;"></div>
        <pre id="claudeAuthUrl" class="auth-url"></pre>
        <form id="claudeLoginForm" style="margin-top:16px;">
          <label for="claudeRedirectInput">Authorization Code or Callback URL</label>
          <input id="claudeRedirectInput" name="redirect_url" placeholder="CODE#STATE or https://platform.claude.com/oauth/code/callback?code=...&state=...">
          <input type="hidden" name="state" value="">
          <div class="modal-actions" style="margin-top:8px;">
            <button type="submit">Submit</button>
          </div>
        </form>
        <p class="muted" style="margin-top:16px;">Cookie fallback: paste a Claude.ai browser cookie if the browser OAuth flow is unavailable.</p>
        <label for="claudeCookieInput" style="margin-top:12px;">Claude.ai Cookie</label>
        <textarea id="claudeCookieInput" rows="5" placeholder="Paste Claude.ai browser cookie here"></textarea>
        <p class="muted" style="margin-top:16px;">Direct fallback: paste Anthropic OAuth tokens from a trusted local Claude login.</p>
        <label for="claudeAccessTokenInput" style="margin-top:12px;">Access Token</label>
        <textarea id="claudeAccessTokenInput" rows="4" placeholder="OAuth access token"></textarea>
        <label for="claudeRefreshTokenInput" style="margin-top:12px;">Refresh Token</label>
        <textarea id="claudeRefreshTokenInput" rows="3" placeholder="optional refresh token"></textarea>
        <div class="modal-actions" style="margin-top:16px;">
          <button type="button" onclick="submitClaudeCookie()">Save Cookie</button>
          <button type="button" onclick="submitClaudeToken()">Save Token</button>
          <button type="button" id="closeClaudeModalBtn" class="secondary-button">Close</button>
        </div>
      </div>
    </div>
    <div id="appSettingsModal" class="modal" role="dialog" aria-modal="true" aria-labelledby="appSettingsTitle" aria-hidden="true" style="display:none;">
      <div class="modal-card">
        <h2 id="appSettingsTitle" style="margin-top:0;">Settings</h2>
        <div class="settings-tabs" role="tablist" aria-label="Settings sections">
          <button type="button" class="settings-tab is-active" role="tab" aria-selected="true" aria-controls="settingsDashboardPanel" data-settings-tab="dashboard">Dashboard</button>
          <button type="button" class="settings-tab" role="tab" aria-selected="false" aria-controls="settingsTestApiPanel" data-settings-tab="test-api">Test API</button>
          <button type="button" class="settings-tab" role="tab" aria-selected="false" aria-controls="settingsApiKeysPanel" data-settings-tab="api-keys">API Keys</button>
          <button type="button" class="settings-tab" role="tab" aria-selected="false" aria-controls="settingsNotificationsPanel" data-settings-tab="notifications">Notifications</button>
        </div>
        <div id="settingsDashboardPanel" class="settings-panel" role="tabpanel" data-settings-panel="dashboard">
          <div class="settings-block custom-model-form-row">
            <label>Provider layout</label>
            <div class="settings-segmented" role="group" aria-label="Provider layout">
              <button type="button" data-provider-layout-mode="column" aria-pressed="false" onclick="setProviderDashboardViewMode('column')">Column</button>
              <button type="button" data-provider-layout-mode="row" aria-pressed="false" onclick="setProviderDashboardViewMode('row')">Row</button>
            </div>
            <div class="settings-help">Applies only to provider sections. Custom models keep their current layout.</div>
          </div>
          <div class="settings-block custom-model-form-row">
            <label>Dashboard providers</label>
            <div id="providerSettingsList" class="provider-settings-list"></div>
          </div>
          <div id="appSettingsStatus" class="muted" style="margin-top:10px;"></div>
          <div class="modal-actions" style="margin-top:12px;">
            <button type="button" id="resetProviderSettingsBtn" class="secondary-button">Reset default</button>
          </div>
        </div>
        <div id="settingsTestApiPanel" class="settings-panel" role="tabpanel" data-settings-panel="test-api" hidden>
          <div class="settings-block">
            <div class="custom-model-form-row">
              <label for="testApiModelInput">Model</label>
              <input id="testApiModelInput" class="test-api-model-input" list="testApiModelList" autocomplete="off" placeholder="qwn:qwen3-coder-plus">
              <datalist id="testApiModelList"></datalist>
              <div class="settings-help">Uses dashboard admin access and the gateway's configured provider accounts.</div>
            </div>
            <div class="test-api-grid" style="margin-top:12px;">
              <div class="custom-model-form-row">
                <label for="testApiPromptInput">Prompt</label>
                <textarea id="testApiPromptInput" rows="7" placeholder="Ask a quick question"></textarea>
              </div>
              <div class="test-api-side">
                <div class="custom-model-form-row">
                  <label for="testApiInstructionsInput">Instructions</label>
                  <textarea id="testApiInstructionsInput" rows="4" placeholder="optional"></textarea>
                </div>
                <div class="custom-model-form-row">
                  <label for="testApiMaxOutputInput">Max output tokens</label>
                  <input id="testApiMaxOutputInput" type="number" min="1" max="8192" step="1" value="512">
                </div>
              </div>
            </div>
            <div class="modal-actions" style="margin-top:12px;">
              <button type="button" id="sendTestApiBtn">Send Test</button>
              <button type="button" id="clearTestApiBtn" class="secondary-button">Clear</button>
            </div>
            <div id="testApiStatus" class="muted" style="margin-top:10px;"></div>
            <div id="testApiResult" class="test-api-result" hidden>
              <div class="test-api-result-head">
                <strong id="testApiResultMeta">Result</strong>
                <button type="button" id="copyTestApiOutputBtn" class="mini-btn secondary-button">Copy</button>
              </div>
              <pre id="testApiOutput" class="test-api-output"></pre>
              <details>
                <summary>Raw response</summary>
                <pre id="testApiRaw" class="test-api-raw"></pre>
              </details>
            </div>
          </div>
        </div>
        <div id="settingsApiKeysPanel" class="settings-panel" role="tabpanel" data-settings-panel="api-keys" hidden>
          <div class="settings-block">
            <div class="custom-model-form-row">
              <label id="apiKeyEditorLabel" for="apiKeyLabelInput">Create API key</label>
              <div class="api-key-create-row">
                <input id="apiKeyLabelInput" autocomplete="off" placeholder="optional label">
                <div class="api-key-create-actions">
                  <button type="button" id="createApiKeyBtn">Create API Key</button>
                  <button type="button" id="cancelApiKeyEditBtn" class="secondary-button" hidden>Cancel</button>
                </div>
              </div>
              <div class="api-key-limit-row" style="margin-top:10px;">
                <label for="apiKeyPromptLimitInput">Whole key prompt token limit</label>
                <input id="apiKeyPromptLimitInput" type="number" min="1" step="1" inputmode="numeric" placeholder="unlimited">
              </div>
              <div class="api-key-access-editor">
                <label for="apiKeyAccessModeInput">Access</label>
                <select id="apiKeyAccessModeInput">
                  <option value="all">All providers and accounts</option>
                  <option value="restricted">Selected providers and accounts</option>
                </select>
                <div id="apiKeyAccessGroups" class="api-key-access-groups" hidden></div>
              </div>
              <div class="settings-help">API keys are for proxy API access only. Dashboard access uses OTP login.</div>
            </div>
            <div id="apiKeyRevealPanel" class="api-key-reveal" hidden>
              <div class="api-key-reveal-header">
                <strong>New API key</strong>
                <button type="button" id="copyApiKeyRevealBtn" class="mini-btn secondary-button">Copy</button>
              </div>
              <code id="apiKeyRevealValue"></code>
              <div class="settings-help">This value is shown once. Store it before closing the modal.</div>
            </div>
            <div id="apiKeysList" class="api-key-list"></div>
            <div id="apiKeyStatus" class="muted" style="margin-top:10px;"></div>
          </div>
        </div>
        <div id="settingsNotificationsPanel" class="settings-panel" role="tabpanel" data-settings-panel="notifications" hidden>
          <div class="settings-block">
            <div class="settings-block-title">
              <span>Notifications</span>
              <label class="check-row" for="notificationEnabledInput">
                <input id="notificationEnabledInput" type="checkbox">
                Enabled
              </label>
            </div>
            <div class="notification-channel-grid">
              <div class="custom-model-form-row">
                <label for="notificationChannelInput">Channel</label>
                <select id="notificationChannelInput">
                  <option value="telegram">Telegram</option>
                  <option value="google_chat">Google Chat</option>
                </select>
              </div>
              <div class="custom-model-form-row" id="telegramNotificationFields">
                <label for="telegramBotTokenInput">Telegram Bot Token</label>
                <input id="telegramBotTokenInput" type="password" autocomplete="off" placeholder="Telegram bot token">
                <label for="telegramChatIdInput" style="margin-top:8px;">Telegram Chat ID</label>
                <input id="telegramChatIdInput" autocomplete="off" placeholder="chat id">
              </div>
              <div class="custom-model-form-row" id="googleChatNotificationFields" hidden>
                <label for="googleChatWebhookInput">Google Chat Webhook URL</label>
                <textarea id="googleChatWebhookInput" rows="4" autocomplete="off" placeholder="Google Chat incoming webhook URL"></textarea>
              </div>
            </div>
            <div class="notification-watch-toolbar">
              <button type="button" class="mini-btn" onclick="setNotificationAllWatch(true)">Check all accounts</button>
              <button type="button" class="mini-btn secondary-button" onclick="setNotificationAllWatch(false)">Uncheck all accounts</button>
            </div>
            <div id="notificationWatchList" class="notification-watch-list"></div>
            <div id="notificationStatus" class="muted" style="margin-top:10px;"></div>
            <div class="modal-actions" style="margin-top:12px;">
              <button type="button" id="saveNotificationSettingsBtn">Save Notifications</button>
              <button type="button" id="testNotificationBtn" class="secondary-button">Send Test</button>
            </div>
          </div>
        </div>
        <div class="modal-actions" style="margin-top:16px;">
          <button type="button" id="closeAppSettingsModalBtn">Done</button>
        </div>
      </div>
    </div>
    <div id="customModelModal" class="modal" role="dialog" aria-modal="true" aria-labelledby="customModelTitle" aria-hidden="true" style="display:none;">
      <div class="modal-card custom-model-modal-card">
        <h2 id="customModelTitle" style="margin-top:0;">Add Custom Model</h2>
        <form id="customModelForm" class="custom-model-form">
          <input type="hidden" name="original_alias" value="">
          <div class="custom-model-form-row">
            <label for="customModelAliasInput">Alias</label>
            <div class="prefixed-input">
              <span>ctm:</span>
              <input id="customModelAliasInput" name="alias" placeholder="asdasd" autocomplete="off">
            </div>
          </div>
          <div class="custom-model-form-row">
            <label for="customModelDisplayNameInput">Display Name</label>
            <input id="customModelDisplayNameInput" name="display_name" placeholder="optional">
          </div>
          <div class="inline-checks">
            <label class="check-row" for="customModelEnabledInput">
              <input id="customModelEnabledInput" name="enabled" type="checkbox" checked>
              Enabled
            </label>
          </div>
          <div class="custom-model-form-row">
            <label>Fallback steps</label>
            <div class="muted">Each step runs if the previous one fails. Targets inside the same step are load-balanced. Use the account condition to route to any account, one account, or every account except the selected account.</div>
            <div id="customModelSteps" class="custom-model-steps"></div>
            <div><button type="button" id="addCustomStepBtn" class="mini-btn">+ Add fallback step</button></div>
            <div id="customModelAliasError" class="custom-model-field-error"></div>
          </div>
          <div class="custom-model-form-row">
            <label>Preview</label>
            <div id="customModelPreview" class="custom-model-preview-wrap"></div>
          </div>
          <textarea id="customModelRoutesInput" name="routes" hidden></textarea>
          <div id="customModelStatus" class="muted"></div>
          <div class="modal-actions" style="margin-top:8px;">
            <button type="submit">Save</button>
            <button type="button" id="closeCustomModelModalBtn" class="secondary-button">Close</button>
          </div>
        </form>
      </div>
    </div>
    <script>
      async function startLogin() {
        const res = await adminFetch('/login/codex/start');
        if (!res) return;
        const data = await res.json();
        if (data.url) {
          window.open(data.url, '_blank');
          document.getElementById('status').textContent = 'Opened login URL in new tab. If blocked, copy from below.';
          const pre = document.getElementById('authUrl');
          pre.textContent = data.url;
          pre.style.display = 'block';
        } else {
          document.getElementById('status').textContent = 'Failed to start login';
        }
      }
      document.getElementById('themeToggleBtn').addEventListener('click', toggleTheme);
      document.getElementById('appSettingsBtn').addEventListener('click', openAppSettingsModal);
      document.getElementById('openTestApiBtn').addEventListener('click', openTestApiPanel);
      // Provider selector menu
      document.getElementById('addProviderBtn').addEventListener('click', function(e) {
        e.stopPropagation();
        var menu = document.getElementById('providerMenu');
        var nextOpen = menu.hidden;
        menu.hidden = !nextOpen;
        this.setAttribute('aria-expanded', nextOpen ? 'true' : 'false');
        if (nextOpen) {
          var firstItem = menu.querySelector('.provider-menu-item');
          if (firstItem) firstItem.focus();
        }
      });
      document.addEventListener('click', function() {
        closeProviderMenu();
        closeAccountActionMenus();
      });
      document.addEventListener('keydown', function(e) {
        if (e.key !== 'Escape') return;
        closeProviderMenu();
        closeAccountActionMenus();
        if (document.getElementById('confirmActionModal').style.display !== 'none') {
          closeCredentialActionConfirm();
        }
        modalIds.forEach(function(id) { closeModal(id); });
      });
      document.getElementById('providerMenu').addEventListener('click', function(e) {
        e.stopPropagation();
      });
      document.querySelectorAll('.provider-menu-item').forEach(function(item) {
        item.addEventListener('click', function() {
          closeProviderMenu();
          setMobileNavOpen(false);
          var provider = item.getAttribute('data-provider');
          if (provider === 'codex') openModal('addModal');
          else if (provider === 'antigravity') openModal('addAgwModal');
          else if (provider === 'gemini') openModal('addGeminiModal');
          else if (provider === 'qwen') openModal('addQwenModal');
          else if (provider === 'deepseek') openModal('addDeepSeekModal');
          else if (provider === 'minimax') openModal('addMiniMaxModal');
          else if (provider === 'grok') openModal('addGrokModal');
          else if (provider === 'copilot') openModal('addCopilotModal');
          else if (provider === 'claude') openModal('addClaudeModal');
          else if (provider === 'glm') openModal('addGlmModal');
          else if (provider === 'custom-model') openCustomModelModal();
        });
      });
      document.getElementById('confirmActionApproveBtn').addEventListener('click', approveCredentialAction);
      document.getElementById('confirmActionRejectBtn').addEventListener('click', closeCredentialActionConfirm);
      document.getElementById('confirmActionModal').addEventListener('click', function(e) {
        if (e.target.id === 'confirmActionModal') {
          closeCredentialActionConfirm();
        }
      });
      document.getElementById('closeAppSettingsModalBtn').addEventListener('click', closeAppSettingsModal);
      document.getElementById('resetProviderSettingsBtn').addEventListener('click', resetProviderDashboardSettings);
      document.querySelectorAll('[data-settings-tab]').forEach(function(button) {
        button.addEventListener('click', function() {
          setAppSettingsTab(button.getAttribute('data-settings-tab'));
        });
      });
      document.getElementById('createApiKeyBtn').addEventListener('click', createApiKey);
      document.getElementById('cancelApiKeyEditBtn').addEventListener('click', resetApiKeyEditor);
      document.getElementById('apiKeyAccessModeInput').addEventListener('change', updateApiKeyAccessMode);
      document.getElementById('copyApiKeyRevealBtn').addEventListener('click', copyApiKeyReveal);
      document.getElementById('sendTestApiBtn').addEventListener('click', sendTestApi);
      document.getElementById('clearTestApiBtn').addEventListener('click', clearTestApi);
      document.getElementById('copyTestApiOutputBtn').addEventListener('click', copyTestApiOutput);
      document.getElementById('notificationChannelInput').addEventListener('change', updateNotificationChannelUi);
      document.getElementById('saveNotificationSettingsBtn').addEventListener('click', saveNotificationSettings);
      document.getElementById('testNotificationBtn').addEventListener('click', sendTestNotification);
      document.getElementById('appSettingsModal').addEventListener('click', function(e) {
        if (e.target.id === 'appSettingsModal') {
          closeAppSettingsModal();
        }
      });
      document.getElementById('closeModalBtn').addEventListener('click', () => {
        closeModal('addModal');
      });
      document.getElementById('addModal').addEventListener('click', (e) => {
        if (e.target.id === 'addModal') {
          closeModal('addModal');
        }
      });
      async function startAgwLogin() {
        const res = await adminFetch('/login/antigravity/start');
        if (!res) return;
        const data = await res.json();
        if (data.url) {
          window.open(data.url, '_blank');
          document.getElementById('agwStatus').textContent = 'Opened login URL in new tab. If blocked, copy from below.';
          const pre = document.getElementById('agwAuthUrl');
          pre.textContent = data.url;
          pre.style.display = 'block';
        } else {
          document.getElementById('agwStatus').textContent = data.message || 'Failed to start login';
        }
      }
      document.getElementById('closeAgwModalBtn').addEventListener('click', () => {
        closeModal('addAgwModal');
      });
      document.getElementById('addAgwModal').addEventListener('click', (e) => {
        if (e.target.id === 'addAgwModal') {
          closeModal('addAgwModal');
        }
      });
      async function startGeminiLogin() {
        const res = await adminFetch('/login/gemini/start');
        if (!res) return;
        const data = await res.json();
        if (data.url) {
          window.open(data.url, '_blank');
          document.getElementById('geminiStatus').textContent = 'Opened login URL in new tab. If blocked, copy from below.';
          const pre = document.getElementById('geminiAuthUrl');
          pre.textContent = data.url;
          pre.style.display = 'block';
        } else {
          document.getElementById('geminiStatus').textContent = data.message || 'Failed to start Gemini login';
        }
      }
      document.getElementById('closeGeminiModalBtn').addEventListener('click', () => {
        closeModal('addGeminiModal');
      });
      document.getElementById('addGeminiModal').addEventListener('click', (e) => {
        if (e.target.id === 'addGeminiModal') {
          closeModal('addGeminiModal');
        }
      });
      function closeQwenModal() {
        closeModal('addQwenModal');
      }
      function startQwenLogin() {
        const popup = window.open('/login/qwen/start', '_blank', 'noopener');
        if (!popup) {
          window.location.href = '/login/qwen/start';
          return;
        }
        document.getElementById('qwenStatus').textContent = 'Opened the Qwen token helper in a new tab. Follow the browser-token steps there, or use the direct token fallback below.';
      }
      async function submitQwenToken() {
        const token = document.getElementById('qwenTokenInput').value.trim();
        if (!token) {
          document.getElementById('qwenStatus').textContent = 'Paste a Qwen browser token first.';
          return;
        }
        const res = await adminFetch('/login/qwen/start', {
          method: 'POST',
          headers: {
            'Content-Type': 'application/json'
          },
          body: JSON.stringify({ token })
        });
        if (!res) return;
        const data = await res.json();
        document.getElementById('qwenStatus').textContent = data.message || 'Failed to save Qwen token';
        if (!data.ok) {
          return;
        }
        document.getElementById('qwenTokenInput').value = '';
        refreshQwenAccounts();
      }
      document.getElementById('closeQwenModalBtn').addEventListener('click', () => {
        closeQwenModal();
      });
      document.getElementById('addQwenModal').addEventListener('click', (e) => {
        if (e.target.id === 'addQwenModal') {
          closeQwenModal();
        }
      });
      function closeDeepSeekModal() {
        closeModal('addDeepSeekModal');
      }
      async function submitDeepSeekKey() {
        const apiKey = document.getElementById('deepseekKeyInput').value.trim();
        const label = document.getElementById('deepseekLabelInput').value.trim();
        const baseUrl = document.getElementById('deepseekBaseUrlInput').value.trim();
        if (!apiKey) {
          document.getElementById('deepseekStatus').textContent = 'Paste a DeepSeek API key first.';
          return;
        }
        const res = await adminFetch('/login/deepseek/start', {
          method: 'POST',
          headers: {
            'Content-Type': 'application/json'
          },
          body: JSON.stringify({
            api_key: apiKey,
            label: label || undefined,
            base_url: baseUrl || undefined
          })
        });
        if (!res) return;
        const data = await res.json();
        document.getElementById('deepseekStatus').textContent = data.message || 'Failed to save DeepSeek key';
        if (!data.ok) {
          return;
        }
        document.getElementById('deepseekKeyInput').value = '';
        document.getElementById('deepseekLabelInput').value = '';
        document.getElementById('deepseekBaseUrlInput').value = '';
        refreshDeepSeekAccounts();
      }
      document.getElementById('closeDeepSeekModalBtn').addEventListener('click', () => {
        closeDeepSeekModal();
      });
      document.getElementById('addDeepSeekModal').addEventListener('click', (e) => {
        if (e.target.id === 'addDeepSeekModal') {
          closeDeepSeekModal();
        }
      });
      function closeMiniMaxModal() {
        closeModal('addMiniMaxModal');
      }
      async function submitMiniMaxKey() {
        const apiKey = document.getElementById('minimaxKeyInput').value.trim();
        const label = document.getElementById('minimaxLabelInput').value.trim();
        const baseUrl = document.getElementById('minimaxBaseUrlInput').value.trim();
        if (!apiKey) {
          document.getElementById('minimaxStatus').textContent = 'Paste a MiniMax API key first.';
          return;
        }
        const res = await adminFetch('/login/minimax/start', {
          method: 'POST',
          headers: {
            'Content-Type': 'application/json'
          },
          body: JSON.stringify({
            api_key: apiKey,
            label: label || undefined,
            base_url: baseUrl || undefined
          })
        });
        if (!res) return;
        const data = await res.json();
        document.getElementById('minimaxStatus').textContent = data.message || 'Failed to save MiniMax key';
        if (!data.ok) {
          return;
        }
        document.getElementById('minimaxKeyInput').value = '';
        document.getElementById('minimaxLabelInput').value = '';
        document.getElementById('minimaxBaseUrlInput').value = '';
        refreshMiniMaxAccounts();
      }
      document.getElementById('closeMiniMaxModalBtn').addEventListener('click', () => {
        closeMiniMaxModal();
      });
      document.getElementById('addMiniMaxModal').addEventListener('click', (e) => {
        if (e.target.id === 'addMiniMaxModal') {
          closeMiniMaxModal();
        }
      });
      function closeGlmModal() {
        closeModal('addGlmModal');
      }
      async function submitGlmKey() {
        const apiKey = document.getElementById('glmKeyInput').value.trim();
        const label = document.getElementById('glmLabelInput').value.trim();
        const accountType = document.getElementById('glmAccountTypeInput').value;
        const openaiBaseUrl = document.getElementById('glmOpenAiBaseUrlInput').value.trim();
        const anthropicBaseUrl = document.getElementById('glmAnthropicBaseUrlInput').value.trim();
        if (!apiKey) {
          document.getElementById('glmStatus').textContent = 'Paste a GLM API key first.';
          return;
        }
        const res = await adminFetch('/login/glm/start', {
          method: 'POST',
          headers: {
            'Content-Type': 'application/json'
          },
          body: JSON.stringify({
            api_key: apiKey,
            label: label || undefined,
            account_type: accountType,
            openai_base_url: openaiBaseUrl || undefined,
            anthropic_base_url: anthropicBaseUrl || undefined
          })
        });
        if (!res) return;
        const data = await res.json();
        const statusEl = document.getElementById('glmStatus');
        let html = escapeHtml(data.message || 'Failed to save GLM key');
        if (data.balance_status) {
          const status = String(data.balance_status);
          let tone = 'muted';
          let icon = '\u2022';
          if (status === 'live') { tone = 'success'; icon = '\u2705'; }
          else if (status === 'unreachable') { tone = 'warn'; icon = '\u26a0\ufe0f'; }
          else if (status === 'probe_error') { tone = 'warn'; icon = '\u26a0\ufe0f'; }
          else if (status === 'subscription') { tone = 'info'; icon = '\u2139\ufe0f'; }
          html += '<br><span class="glm-balance-tone ' + tone + '">' + icon + ' Balance ' +
            status + ': ' + escapeHtml(data.balance_message || '') + '</span>';
          if (Array.isArray(data.balances) && data.balances.length) {
            html += '<ul class="glm-balance-list">';
            data.balances.forEach(function(b) {
              html += '<li><code>' + escapeHtml((b.total_balance || '?') + ' ' + (b.currency || '')) + '</code>';
              if (b.granted_balance && b.granted_balance !== '0.00') {
                html += ' (granted ' + escapeHtml(b.granted_balance) + ')';
              } else if (b.topped_up_balance && b.topped_up_balance !== '0.00') {
                html += ' (topped up ' + escapeHtml(b.topped_up_balance) + ')';
              }
              html += '</li>';
            });
            html += '</ul>';
          }
        }
        statusEl.innerHTML = html;
        if (!data.ok) {
          return;
        }
        document.getElementById('glmKeyInput').value = '';
        document.getElementById('glmLabelInput').value = '';
        document.getElementById('glmAccountTypeInput').value = 'api_usage';
        document.getElementById('glmOpenAiBaseUrlInput').value = '';
        document.getElementById('glmAnthropicBaseUrlInput').value = '';
        refreshGlmQuota();
        refreshGlmAccounts();
      }

      function detectGlmAccountTypeFromUrl(value) {
        var v = String(value || '').toLowerCase();
        if (!v) return null;
        if (v.indexOf('/api/coding/') !== -1 || v.indexOf('/coding/') !== -1) return 'subscription';
        if (v.indexOf('/api/paas/') !== -1 || v.indexOf('/paas/') !== -1) return 'api_usage';
        if (v.indexOf('/api/anthropic') !== -1 || v.indexOf('anthropic') !== -1) return 'subscription';
        return null;
      }
      var glmOpenAiBaseInput = document.getElementById('glmOpenAiBaseUrlInput');
      if (glmOpenAiBaseInput) {
        glmOpenAiBaseInput.addEventListener('input', function() {
          var detected = detectGlmAccountTypeFromUrl(glmOpenAiBaseInput.value);
          var select = document.getElementById('glmAccountTypeInput');
          if (!select || !detected) return;
          if (!select.dataset.userTouched) {
            select.value = detected;
            var preview = document.getElementById('glmAnthropicBaseUrlInput');
            if (detected === 'subscription' && preview && !preview.value) {
              preview.placeholder = 'Subscription default: https://api.z.ai/api/anthropic';
            }
          }
        });
        glmOpenAiBaseInput.addEventListener('blur', function() {
          var detected = detectGlmAccountTypeFromUrl(glmOpenAiBaseInput.value);
          var select = document.getElementById('glmAccountTypeInput');
          if (select && detected && !select.dataset.userTouched) {
            select.value = detected;
          }
        });
      }
      var glmAccountTypeSelect = document.getElementById('glmAccountTypeInput');
      if (glmAccountTypeSelect) {
        glmAccountTypeSelect.addEventListener('change', function() {
          glmAccountTypeSelect.dataset.userTouched = '1';
        });
      }
      document.getElementById('closeGlmModalBtn').addEventListener('click', () => {
        closeGlmModal();
      });
      document.getElementById('addGlmModal').addEventListener('click', (e) => {
        if (e.target.id === 'addGlmModal') {
          closeGlmModal();
        }
      });
      // Grok modal
      function closeGrokModal() {
        closeModal('addGrokModal');
        const form = document.getElementById('grokLoginForm');
        form.querySelector('input[name="state"]').value = '';
        form.querySelector('input[name="redirect_url"]').value = '';
      }
      async function startGrokLogin() {
        document.getElementById('grokStatus').textContent = 'Starting login...';
        const res = await adminFetch('/login/grok/start');
        if (!res) return;
        const data = await res.json();
        if (data.url) {
          document.getElementById('grokLoginForm').querySelector('input[name="state"]').value = data.state || '';
          window.open(data.url, '_blank');
          document.getElementById('grokStatus').textContent = 'Opened login URL. Complete login, then paste the callback URL or the bare authorization code if xAI shows it directly.';
          const pre = document.getElementById('grokAuthUrl');
          pre.textContent = data.url;
          pre.style.display = 'block';
        } else {
          document.getElementById('grokStatus').textContent = data.message || 'Failed to start Grok login';
        }
      }
      document.getElementById('closeGrokModalBtn').addEventListener('click', () => {
        closeGrokModal();
      });
      document.getElementById('addGrokModal').addEventListener('click', (e) => {
        if (e.target.id === 'addGrokModal') {
          closeGrokModal();
        }
      });
      document.getElementById('grokLoginForm').addEventListener('submit', async (e) => {
        e.preventDefault();
        const form = e.target;
        const input = form.querySelector('input[name="redirect_url"]');
        const stateInput = form.querySelector('input[name="state"]');
        const redirectUrl = input.value.trim();
        if (!redirectUrl) {
          document.getElementById('grokStatus').textContent = 'Callback URL or authorization code is required.';
          return;
        }
        const res = await adminFetch('/login/grok/submit', {
          method: 'POST',
          headers: { 'Content-Type': 'application/x-www-form-urlencoded' },
          body: new URLSearchParams({ redirect_url: redirectUrl, state: stateInput.value || '' })
        });
        if (!res) return;
        const data = await res.json();
        document.getElementById('grokStatus').textContent = data.message || 'Login completed.';
        if (data.ok) {
          input.value = '';
          stateInput.value = '';
          refreshGrokAccounts();
          refreshMiniMaxAccounts();
        }
      });
      function closeCopilotModal() {
        clearCopilotDevicePoll();
        closeModal('addCopilotModal');
        const form = document.getElementById('copilotDeviceForm');
        form.querySelector('input[name="device_code"]').value = '';
        document.getElementById('copilotDeviceSubmitBtn').disabled = true;
        document.getElementById('copilotTokenInput').value = '';
        document.getElementById('copilotDeviceInfo').textContent = '';
        document.getElementById('copilotDeviceInfo').style.display = 'none';
      }
      function clearCopilotDevicePoll() {
        if (copilotDevicePollTimer) {
          clearTimeout(copilotDevicePollTimer);
          copilotDevicePollTimer = null;
        }
        copilotDeviceExpiresAt = 0;
      }
      function copilotPendingMessage(message) {
        var lower = String(message || '').toLowerCase();
        return lower.indexOf('authorization_pending') !== -1 || lower.indexOf('slow_down') !== -1;
      }
      function scheduleCopilotDevicePoll(delayMs) {
        if (copilotDevicePollTimer) {
          clearTimeout(copilotDevicePollTimer);
        }
        copilotDevicePollTimer = setTimeout(function() {
          copilotDevicePollTimer = null;
          pollCopilotDevice(true);
        }, delayMs);
      }
      function resetCopilotDeviceUi() {
        const form = document.getElementById('copilotDeviceForm');
        form.querySelector('input[name="device_code"]').value = '';
        document.getElementById('copilotDeviceSubmitBtn').disabled = true;
        document.getElementById('copilotDeviceInfo').textContent = '';
        document.getElementById('copilotDeviceInfo').style.display = 'none';
      }
      async function pollCopilotDevice(autoPoll) {
        const form = document.getElementById('copilotDeviceForm');
        const label = document.getElementById('copilotLabelInput').value.trim();
        const accountType = document.getElementById('copilotAccountTypeInput').value || 'individual';
        const deviceCode = form.querySelector('input[name="device_code"]').value.trim();
        if (!deviceCode) {
          if (!autoPoll) document.getElementById('copilotStatus').textContent = 'Start device login first.';
          return;
        }
        if (copilotDeviceExpiresAt && Date.now() > copilotDeviceExpiresAt) {
          clearCopilotDevicePoll();
          document.getElementById('copilotStatus').textContent = 'GitHub device code expired. Start device login again.';
          document.getElementById('copilotDeviceSubmitBtn').disabled = true;
          return;
        }
        if (copilotDevicePollInFlight) return;
        copilotDevicePollInFlight = true;
        if (!autoPoll) {
          document.getElementById('copilotStatus').textContent = 'Checking GitHub authorization...';
        }
        const res = await adminFetch('/login/copilot/submit', {
          method: 'POST',
          headers: { 'Content-Type': 'application/x-www-form-urlencoded' },
          body: new URLSearchParams({
            device_code: deviceCode,
            label: label,
            account_type: accountType
          })
        });
        copilotDevicePollInFlight = false;
        if (!res) return;
        if (form.querySelector('input[name="device_code"]').value.trim() !== deviceCode) {
          return;
        }
        const data = await res.json();
        const message = data.message || '';
        if (data.ok) {
          clearCopilotDevicePoll();
          resetCopilotDeviceUi();
          document.getElementById('copilotStatus').textContent = message || 'GitHub Copilot account saved.';
          refreshCopilotQuota();
          refreshCopilotAccounts();
          return;
        }
        if (copilotPendingMessage(message)) {
          document.getElementById('copilotStatus').textContent = 'Waiting for GitHub approval... Keep this dialog open after approving the device code.';
          scheduleCopilotDevicePoll(message.toLowerCase().indexOf('slow_down') !== -1 ? 10000 : 6000);
          return;
        }
        clearCopilotDevicePoll();
        document.getElementById('copilotStatus').textContent = message || 'Failed to save Copilot account.';
      }
      async function startCopilotLogin() {
        clearCopilotDevicePoll();
        const label = document.getElementById('copilotLabelInput').value.trim();
        const accountType = document.getElementById('copilotAccountTypeInput').value || 'individual';
        document.getElementById('copilotStatus').textContent = 'Starting GitHub device login...';
        const res = await adminFetch('/login/copilot/start', {
          method: 'POST',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify({ label: label || undefined, account_type: accountType })
        });
        if (!res) return;
        const data = await res.json();
        if (!data.ok) {
          document.getElementById('copilotStatus').textContent = data.message || 'Failed to start Copilot login';
          return;
        }
        const form = document.getElementById('copilotDeviceForm');
        form.querySelector('input[name="device_code"]').value = data.device_code || '';
        document.getElementById('copilotDeviceSubmitBtn').disabled = !data.device_code;
        copilotDeviceExpiresAt = Date.now() + Math.max(1, Number(data.expires_in || 900)) * 1000;
        if (data.verification_uri) {
          window.open(data.verification_uri, '_blank');
        }
        document.getElementById('copilotStatus').textContent = 'Enter the GitHub code and approve access. This dialog will save the account automatically.';
        const pre = document.getElementById('copilotDeviceInfo');
        pre.textContent = 'Open: ' + (data.verification_uri || 'https://github.com/login/device') + '\nCode: ' + (data.user_code || '') + '\nDevice code: ' + (data.device_code || '');
        pre.style.display = 'block';
        if (data.device_code) {
          scheduleCopilotDevicePoll((Math.max(1, Number(data.interval || 5)) + 1) * 1000);
        }
      }
      async function submitCopilotToken() {
        clearCopilotDevicePoll();
        const githubToken = document.getElementById('copilotTokenInput').value.trim();
        const label = document.getElementById('copilotLabelInput').value.trim();
        const accountType = document.getElementById('copilotAccountTypeInput').value || 'individual';
        if (!githubToken) {
          document.getElementById('copilotStatus').textContent = 'Paste a GitHub token first.';
          return;
        }
        document.getElementById('copilotStatus').textContent = 'Validating GitHub token...';
        const res = await adminFetch('/login/copilot/start', {
          method: 'POST',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify({
            github_token: githubToken,
            label: label || undefined,
            account_type: accountType
          })
        });
        if (!res) return;
        const data = await res.json();
        document.getElementById('copilotStatus').textContent = data.message || 'Failed to save Copilot token';
        if (data.ok) {
          document.getElementById('copilotTokenInput').value = '';
          refreshCopilotQuota();
          refreshCopilotAccounts();
        }
      }
      function closeClaudeModal() {
        closeModal('addClaudeModal');
        const form = document.getElementById('claudeLoginForm');
        form.querySelector('input[name="state"]').value = '';
        document.getElementById('claudeRedirectInput').value = '';
        document.getElementById('claudeAuthUrl').textContent = '';
        document.getElementById('claudeAuthUrl').style.display = 'none';
        document.getElementById('claudeCookieInput').value = '';
        document.getElementById('claudeAccessTokenInput').value = '';
        document.getElementById('claudeRefreshTokenInput').value = '';
      }
      function claudePayloadBase() {
        const label = document.getElementById('claudeLabelInput').value.trim();
        const organizationUuid = document.getElementById('claudeOrganizationInput').value.trim();
        const baseUrl = document.getElementById('claudeBaseUrlInput').value.trim();
        const payload = {};
        if (label) payload.label = label;
        if (organizationUuid) payload.organization_uuid = organizationUuid;
        if (baseUrl) payload.base_url = baseUrl;
        return payload;
      }
      async function startClaudeLogin() {
        const payload = claudePayloadBase();
        const params = new URLSearchParams();
        if (payload.label) params.set('label', payload.label);
        if (payload.organization_uuid) params.set('organization_uuid', payload.organization_uuid);
        if (payload.base_url) params.set('base_url', payload.base_url);
        document.getElementById('claudeStatus').textContent = 'Starting Claude OAuth login...';
        const query = params.toString();
        const res = await adminFetch('/login/claude/start' + (query ? '?' + query : ''));
        if (!res) return;
        const data = await res.json();
        if (!data.ok || !data.url) {
          document.getElementById('claudeStatus').textContent = data.message || 'Failed to start Claude login.';
          return;
        }
        const form = document.getElementById('claudeLoginForm');
        form.querySelector('input[name="state"]').value = data.state || '';
        window.open(data.url, '_blank');
        document.getElementById('claudeStatus').textContent = 'Opened Claude login URL in a new tab. After login, paste the displayed code or callback URL below.';
        const pre = document.getElementById('claudeAuthUrl');
        pre.textContent = data.url;
        pre.style.display = 'block';
      }
      async function submitClaudeRedirect(e) {
        e.preventDefault();
        const form = document.getElementById('claudeLoginForm');
        const redirectUrl = document.getElementById('claudeRedirectInput').value.trim();
        if (!redirectUrl) {
          document.getElementById('claudeStatus').textContent = 'Authorization code or callback URL is required.';
          return;
        }
        const payload = claudePayloadBase();
        const body = new URLSearchParams({
          redirect_url: redirectUrl,
          state: form.querySelector('input[name="state"]').value || ''
        });
        if (payload.label) body.set('label', payload.label);
        if (payload.organization_uuid) body.set('organization_uuid', payload.organization_uuid);
        if (payload.base_url) body.set('base_url', payload.base_url);
        document.getElementById('claudeStatus').textContent = 'Completing Claude OAuth login...';
        const res = await adminFetch('/login/claude/submit', {
          method: 'POST',
          headers: { 'Content-Type': 'application/x-www-form-urlencoded' },
          body: body
        });
        if (!res) return;
        const data = await res.json();
        document.getElementById('claudeStatus').textContent = data.message || (data.ok ? 'Claude account saved.' : 'Failed to save Claude account.');
        if (data.ok) {
          form.querySelector('input[name="state"]').value = '';
          document.getElementById('claudeRedirectInput').value = '';
          document.getElementById('claudeAuthUrl').textContent = '';
          document.getElementById('claudeAuthUrl').style.display = 'none';
          refreshClaudeQuota();
          refreshClaudeAccounts();
        }
      }
      async function submitClaudeCookie() {
        const cookie = document.getElementById('claudeCookieInput').value.trim();
        if (!cookie) {
          document.getElementById('claudeStatus').textContent = 'Paste a Claude.ai cookie first.';
          return;
        }
        document.getElementById('claudeStatus').textContent = 'Exchanging Claude cookie for OAuth tokens...';
        const payload = claudePayloadBase();
        payload.cookie = cookie;
        const res = await adminFetch('/login/claude/start', {
          method: 'POST',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify(payload)
        });
        if (!res) return;
        const data = await res.json();
        document.getElementById('claudeStatus').textContent = data.message || (data.ok ? 'Claude account saved.' : 'Failed to save Claude account.');
        if (data.organizations && data.organizations.length) {
          document.getElementById('claudeStatus').textContent += ' Choose an organization UUID and try again.';
        }
        if (data.ok) {
          document.getElementById('claudeCookieInput').value = '';
          refreshClaudeQuota();
          refreshClaudeAccounts();
        }
      }
      async function submitClaudeToken() {
        const accessToken = document.getElementById('claudeAccessTokenInput').value.trim();
        const refreshToken = document.getElementById('claudeRefreshTokenInput').value.trim();
        if (!accessToken) {
          document.getElementById('claudeStatus').textContent = 'Paste a Claude OAuth access token first.';
          return;
        }
        document.getElementById('claudeStatus').textContent = 'Validating Claude token...';
        const payload = claudePayloadBase();
        payload.access_token = accessToken;
        if (refreshToken) payload.refresh_token = refreshToken;
        const res = await adminFetch('/login/claude/start', {
          method: 'POST',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify(payload)
        });
        if (!res) return;
        const data = await res.json();
        document.getElementById('claudeStatus').textContent = data.message || (data.ok ? 'Claude account saved.' : 'Failed to save Claude token.');
        if (data.ok) {
          document.getElementById('claudeAccessTokenInput').value = '';
          document.getElementById('claudeRefreshTokenInput').value = '';
          refreshClaudeQuota();
          refreshClaudeAccounts();
        }
      }
      document.getElementById('closeCopilotModalBtn').addEventListener('click', () => {
        closeCopilotModal();
      });
      document.getElementById('addCopilotModal').addEventListener('click', (e) => {
        if (e.target.id === 'addCopilotModal') {
          closeCopilotModal();
        }
      });
      document.getElementById('closeClaudeModalBtn').addEventListener('click', () => {
        closeClaudeModal();
      });
      document.getElementById('addClaudeModal').addEventListener('click', (e) => {
        if (e.target.id === 'addClaudeModal') {
          closeClaudeModal();
        }
      });
      document.getElementById('claudeLoginForm').addEventListener('submit', submitClaudeRedirect);
      document.getElementById('addCustomModelBtn').addEventListener('click', () => {
        openCustomModelModal();
      });
      document.getElementById('closeCustomModelModalBtn').addEventListener('click', () => {
        closeCustomModelModal();
      });
      document.getElementById('customModelModal').addEventListener('click', (e) => {
        if (e.target.id === 'customModelModal') {
          closeCustomModelModal();
        }
      });
      document.getElementById('customModelForm').addEventListener('submit', submitCustomModelForm);
      bindCustomModelEditorEvents();
      document.getElementById('copilotDeviceForm').addEventListener('submit', async (e) => {
        e.preventDefault();
        await pollCopilotDevice(false);
      });
      document.getElementById('loginForm').addEventListener('submit', async (e) => {
        e.preventDefault();
        const form = e.target;
        const input = form.querySelector('input[name="redirect_url"]');
        const redirectUrl = input.value.trim();
        if (!redirectUrl) {
          document.getElementById('status').textContent = 'Callback URL is required.';
          return;
        }
        const res = await adminFetch('/login/codex/submit', {
          method: 'POST',
          headers: { 'Content-Type': 'application/x-www-form-urlencoded' },
          body: new URLSearchParams({ redirect_url: redirectUrl })
        });
        if (!res) return;
        const data = await res.json();
        document.getElementById('status').textContent = data.message || 'Login completed.';
        if (data.ok) {
          refresh();
        }
      });
      document.getElementById('agwLoginForm').addEventListener('submit', async (e) => {
        e.preventDefault();
        const form = e.target;
        const input = form.querySelector('input[name="redirect_url"]');
        const redirectUrl = input.value.trim();
        if (!redirectUrl) {
          document.getElementById('agwStatus').textContent = 'Callback URL is required.';
          return;
        }
        const res = await adminFetch('/login/antigravity/submit', {
          method: 'POST',
          headers: { 'Content-Type': 'application/x-www-form-urlencoded' },
          body: new URLSearchParams({ redirect_url: redirectUrl })
        });
        if (!res) return;
        const data = await res.json();
        document.getElementById('agwStatus').textContent = data.message || 'Login completed.';
        if (data.ok) {
          refreshAgwQuota();
          refreshAgwAccounts();
        }
      });
      document.getElementById('geminiLoginForm').addEventListener('submit', async (e) => {
        e.preventDefault();
        const form = e.target;
        const redirectInput = form.querySelector('input[name="redirect_url"]');
        const projectInput = form.querySelector('input[name="project_id"]');
        const redirectUrl = redirectInput.value.trim();
        const projectId = projectInput.value.trim();
        if (!redirectUrl) {
          document.getElementById('geminiStatus').textContent = 'Callback URL is required.';
          return;
        }
        const res = await adminFetch('/login/gemini/submit', {
          method: 'POST',
          headers: { 'Content-Type': 'application/x-www-form-urlencoded' },
          body: new URLSearchParams({ redirect_url: redirectUrl, project_id: projectId })
        });
        if (!res) return;
        const data = await res.json();
        document.getElementById('geminiStatus').textContent = data.message || 'Login completed.';
        if (data.ok) {
          refreshGeminiQuota();
          refreshGeminiAccounts();
        }
      });
      function deleteCred(fileName, label) {
        const display = label || fileName || 'this credential';
        openCredentialActionConfirm({
          title: 'Delete credential?',
          message: 'Delete ' + display + '? This cannot be undone.',
          approveLabel: 'Delete',
          danger: true,
          run: function() { return performDeleteCred(fileName); }
        });
      }
      async function performDeleteCred(fileName) {
        const res = await adminFetch('/credentials/delete', {
          method: 'POST',
          headers: {
            'Content-Type': 'application/x-www-form-urlencoded'
          },
          body: new URLSearchParams({ file_name: fileName })
        });
        if (!res) return;
        const data = await res.json();
        notify(data.message || 'Credential deleted', data.ok === false ? 'error' : '');
        refreshCredentialViews();
      }
      function toggleCred(fileName, enabled, label) {
        closeAccountActionMenus();
        performToggleCred(fileName, enabled).catch(function(err) {
          notify(err && err.message ? err.message : 'Failed to update credential', 'error');
        });
      }
      async function performToggleCred(fileName, enabled) {
        const res = await adminFetch('/credentials/toggle', {
          method: 'POST',
          headers: {
            'Content-Type': 'application/x-www-form-urlencoded'
          },
          body: new URLSearchParams({ file_name: fileName, enabled: enabled ? 'true' : 'false' })
        });
        if (!res) return;
        const data = await res.json();
        notify(data.message || 'Credential updated', data.ok === false ? 'error' : '');
        refreshCredentialViews();
      }
      async function toggleAccountPriority(provider, accountKey, priority) {
        closeAccountActionMenus();
        const res = await adminFetch('/admin/account-routing/priority', {
          method: 'POST',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify({
            provider: provider,
            account: accountKey,
            priority: !!priority
          })
        });
        if (!res) return;
        const data = await res.json();
        if (!data.ok) {
          notify(data.message || 'Failed to update priority routing', 'error');
          return;
        }
        dashboardState.accountRouting = data.settings || { providers: {} };
        dashboardState.apiKeyAccounts = Array.isArray(data.accounts) ? data.accounts : dashboardState.apiKeyAccounts;
        notify(priority ? 'Account will be used first' : 'Account priority removed', 'success');
        refreshCredentialViews();
      }
      async function refreshModelCatalog(provider, fileName) {
        closeAccountActionMenus();
        var body = new URLSearchParams();
        if (provider) body.set('provider', provider);
        if (fileName) body.set('file_name', fileName);
        var target = provider ? (providerLabels[provider] || provider) : 'all providers';
        notify('Refreshing ' + target + ' models and quota...', '', 8000);
        try {
          const res = await adminFetch('/models/refresh', {
            method: 'POST',
            headers: {
              'Content-Type': 'application/x-www-form-urlencoded'
            },
            body: body
          });
          if (!res) return;
          const data = await res.json();
          notify(data.message || (target + ' models and quota refreshed'), data.ok === false ? 'error' : 'success', 5200);
          invalidateDashboardSnapshot();
          const snapshot = await refreshDashboardSnapshot(true);
          if (snapshot) renderDashboardSnapshot();
          refreshCustomModels();
        } catch (err) {
          notify('Refresh failed: ' + (err && err.message ? err.message : err), 'error', 7000);
        }
      }
      function redeemCodexReset(fileName, label, accountId, creditId, creditTitle) {
        var display = label || accountId || fileName || 'this Codex account';
        var resetLabel = creditTitle || 'usage limit reset';
        openCredentialActionConfirm({
          title: 'Redeem usage reset?',
          message: 'Use ' + resetLabel + ' for ' + display + '?',
          approveLabel: 'Redeem reset',
          run: function() {
            return performCodexReset(fileName, label, accountId, creditId);
          }
        });
      }
      async function performCodexReset(fileName, label, accountId, creditId) {
        var body = new URLSearchParams();
        if (fileName) body.set('file_name', fileName);
        if (label) body.set('label', label);
        if (accountId) body.set('account_id', accountId);
        if (creditId) body.set('credit_id', creditId);
        if (window.crypto && typeof window.crypto.randomUUID === 'function') {
          body.set('idempotency_key', window.crypto.randomUUID());
        }
        const res = await adminFetch('/codex/rate-limit-reset-credit/consume', {
          method: 'POST',
          headers: {
            'Content-Type': 'application/x-www-form-urlencoded'
          },
          body: body
        });
        if (!res) return;
        const data = await res.json();
        var tone = data.ok === false || data.outcome === 'no_credit' ? 'error' : '';
        notify(data.message || 'Reset request completed', tone);
        invalidateDashboardSnapshot();
        refreshQuota();
      }
      function renderDashboardSnapshot() {
        if (dashboardSnapshot && dashboardSnapshot.account_routing) {
          var routing = dashboardSnapshot.account_routing;
          dashboardState.accountRouting = routing.settings || { providers: {} };
          dashboardState.apiKeyAccounts = Array.isArray(routing.accounts) ? routing.accounts : dashboardState.apiKeyAccounts;
        }
        refreshQuota();
        refreshAgwQuota();
        refreshGeminiQuota();
        refreshQwenQuota();
        refreshDeepSeekQuota();
        refreshGrokQuota();
        refreshMiniMaxQuota();
        refreshCopilotQuota();
        refreshClaudeQuota();
        refreshGlmQuota();
      }
      async function startDashboard() {
        await refreshDashboardSnapshot(true);
        renderDashboardSnapshot();
        refreshContextChart();
        refreshCustomModels();
        if (dashboardIntervalsStarted) {
          return;
        }
        dashboardIntervalsStarted = true;
        setInterval(() => {
          if (!adminAuthenticated) return;
          refreshDashboardSnapshot(true).then(function(snapshot) {
            if (snapshot) renderDashboardSnapshot();
          });
        }, 10000);
        setInterval(() => { if (adminAuthenticated) refreshContextChart(); }, 60000);
      }
      document.getElementById('adminLoginForm').addEventListener('submit', async (e) => {
        e.preventDefault();
        const otp = document.getElementById('adminOtpInput').value.trim();
        if (!otp) {
          document.getElementById('adminLoginStatus').textContent = 'OTP is required.';
          return;
        }
        const res = await fetch('/admin/login', {
          method: 'POST',
          credentials: 'same-origin',
          headers: { 'Content-Type': 'application/x-www-form-urlencoded' },
          body: new URLSearchParams({ otp: otp })
        });
        const data = await res.json();
        if (!res.ok || !data.ok) {
          document.getElementById('adminLoginStatus').textContent = data.message || 'Login failed.';
          return;
        }
        adminAuthEpoch += 1;
        adminAuthenticated = true;
        document.getElementById('adminOtpInput').value = '';
        hideAdminLogin();
        startDashboard();
      });
      document.getElementById('logoutBtn').addEventListener('click', async () => {
        const res = await fetch('/admin/logout', { method: 'POST', credentials: 'same-origin' });
        if (res) {
          try { await res.json(); } catch (_) {}
        }
        showAdminLogin('Logged out.');
      });
      configureMobileNav();
      configureContextRangeControls();
      configureChartDisclosure();
      window.addEventListener('resize', scheduleProviderRowArrangement);
      applyProviderDashboardSettings();
      updateOverview();
      bootstrapAdmin();
    </script>
  </body>
</html>
"###;
    (
        StatusCode::OK,
        [
            ("Content-Type", "text/html"),
            ("Cache-Control", "no-store"),
            ("Pragma", "no-cache"),
        ],
        html,
    )
        .into_response()
}

async fn admin_session_route(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let enabled = admin_auth::is_enabled(&state.cfg.admin_auth);
    let configured = admin_auth::is_configured(&state.cfg.admin_auth);
    let authenticated = if enabled {
        let mut sessions = state.admin_sessions.lock().unwrap();
        admin_auth::validate_session(&headers, &mut sessions)
    } else {
        true
    };

    axum::Json(serde_json::json!({
        "enabled": enabled,
        "configured": configured,
        "authenticated": authenticated
    }))
}

async fn admin_login_route(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<admin_auth::LoginForm>,
) -> impl IntoResponse {
    let now = std::time::SystemTime::now();
    let client_key = admin_auth::login_client_key(&headers, state.cfg.trusted_proxy);
    if let Some(message) = {
        let mut attempts = state.admin_login_attempts.lock().unwrap();
        admin_auth::current_lockout_message(&mut attempts, &client_key, now)
    } {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            axum::Json(serde_json::json!({
                "ok": false,
                "message": message
            })),
        )
            .into_response();
    }

    match admin_auth::verify_login(&state.cfg.admin_auth, &form.otp, now) {
        Ok(()) => {
            let ttl_seconds = admin_auth::session_ttl_seconds(&state.cfg.admin_auth);
            {
                let mut attempts = state.admin_login_attempts.lock().unwrap();
                admin_auth::clear_login_attempts(&mut attempts, &client_key);
            }
            let session_id = {
                let mut sessions = state.admin_sessions.lock().unwrap();
                let session_id = admin_auth::create_session(&mut sessions, ttl_seconds);
                admin_auth::save_sessions(&admin_session_path(state.cfg.as_ref()), &sessions);
                session_id
            };
            let mut response = axum::Json(serde_json::json!({
                "ok": true,
                "message": "logged in"
            }))
            .into_response();
            admin_auth::append_set_cookie(
                response.headers_mut(),
                &admin_auth::build_session_cookie(
                    &session_id,
                    ttl_seconds,
                    state.cfg.admin_auth.secure_cookies,
                ),
            );
            response
        }
        Err(err) => {
            let lockout_message = {
                let mut attempts = state.admin_login_attempts.lock().unwrap();
                admin_auth::record_failed_login(&mut attempts, &client_key, now)
            };
            let status = if lockout_message.is_some() {
                StatusCode::TOO_MANY_REQUESTS
            } else {
                StatusCode::UNAUTHORIZED
            };
            (
                status,
                axum::Json(serde_json::json!({
                    "ok": false,
                    "message": lockout_message.unwrap_or(err)
                })),
            )
                .into_response()
        }
    }
}

async fn admin_logout_route(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> impl IntoResponse {
    {
        let mut sessions = state.admin_sessions.lock().unwrap();
        admin_auth::remove_session(&headers, &mut sessions);
        admin_auth::save_sessions(&admin_session_path(state.cfg.as_ref()), &sessions);
    }
    let mut response = axum::Json(serde_json::json!({
        "ok": true,
        "message": "logged out"
    }))
    .into_response();
    admin_auth::append_set_cookie(
        response.headers_mut(),
        &admin_auth::clear_session_cookie(state.cfg.admin_auth.secure_cookies),
    );
    response
}

async fn admin_api_keys_route(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Some(response) = require_admin_session_json(&state, &headers) {
        return response;
    }
    let keys = {
        let store = state.api_keys.lock().unwrap();
        api_keys::public_records(&store)
    };
    axum::Json(serde_json::json!({
        "ok": true,
        "keys": keys,
        "accounts": notification_account_options(&state)
    }))
    .into_response()
}

async fn admin_api_keys_create_route(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<ApiKeyCreateRequest>,
) -> impl IntoResponse {
    if let Some(response) = require_admin_session_json(&state, &headers) {
        return response;
    }
    let label = payload.label.as_deref().unwrap_or("");
    let now = now_rfc3339();
    let (created, snapshot) = {
        let mut store = state.api_keys.lock().unwrap();
        match api_keys::create_key(&mut store, label, &payload.access, &now) {
            Ok(created) => (created, store.clone()),
            Err(err) => {
                return (
                    StatusCode::BAD_REQUEST,
                    axum::Json(serde_json::json!({
                        "ok": false,
                        "message": err
                    })),
                )
                    .into_response();
            }
        }
    };
    if let Err(err) = api_keys::save(state.cfg.as_ref(), &snapshot) {
        error!("failed to save API key store: {}", err);
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(serde_json::json!({
                "ok": false,
                "message": "failed to save API key"
            })),
        )
            .into_response();
    }
    state.api_key_cache.write().unwrap().clear();
    axum::Json(serde_json::json!({
        "ok": true,
        "key": created.key,
        "plain_text_key": created.plain_text_key,
        "keys": api_keys::public_records(&snapshot),
        "accounts": notification_account_options(&state)
    }))
    .into_response()
}

async fn admin_api_keys_access_route(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<ApiKeyAccessUpdateRequest>,
) -> impl IntoResponse {
    if let Some(response) = require_admin_session_json(&state, &headers) {
        return response;
    }
    let snapshot = {
        let mut store = state.api_keys.lock().unwrap();
        match api_keys::update_access(&mut store, payload.id.trim(), &payload.access) {
            Ok(_) => store.clone(),
            Err(err) => {
                return (
                    StatusCode::BAD_REQUEST,
                    axum::Json(serde_json::json!({
                        "ok": false,
                        "message": err
                    })),
                )
                    .into_response();
            }
        }
    };
    if let Err(err) = api_keys::save(state.cfg.as_ref(), &snapshot) {
        error!("failed to save API key access: {}", err);
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(serde_json::json!({
                "ok": false,
                "message": "failed to save API key access"
            })),
        )
            .into_response();
    }
    state.api_key_cache.write().unwrap().clear();
    axum::Json(serde_json::json!({
        "ok": true,
        "keys": api_keys::public_records(&snapshot),
        "accounts": notification_account_options(&state)
    }))
    .into_response()
}

async fn admin_api_keys_revoke_route(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<ApiKeyRevokeRequest>,
) -> impl IntoResponse {
    if let Some(response) = require_admin_session_json(&state, &headers) {
        return response;
    }
    let now = now_rfc3339();
    let snapshot = {
        let mut store = state.api_keys.lock().unwrap();
        match api_keys::revoke_key(&mut store, payload.id.trim(), &now) {
            Ok(_) => {
                state.api_key_cache.write().unwrap().clear();
                store.clone()
            }
            Err(err) => {
                return (
                    StatusCode::NOT_FOUND,
                    axum::Json(serde_json::json!({
                        "ok": false,
                        "message": err
                    })),
                )
                    .into_response();
            }
        }
    };
    if let Err(err) = api_keys::save(state.cfg.as_ref(), &snapshot) {
        error!("failed to save API key store: {}", err);
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(serde_json::json!({
                "ok": false,
                "message": "failed to save API key changes"
            })),
        )
            .into_response();
    }
    axum::Json(serde_json::json!({
        "ok": true,
        "keys": api_keys::public_records(&snapshot),
        "accounts": notification_account_options(&state)
    }))
    .into_response()
}

async fn admin_account_routing_route(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response {
    if let Some(response) = require_admin_session_json(&state, &headers) {
        return response;
    }
    prune_account_routing_settings(&state);
    axum::Json(serde_json::json!({
        "ok": true,
        "settings": account_routing_public_json(&state),
        "accounts": notification_account_options(&state)
    }))
    .into_response()
}

async fn admin_account_routing_priority_route(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<AccountRoutingPriorityRequest>,
) -> Response {
    if let Some(response) = require_admin_session_json(&state, &headers) {
        return response;
    }
    let provider = match normalize_routing_provider(&payload.provider) {
        Some(provider) => provider,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                axum::Json(serde_json::json!({
                    "ok": false,
                    "message": "unknown provider"
                })),
            )
                .into_response();
        }
    };
    let account = payload.account.trim().to_string();
    if account.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            axum::Json(serde_json::json!({
                "ok": false,
                "message": "account is required"
            })),
        )
            .into_response();
    }

    if payload.priority {
        let enabled_accounts = enabled_routing_account_keys(&state);
        let allowed = enabled_accounts
            .get(provider)
            .map(|accounts| accounts.contains(&account))
            .unwrap_or(false);
        if !allowed {
            return (
                StatusCode::BAD_REQUEST,
                axum::Json(serde_json::json!({
                    "ok": false,
                    "message": "priority account must be enabled"
                })),
            )
                .into_response();
        }
    }

    {
        let mut settings = state.account_routing.lock().unwrap();
        let entry = settings.providers.entry(provider.to_string()).or_default();
        if payload.priority {
            if !entry
                .priority_accounts
                .iter()
                .any(|value| value == &account)
            {
                entry.priority_accounts.push(account);
            }
        } else {
            entry.priority_accounts.retain(|value| value != &account);
        }
        normalize_account_routing_settings(&mut settings);
    }
    prune_account_routing_settings(&state);
    if let Err(err) =
        save_account_routing_settings(&state.cfg, &state.account_routing.lock().unwrap())
    {
        error!("failed to save account routing settings: {}", err);
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(serde_json::json!({
                "ok": false,
                "message": "failed to save account routing settings"
            })),
        )
            .into_response();
    }

    axum::Json(serde_json::json!({
        "ok": true,
        "settings": account_routing_public_json(&state),
        "accounts": notification_account_options(&state)
    }))
    .into_response()
}

/// Returns the dashboard counters and per-account request totals.
#[utoipa::path(
    get,
    path = "/dashboard.json",
    responses((
        status = 200,
        description = "Dashboard JSON snapshot",
        body = crate::source::openapi::DashboardJsonResponse
    ))
)]
async fn dashboard_json(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(response) = require_admin_session_json(&state, &headers) {
        return response;
    }
    let snapshot = {
        let stats = state.stats.lock().unwrap();
        stats.clone()
    };
    let accounts: Vec<serde_json::Value> = snapshot
        .codex_accounts
        .into_iter()
        .enumerate()
        .map(|(i, a)| {
            let file_name = {
                let tokens = state.tokens.lock().unwrap();
                tokens.get(i).and_then(|t| t.file_name.clone())
            };
            let enabled = {
                let tokens = state.tokens.lock().unwrap();
                tokens.get(i).map(|t| t.enabled).unwrap_or(false)
            };
            let expired_at = {
                let tokens = state.tokens.lock().unwrap();
                tokens.get(i).and_then(|t| t.expired_at.clone())
            };
            let runtime = {
                let tokens = state.tokens.lock().unwrap();
                tokens
                    .get(i)
                    .map(|token| {
                        router_account_runtime_json(
                            &state,
                            "codex",
                            &codex_stats_key(token),
                            token.enabled,
                        )
                    })
                    .unwrap_or_else(|| {
                        serde_json::json!({
                            "status": "disabled",
                            "cooldown_remaining_seconds": null,
                            "active_requests": 0,
                            "consecutive_failures": 0
                        })
                    })
            };
            serde_json::json!({
                "label": a.label,
                "account_id": a.account_id,
                "requests": a.requests,
                "errors": a.errors,
                "file_name": file_name,
                "enabled": enabled,
                "expired_at": expired_at,
                "runtime": runtime,
                "last_success_at": a.last_success_at,
                "last_error_at": a.last_error_at,
                "last_error_message": a.last_error_message
            })
        })
        .collect();
    axum::Json(serde_json::json!({
        "total_requests": snapshot.total_requests,
        "total_errors": snapshot.total_errors,
        "total_prompt_total": snapshot.total_prompt_total,
        "total_prompt_error_total": snapshot.total_prompt_error_total,
        "total_input_tokens": snapshot.total_input_tokens,
        "total_output_tokens": snapshot.total_output_tokens,
        "total_tokens_used": snapshot.total_tokens_used,
        "total_cache_tokens": snapshot.total_cache_tokens,
        "total_reasoning_tokens": snapshot.total_reasoning_tokens,
        "first_recorded_at": snapshot.first_recorded_at,
        "last_recorded_at": snapshot.last_recorded_at,
        "accounts": accounts
    }))
    .into_response()
}

async fn dashboard_snapshot_route(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(response) = require_admin_session_json(&state, &headers) {
        return response;
    }

    let dashboard = async {
        response_json_value(dashboard_json(State(state.clone()), headers.clone()).await).await
    };
    let antigravity = async {
        response_json_value(
            target::antigravity::admin::accounts_json(State(state.clone()))
                .await
                .into_response(),
        )
        .await
    };
    let gemini = async {
        response_json_value(
            target::gemini::admin::accounts_json(State(state.clone()))
                .await
                .into_response(),
        )
        .await
    };
    let qwen = async {
        response_json_value(
            target::qwen::admin::accounts_json(State(state.clone()))
                .await
                .into_response(),
        )
        .await
    };
    let deepseek = async {
        response_json_value(
            target::deepseek::admin::accounts_json(State(state.clone()))
                .await
                .into_response(),
        )
        .await
    };
    let minimax = async {
        response_json_value(
            target::minimax::admin::accounts_json(State(state.clone()))
                .await
                .into_response(),
        )
        .await
    };
    let grok = async {
        response_json_value(
            target::grok::admin::accounts_json(State(state.clone()))
                .await
                .into_response(),
        )
        .await
    };
    let copilot = async {
        response_json_value(
            target::copilot::admin::accounts_json(State(state.clone()))
                .await
                .into_response(),
        )
        .await
    };
    let claude = async {
        response_json_value(
            target::claude::admin::accounts_json(State(state.clone()))
                .await
                .into_response(),
        )
        .await
    };
    let glm = async {
        response_json_value(
            target::glm::admin::accounts_json(State(state.clone()))
                .await
                .into_response(),
        )
        .await
    };
    let (dashboard, antigravity, gemini, qwen, deepseek, minimax, grok, copilot, claude, glm) = tokio::join!(
        dashboard,
        antigravity,
        gemini,
        qwen,
        deepseek,
        minimax,
        grok,
        copilot,
        claude,
        glm,
    );

    let quota = |provider: &str| {
        serde_json::json!({
            "accounts": quota_snapshot(&state, provider)
        })
    };
    axum::Json(serde_json::json!({
        "generated_at": now_rfc3339(),
        "dashboard": dashboard,
        "providers": {
            "agw": antigravity,
            "gemini": gemini,
            "qwen": qwen,
            "deepseek": deepseek,
            "minimax": minimax,
            "grok": grok,
            "copilot": copilot,
            "claude": claude,
            "glm": glm
        },
        "quotas": {
            "codex": quota("codex"),
            "agw": quota("antigravity"),
            "gemini": quota("gemini"),
            "qwen": quota("qwen"),
            "deepseek": quota("deepseek"),
            "minimax": quota("minimax"),
            "grok": target::grok::admin::cached_quota_json(&state),
            "copilot": quota("copilot"),
            "claude": quota("claude"),
            "glm": quota("glm")
        },
        "account_routing": {
            "ok": true,
            "settings": account_routing_public_json(&state),
            "accounts": notification_account_options(&state)
        }
    }))
    .into_response()
}

async fn response_json_value(response: Response) -> serde_json::Value {
    let status = response.status();
    match axum::body::to_bytes(response.into_body(), 16 * 1024 * 1024).await {
        Ok(body) => serde_json::from_slice(&body).unwrap_or_else(|_| {
            serde_json::json!({
                "error": format!("dashboard source returned non-JSON status {}", status)
            })
        }),
        Err(err) => serde_json::json!({
            "error": format!("dashboard source body failed: {}", err)
        }),
    }
}

async fn admin_test_api_route(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<AdminTestApiRequest>,
) -> Response {
    if let Some(response) = require_admin_session_json(&state, &headers) {
        return response;
    }

    let model = payload.model.trim().to_string();
    if model.is_empty() {
        return admin_test_json_response(
            StatusCode::BAD_REQUEST,
            serde_json::json!({
                "ok": false,
                "status": StatusCode::BAD_REQUEST.as_u16(),
                "message": "model is required"
            }),
        );
    }

    let prompt = payload.prompt.trim().to_string();
    if prompt.is_empty() {
        return admin_test_json_response(
            StatusCode::BAD_REQUEST,
            serde_json::json!({
                "ok": false,
                "status": StatusCode::BAD_REQUEST.as_u16(),
                "model": model,
                "message": "prompt is required"
            }),
        );
    }

    let mut request_body = serde_json::json!({
        "model": model,
        "input": prompt,
        "stream": false,
        "store": false,
        "max_output_tokens": payload.max_output_tokens.unwrap_or(512).clamp(1, 8192)
    });
    if let Some(instructions) = payload
        .instructions
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        request_body["instructions"] = serde_json::Value::String(instructions.to_string());
    }

    let body = Bytes::from(serde_json::to_vec(&request_body).unwrap_or_default());
    let mut model_headers = internal_proxy_api_headers(&state);
    model_headers.insert(
        axum::http::header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    let uri: axum::http::Uri = "/v1/responses"
        .parse()
        .expect("static test API route URI is valid");
    let routed = match route_request("/v1/responses", &uri, &Method::POST, &model_headers, body) {
        Ok(routed) => routed,
        Err(err) => {
            return admin_test_json_response(
                err.status,
                serde_json::json!({
                    "ok": false,
                    "status": err.status.as_u16(),
                    "model": payload.model.trim(),
                    "message": err.message
                }),
            );
        }
    };

    let started_at = std::time::Instant::now();
    let response = dispatch_admin_test_routed_request(state.clone(), model_headers, routed).await;
    admin_test_response_from_upstream(state, payload.model.trim(), response, started_at.elapsed())
        .await
}

async fn dispatch_admin_test_routed_request(
    state: AppState,
    headers: HeaderMap,
    routed: RoutedRequest,
) -> Response {
    match routed.target {
        TargetModel::Custom => {
            let prompt = serde_json::from_slice::<serde_json::Value>(&routed.upstream_body)
                .ok()
                .map(|value| prompt_metrics_from_request_value(&value))
                .unwrap_or_default();
            custom_model_response(
                state,
                headers,
                &routed.upstream_path,
                routed.upstream_body,
                routed.response_mode,
                &api_keys::ApiKeyAccess::default(),
                prompt,
            )
            .await
        }
        TargetModel::CodexModels | TargetModel::UnifiedV1Models => (
            StatusCode::BAD_REQUEST,
            [("Content-Type", "application/json")],
            openai_error_body(
                "test API requires a response model",
                "invalid_request_error",
                None,
            ),
        )
            .into_response(),
        target => {
            dispatch_custom_target(
                state,
                headers,
                &routed.upstream_path,
                target,
                routed.upstream_body,
                routed.response_mode,
            )
            .await
        }
    }
}

async fn admin_test_response_from_upstream(
    _state: AppState,
    requested_model: &str,
    response: Response,
    elapsed: Duration,
) -> Response {
    let status = response.status();
    let content_type = response
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let body = match axum::body::to_bytes(response.into_body(), 16 * 1024 * 1024).await {
        Ok(body) => body,
        Err(err) => {
            return admin_test_json_response(
                StatusCode::BAD_GATEWAY,
                serde_json::json!({
                    "ok": false,
                    "status": StatusCode::BAD_GATEWAY.as_u16(),
                    "model": requested_model,
                    "duration_ms": elapsed.as_millis(),
                    "message": format!("failed to read upstream response body: {}", err)
                }),
            );
        }
    };

    let json_value = serde_json::from_slice::<serde_json::Value>(&body).ok();
    let body_text = String::from_utf8_lossy(&body).trim().to_string();
    let output_text = json_value
        .as_ref()
        .and_then(extract_response_output_text)
        .unwrap_or_default();
    let message = if status.is_success() {
        if output_text.is_empty() {
            "model returned a response without text output".to_string()
        } else {
            "model test completed".to_string()
        }
    } else {
        json_value
            .as_ref()
            .and_then(response_error_message)
            .unwrap_or_else(|| {
                if body_text.is_empty() {
                    format!("upstream returned {}", status)
                } else {
                    body_text.clone()
                }
            })
    };
    let response_value = json_value.unwrap_or_else(|| {
        serde_json::json!({
            "body": body_text
        })
    });

    admin_test_json_response(
        status,
        serde_json::json!({
            "ok": status.is_success(),
            "status": status.as_u16(),
            "model": requested_model,
            "duration_ms": elapsed.as_millis(),
            "content_type": content_type,
            "output_text": output_text,
            "message": message,
            "response": response_value
        }),
    )
}

fn admin_test_json_response(status: StatusCode, value: serde_json::Value) -> Response {
    (
        status,
        [("Content-Type", "application/json")],
        serde_json::to_vec(&value).unwrap_or_default(),
    )
        .into_response()
}

fn extract_response_output_text(value: &serde_json::Value) -> Option<String> {
    if let Some(text) = value
        .get("output_text")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return Some(text.to_string());
    }

    let mut parts = Vec::new();
    if let Some(output) = value.get("output").and_then(|value| value.as_array()) {
        for item in output {
            if let Some(content) = item.get("content") {
                collect_response_text_parts(content, &mut parts);
            } else {
                collect_response_text_parts(item, &mut parts);
            }
        }
    }
    if parts.is_empty() {
        if let Some(choices) = value.get("choices").and_then(|value| value.as_array()) {
            for choice in choices {
                if let Some(message) = choice.get("message") {
                    if let Some(content) = message.get("content") {
                        collect_response_text_parts(content, &mut parts);
                    }
                }
                if let Some(delta) = choice.get("delta") {
                    if let Some(content) = delta.get("content") {
                        collect_response_text_parts(content, &mut parts);
                    }
                }
            }
        }
    }
    let text = parts
        .into_iter()
        .map(|part| part.trim().to_string())
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

fn collect_response_text_parts(value: &serde_json::Value, out: &mut Vec<String>) {
    match value {
        serde_json::Value::String(text) => {
            if !text.trim().is_empty() {
                out.push(text.to_string());
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                collect_response_text_parts(item, out);
            }
        }
        serde_json::Value::Object(object) => {
            if object
                .get("type")
                .and_then(|value| value.as_str())
                .is_some_and(|kind| kind == "refusal")
            {
                return;
            }
            if let Some(text) = object
                .get("text")
                .or_else(|| object.get("output_text"))
                .and_then(|value| value.as_str())
            {
                if !text.trim().is_empty() {
                    out.push(text.to_string());
                }
                return;
            }
            if let Some(content) = object.get("content") {
                collect_response_text_parts(content, out);
            }
        }
        _ => {}
    }
}

fn response_error_message(value: &serde_json::Value) -> Option<String> {
    let error = value.get("error");
    error
        .and_then(|error| error.get("message"))
        .or_else(|| error.and_then(|error| error.get("error_description")))
        .or_else(|| value.get("message"))
        .or_else(|| value.get("error_description"))
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| value.to_string())
        .or_else(|| {
            error
                .and_then(|value| value.as_str())
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(|value| value.to_string())
        })
}

/// Returns cached quota usage for each configured Codex credential.
#[utoipa::path(
    get,
    path = "/quota.json",
    responses((
        status = 200,
        description = "Quota summary",
        body = crate::source::openapi::QuotaResponse
    ))
)]
async fn quota_json_route(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(response) = require_admin_session_json(&state, &headers) {
        return response;
    }
    let accounts = quota_snapshot(&state, "codex");
    quota_accounts_json_response(&state, "codex", "Codex", accounts)
}

async fn codex_rate_limit_reset_consume_route(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<target::codex::quota::ConsumeRateLimitResetForm>,
) -> Response {
    if let Some(response) = require_admin_session_json(&state, &headers) {
        return response;
    }
    let fallback_idempotency_key = Uuid::new_v4().to_string();
    match target::codex::quota::consume_rate_limit_reset_credit(
        &state,
        form,
        fallback_idempotency_key,
    )
    .await
    {
        Ok(value) => axum::Json(value).into_response(),
        Err(err) => axum::Json(serde_json::json!({
            "ok": false,
            "message": err
        }))
        .into_response(),
    }
}

fn quota_accounts_json_response(
    state: &AppState,
    provider: &str,
    provider_label: &str,
    accounts: Vec<serde_json::Value>,
) -> Response {
    notifications::notify_model_quota_transitions(
        state,
        provider,
        provider_label,
        &accounts,
        notification_account_options(state),
        &now_rfc3339(),
    );
    axum::Json(serde_json::json!({ "accounts": accounts })).into_response()
}

async fn custom_models_json_route(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(response) = require_admin_session_json(&state, &headers) {
        return response;
    }
    let models = state.custom_models.lock().unwrap().clone();
    let model_headers = internal_proxy_api_headers(&state);
    let model_catalog = collect_unified_v1_models(&state, &model_headers).await;
    let model_options = custom_model_options_from_catalog(model_catalog.clone());
    let test_model_options = test_api_model_options_from_catalog(model_catalog);
    let data = models
        .into_iter()
        .map(|model| {
            serde_json::json!({
                "id": custom_models::public_model_id(&model.alias),
                "alias": model.alias,
                "display_name": model.display_name,
                "enabled": model.enabled,
                "routes": model.routes.clone(),
                "route_group_count": custom_models::route_group_count(&model),
                "target_count": custom_models::target_count(&model)
            })
        })
        .collect::<Vec<_>>();
    axum::Json(serde_json::json!({
        "models": data,
        "model_options": model_options,
        "test_model_options": test_model_options,
        "accounts": notification_account_options(&state),
        "path": custom_models::custom_models_path(&state.cfg)
    }))
    .into_response()
}

async fn model_catalog_refresh_route(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<ModelCatalogRefreshForm>,
) -> Response {
    if let Some(response) = require_admin_session_json(&state, &headers) {
        return response;
    }

    let provider = match form
        .provider
        .as_deref()
        .map(normalize_model_catalog_provider)
    {
        Some(Some(provider)) => Some(provider),
        Some(None) => {
            return (
                StatusCode::BAD_REQUEST,
                [("Content-Type", "application/json")],
                openai_error_body("Unknown model provider", "invalid_request_error", None),
            )
                .into_response();
        }
        None => None,
    };

    if form
        .file_name
        .as_deref()
        .is_some_and(|value| !is_safe_credential_file_name(value))
    {
        return (
            StatusCode::BAD_REQUEST,
            [("Content-Type", "application/json")],
            openai_error_body(
                "Invalid credential file name",
                "invalid_request_error",
                None,
            ),
        )
            .into_response();
    }

    let model_headers = internal_proxy_api_headers(&state);
    let outcome = refresh_unified_model_cache_now(&state, &model_headers, provider).await;
    let quota_count =
        refresh_quota_snapshots_now(&state, provider, form.file_name.as_deref()).await;
    let provider_label = provider.unwrap_or("all");
    let provider_display_label = model_catalog_provider_display_label(provider);
    let message = if outcome.started {
        format!("Models and quota refreshed for {}", provider_display_label)
    } else {
        format!(
            "Model catalog refresh already running; quota refreshed for {}",
            provider_display_label
        )
    };

    axum::Json(serde_json::json!({
        "ok": true,
        "provider": provider_label,
        "model_count": outcome.models.len(),
        "quota_account_count": quota_count,
        "message": message
    }))
    .into_response()
}

fn model_catalog_provider_display_label(provider: Option<&str>) -> &'static str {
    match provider {
        Some("cod") => "Codex",
        Some("agw") => "Antigravity",
        Some("gem") => "Gemini",
        Some("qwn") => "Qwen",
        Some("dsk") => "DeepSeek",
        Some("min") => "MiniMax",
        Some("grk") => "Grok",
        Some("cop") => "GitHub Copilot",
        Some("cld") => "Claude",
        Some("glm") => "GLM (Z.AI)",
        Some(_) => "selected provider",
        None => "all providers",
    }
}

fn internal_proxy_api_headers(state: &AppState) -> HeaderMap {
    let mut headers = HeaderMap::new();
    if let Ok(value) = HeaderValue::from_str(state.internal_proxy_secret.as_str()) {
        headers.insert("x-internal-proxy-key", value);
    }
    headers
}

fn custom_model_options_from_catalog(models: Vec<serde_json::Value>) -> Vec<serde_json::Value> {
    let mut out = models
        .into_iter()
        .filter_map(|model| {
            let provider = model
                .get("provider_prefix")
                .and_then(|value| value.as_str())
                .unwrap_or_default()
                .trim()
                .to_string();
            if provider.is_empty() || provider == "ctm" {
                return None;
            }
            let id = model
                .get("id")
                .and_then(|value| value.as_str())
                .unwrap_or_default()
                .trim()
                .to_string();
            let upstream_model = model
                .get("upstream_model")
                .and_then(|value| value.as_str())
                .unwrap_or(id.as_str())
                .trim()
                .to_string();
            if upstream_model.is_empty() {
                return None;
            }
            let display_name = model
                .get("display_name")
                .or_else(|| model.get("name"))
                .and_then(|value| value.as_str())
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .unwrap_or(upstream_model.as_str())
                .to_string();
            Some(serde_json::json!({
                "provider": provider,
                "model": upstream_model,
                "id": id,
                "display_name": display_name
            }))
        })
        .collect::<Vec<_>>();
    out.sort_by(|left, right| {
        left.get("provider")
            .and_then(|value| value.as_str())
            .cmp(&right.get("provider").and_then(|value| value.as_str()))
            .then_with(|| {
                left.get("model")
                    .and_then(|value| value.as_str())
                    .cmp(&right.get("model").and_then(|value| value.as_str()))
            })
    });
    out
}

fn test_api_model_options_from_catalog(models: Vec<serde_json::Value>) -> Vec<serde_json::Value> {
    let mut seen = HashSet::new();
    let mut out = models
        .into_iter()
        .filter_map(|model| {
            let id = model
                .get("id")
                .and_then(|value| value.as_str())
                .unwrap_or_default()
                .trim()
                .to_string();
            if id.is_empty() || !seen.insert(id.to_ascii_lowercase()) {
                return None;
            }
            let provider = model
                .get("provider_prefix")
                .and_then(|value| value.as_str())
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .or_else(|| id.split_once(':').map(|(prefix, _)| prefix))
                .unwrap_or_default()
                .to_string();
            let upstream_model = model
                .get("upstream_model")
                .and_then(|value| value.as_str())
                .unwrap_or(id.as_str())
                .trim()
                .to_string();
            let display_name = model
                .get("display_name")
                .or_else(|| model.get("name"))
                .and_then(|value| value.as_str())
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .unwrap_or(id.as_str())
                .to_string();
            Some(serde_json::json!({
                "provider": provider,
                "model": upstream_model,
                "id": id,
                "display_name": display_name
            }))
        })
        .collect::<Vec<_>>();
    out.sort_by(|left, right| {
        left.get("provider")
            .and_then(|value| value.as_str())
            .cmp(&right.get("provider").and_then(|value| value.as_str()))
            .then_with(|| {
                left.get("id")
                    .and_then(|value| value.as_str())
                    .cmp(&right.get("id").and_then(|value| value.as_str()))
            })
    });
    out
}

async fn custom_models_save_route(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Some(response) = require_admin_session_json(&state, &headers) {
        return response;
    }
    let (model, original_alias) = match parse_custom_model_save(&headers, &body) {
        Ok((model, original_alias)) => (custom_models::normalize_model(model), original_alias),
        Err(err) => {
            return (
                StatusCode::BAD_REQUEST,
                [("Content-Type", "application/json")],
                serde_json::to_vec(&serde_json::json!({
                    "ok": false,
                    "message": err
                }))
                .unwrap_or_default(),
            )
                .into_response();
        }
    };
    if let Err(err) = custom_models::validate_model(&model) {
        return (
            StatusCode::BAD_REQUEST,
            [("Content-Type", "application/json")],
            serde_json::to_vec(&serde_json::json!({
                "ok": false,
                "message": err
            }))
            .unwrap_or_default(),
        )
            .into_response();
    }

    let normalized_original_alias = original_alias
        .map(|alias| custom_models::normalize_alias(&alias))
        .filter(|alias| !alias.is_empty());
    let saved_models = {
        let mut models = state.custom_models.lock().unwrap();
        models.retain(|existing| {
            !existing.alias.eq_ignore_ascii_case(&model.alias)
                && normalized_original_alias
                    .as_ref()
                    .map(|alias| !existing.alias.eq_ignore_ascii_case(alias))
                    .unwrap_or(true)
        });
        models.push(model.clone());
        models.sort_by(|left, right| left.alias.cmp(&right.alias));
        models.clone()
    };
    if let Err(err) = custom_models::save(&state.cfg, &saved_models) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            [("Content-Type", "application/json")],
            serde_json::to_vec(&serde_json::json!({
                "ok": false,
                "message": err
            }))
            .unwrap_or_default(),
        )
            .into_response();
    }
    if let Some(original_alias) = normalized_original_alias {
        if !original_alias.eq_ignore_ascii_case(&model.alias) {
            state
                .custom_model_rr
                .lock()
                .unwrap()
                .remove(&original_alias);
        }
    }
    invalidate_model_caches().await;
    axum::Json(serde_json::json!({
        "ok": true,
        "message": "Custom model saved",
        "model": {
            "id": custom_models::public_model_id(&model.alias),
            "alias": model.alias
        }
    }))
    .into_response()
}

async fn custom_models_delete_route(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Some(response) = require_admin_session_json(&state, &headers) {
        return response;
    }
    let alias = match parse_alias_body(&headers, &body) {
        Some(alias) => custom_models::normalize_alias(&alias),
        None => {
            return (
                StatusCode::BAD_REQUEST,
                [("Content-Type", "application/json")],
                serde_json::to_vec(&serde_json::json!({
                    "ok": false,
                    "message": "alias is required"
                }))
                .unwrap_or_default(),
            )
                .into_response();
        }
    };
    let saved_models = {
        let mut models = state.custom_models.lock().unwrap();
        let before = models.len();
        models.retain(|model| !model.alias.eq_ignore_ascii_case(&alias));
        if models.len() == before {
            return (
                StatusCode::NOT_FOUND,
                [("Content-Type", "application/json")],
                serde_json::to_vec(&serde_json::json!({
                    "ok": false,
                    "message": "custom model not found"
                }))
                .unwrap_or_default(),
            )
                .into_response();
        }
        models.clone()
    };
    if let Err(err) = custom_models::save(&state.cfg, &saved_models) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            [("Content-Type", "application/json")],
            serde_json::to_vec(&serde_json::json!({
                "ok": false,
                "message": err
            }))
            .unwrap_or_default(),
        )
            .into_response();
    }
    state.custom_model_rr.lock().unwrap().remove(&alias);
    invalidate_model_caches().await;
    axum::Json(serde_json::json!({
        "ok": true,
        "message": "Custom model deleted"
    }))
    .into_response()
}

async fn notification_settings_route(
    State(state): State<AppState>,
    headers: HeaderMap,
    method: Method,
    body: Bytes,
) -> Response {
    if let Some(response) = require_admin_session_json(&state, &headers) {
        return response;
    }

    match method {
        Method::GET => notification_settings_json(&state),
        Method::POST => {
            let update: notifications::NotificationSettingsUpdate =
                match serde_json::from_slice(&body) {
                    Ok(update) => update,
                    Err(err) => {
                        return (
                            StatusCode::BAD_REQUEST,
                            [("Content-Type", "application/json")],
                            serde_json::to_vec(&serde_json::json!({
                                "ok": false,
                                "message": format!("invalid notification settings JSON: {}", err)
                            }))
                            .unwrap_or_default(),
                        )
                            .into_response();
                    }
                };
            let next = {
                let current = state.notification_settings.lock().unwrap().clone();
                notifications::apply_update(&current, update)
            };
            if let Err(err) = notifications::save(state.cfg.as_ref(), &next) {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    [("Content-Type", "application/json")],
                    serde_json::to_vec(&serde_json::json!({
                        "ok": false,
                        "message": err
                    }))
                    .unwrap_or_default(),
                )
                    .into_response();
            }
            {
                let mut lock = state.notification_settings.lock().unwrap();
                *lock = next;
            }
            notification_settings_json(&state)
        }
        _ => (
            StatusCode::METHOD_NOT_ALLOWED,
            [("Content-Type", "application/json")],
            serde_json::to_vec(&serde_json::json!({
                "ok": false,
                "message": "notification settings supports GET and POST"
            }))
            .unwrap_or_default(),
        )
            .into_response(),
    }
}

async fn notification_test_route(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(response) = require_admin_session_json(&state, &headers) {
        return response;
    }
    let settings = state.notification_settings.lock().unwrap().clone();
    let text = format!(
        "IO Gateway test notification\nTime: {}",
        notifications::display_datetime(&now_rfc3339())
    );
    match notifications::send_notification(&state.client, &settings, &text).await {
        Ok(()) => axum::Json(serde_json::json!({
            "ok": true,
            "message": "test notification sent"
        }))
        .into_response(),
        Err(err) => (
            StatusCode::BAD_REQUEST,
            [("Content-Type", "application/json")],
            serde_json::to_vec(&serde_json::json!({
                "ok": false,
                "message": err
            }))
            .unwrap_or_default(),
        )
            .into_response(),
    }
}

fn notification_settings_json(state: &AppState) -> Response {
    let settings = state.notification_settings.lock().unwrap().clone();
    let accounts = notification_account_options(state);
    axum::Json(serde_json::json!({
        "ok": true,
        "settings": notifications::public_json(&settings, accounts)
    }))
    .into_response()
}

fn notification_account_options(state: &AppState) -> Vec<notifications::NotificationAccountOption> {
    let mut out = Vec::new();

    for token in state.tokens.lock().unwrap().clone() {
        out.push(notifications::NotificationAccountOption {
            provider: "codex".to_string(),
            provider_label: "Codex".to_string(),
            key: codex_stats_key(&token),
            label: token.label.clone(),
            account_id: token.account_id.clone().unwrap_or_default(),
            credential_file: token.file_name.clone(),
            enabled: token.enabled,
        });
    }
    for account in state.agw_accounts.lock().unwrap().clone() {
        out.push(notifications::NotificationAccountOption {
            provider: "agw".to_string(),
            provider_label: "Antigravity".to_string(),
            key: antigravity_stats_key(&account),
            label: account.label.clone(),
            account_id: account.email.clone(),
            credential_file: account.file_name.clone(),
            enabled: account.enabled,
        });
    }
    for account in state.gemini_accounts.lock().unwrap().clone() {
        out.push(notifications::NotificationAccountOption {
            provider: "gemini".to_string(),
            provider_label: "Gemini".to_string(),
            key: gemini_stats_key(&account),
            label: account.label.clone(),
            account_id: account.email.clone(),
            credential_file: account.file_name.clone(),
            enabled: account.enabled,
        });
    }
    for account in state.qwen_accounts.lock().unwrap().clone() {
        out.push(notifications::NotificationAccountOption {
            provider: "qwen".to_string(),
            provider_label: "Qwen".to_string(),
            key: qwen_stats_key(&account),
            label: account.label.clone(),
            account_id: account.account_id.clone(),
            credential_file: account.file_name.clone(),
            enabled: account.enabled,
        });
    }
    for account in state.deepseek_accounts.lock().unwrap().clone() {
        out.push(notifications::NotificationAccountOption {
            provider: "deepseek".to_string(),
            provider_label: "DeepSeek".to_string(),
            key: deepseek_stats_key(&account),
            label: account.label.clone(),
            account_id: account.account_id.clone(),
            credential_file: account.file_name.clone(),
            enabled: account.enabled,
        });
    }
    for account in state.minimax_accounts.lock().unwrap().clone() {
        out.push(notifications::NotificationAccountOption {
            provider: "minimax".to_string(),
            provider_label: "MiniMax".to_string(),
            key: minimax_stats_key(&account),
            label: account.label.clone(),
            account_id: account.account_id.clone(),
            credential_file: account.file_name.clone(),
            enabled: account.enabled,
        });
    }
    for account in state.grok_accounts.lock().unwrap().clone() {
        out.push(notifications::NotificationAccountOption {
            provider: "grok".to_string(),
            provider_label: "Grok".to_string(),
            key: grok_stats_key(&account),
            label: account.label.clone(),
            account_id: account
                .user_id
                .clone()
                .or_else(|| account.email.clone())
                .unwrap_or_default(),
            credential_file: account.file_name.clone(),
            enabled: account.enabled,
        });
    }
    for account in state.copilot_accounts.lock().unwrap().clone() {
        out.push(notifications::NotificationAccountOption {
            provider: "copilot".to_string(),
            provider_label: "GitHub Copilot".to_string(),
            key: copilot_stats_key(&account),
            label: account.label.clone(),
            account_id: account.account_id.clone(),
            credential_file: account.file_name.clone(),
            enabled: account.enabled,
        });
    }
    for account in state.claude_accounts.lock().unwrap().clone() {
        out.push(notifications::NotificationAccountOption {
            provider: "claude".to_string(),
            provider_label: "Claude".to_string(),
            key: claude_stats_key(&account),
            label: account.label.clone(),
            account_id: account.account_id.clone(),
            credential_file: account.file_name.clone(),
            enabled: account.enabled,
        });
    }
    for account in state.glm_accounts.lock().unwrap().clone() {
        out.push(notifications::NotificationAccountOption {
            provider: "glm".to_string(),
            provider_label: "GLM (Z.AI)".to_string(),
            key: glm_stats_key(&account),
            label: account.label.clone(),
            account_id: account.account_id.clone(),
            credential_file: account.file_name.clone(),
            enabled: account.enabled,
        });
    }

    out.sort_by(|a, b| {
        a.provider
            .cmp(&b.provider)
            .then_with(|| a.label.cmp(&b.label))
            .then_with(|| a.key.cmp(&b.key))
    });
    out
}

fn account_routing_settings_path(cfg: &Config) -> PathBuf {
    cfg.auth_dir
        .as_deref()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("account-routing.json")
}

fn load_account_routing_settings(cfg: &Config) -> AccountRoutingSettings {
    let path = account_routing_settings_path(cfg);
    let Ok(data) = std::fs::read_to_string(path) else {
        return AccountRoutingSettings::default();
    };
    serde_json::from_str::<AccountRoutingSettings>(&data)
        .map(|mut settings| {
            normalize_account_routing_settings(&mut settings);
            settings
        })
        .unwrap_or_default()
}

fn save_account_routing_settings(
    cfg: &Config,
    settings: &AccountRoutingSettings,
) -> Result<(), String> {
    let mut settings = settings.clone();
    normalize_account_routing_settings(&mut settings);
    let path = account_routing_settings_path(cfg);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|err| format!("failed to create account routing dir: {}", err))?;
    }
    let data = serde_json::to_vec_pretty(&settings)
        .map_err(|err| format!("failed to serialize account routing settings: {}", err))?;
    std::fs::write(&path, data)
        .map_err(|err| format!("failed to write account routing settings: {}", err))
}

fn normalize_account_routing_settings(settings: &mut AccountRoutingSettings) {
    let mut normalized = HashMap::new();
    for (provider, mut provider_settings) in std::mem::take(&mut settings.providers) {
        let Some(provider) = normalize_routing_provider(&provider) else {
            continue;
        };
        let mut seen = HashSet::new();
        provider_settings.priority_accounts.retain(|account| {
            let account = account.trim();
            !account.is_empty() && seen.insert(account.to_string())
        });
        provider_settings
            .priority_accounts
            .iter_mut()
            .for_each(|account| *account = account.trim().to_string());
        if !provider_settings.priority_accounts.is_empty() {
            normalized.insert(provider.to_string(), provider_settings);
        }
    }
    settings.providers = normalized;
}

fn normalize_routing_provider(provider: &str) -> Option<&'static str> {
    match provider.trim().to_ascii_lowercase().as_str() {
        "cod" | "codex" => Some("codex"),
        "agw" | "antigravity" => Some("antigravity"),
        "gem" | "gemini" => Some("gemini"),
        "qwn" | "qwen" => Some("qwen"),
        "dsk" | "deepseek" => Some("deepseek"),
        "grk" | "grok" => Some("grok"),
        "min" | "minimax" => Some("minimax"),
        "cop" | "copilot" => Some("copilot"),
        "cld" | "claude" => Some("claude"),
        "glm" => Some("glm"),
        _ => None,
    }
}

fn account_routing_public_json(state: &AppState) -> serde_json::Value {
    let mut settings = state.account_routing.lock().unwrap().clone();
    normalize_account_routing_settings(&mut settings);
    serde_json::to_value(settings).unwrap_or_else(|_| serde_json::json!({ "providers": {} }))
}

pub(crate) fn routing_priority_accounts_for_provider(
    state: &AppState,
    provider: &str,
) -> HashSet<String> {
    let Some(provider) = normalize_routing_provider(provider) else {
        return HashSet::new();
    };
    state
        .account_routing
        .lock()
        .unwrap()
        .providers
        .get(provider)
        .map(|settings| settings.priority_accounts.iter().cloned().collect())
        .unwrap_or_default()
}

fn enabled_routing_account_keys(state: &AppState) -> HashMap<&'static str, HashSet<String>> {
    let mut out: HashMap<&'static str, HashSet<String>> = HashMap::new();
    let mut push = |provider: &'static str, key: String| {
        out.entry(provider).or_default().insert(key);
    };

    for token in state
        .tokens
        .lock()
        .unwrap()
        .iter()
        .filter(|token| token.enabled)
    {
        push("codex", codex_stats_key(token));
    }
    for account in state
        .agw_accounts
        .lock()
        .unwrap()
        .iter()
        .filter(|account| account.enabled)
    {
        push("antigravity", antigravity_stats_key(account));
    }
    for account in state
        .gemini_accounts
        .lock()
        .unwrap()
        .iter()
        .filter(|account| account.enabled)
    {
        push("gemini", gemini_stats_key(account));
    }
    for account in state
        .qwen_accounts
        .lock()
        .unwrap()
        .iter()
        .filter(|account| account.enabled)
    {
        push("qwen", qwen_stats_key(account));
    }
    for account in state
        .deepseek_accounts
        .lock()
        .unwrap()
        .iter()
        .filter(|account| account.enabled)
    {
        push("deepseek", deepseek_stats_key(account));
    }
    for account in state
        .grok_accounts
        .lock()
        .unwrap()
        .iter()
        .filter(|account| account.enabled)
    {
        push("grok", grok_stats_key(account));
    }
    for account in state
        .minimax_accounts
        .lock()
        .unwrap()
        .iter()
        .filter(|account| account.enabled)
    {
        push("minimax", minimax_stats_key(account));
    }
    for account in state
        .copilot_accounts
        .lock()
        .unwrap()
        .iter()
        .filter(|account| account.enabled)
    {
        push("copilot", copilot_stats_key(account));
    }
    for account in state
        .claude_accounts
        .lock()
        .unwrap()
        .iter()
        .filter(|account| account.enabled)
    {
        push("claude", claude_stats_key(account));
    }
    for account in state
        .glm_accounts
        .lock()
        .unwrap()
        .iter()
        .filter(|account| account.enabled)
    {
        push("glm", glm_stats_key(account));
    }

    out
}

fn prune_account_routing_settings(state: &AppState) {
    let enabled_accounts = enabled_routing_account_keys(state);
    let mut changed = false;
    {
        let mut settings = state.account_routing.lock().unwrap();
        normalize_account_routing_settings(&mut settings);
        settings.providers.retain(|provider, provider_settings| {
            let Some(enabled) = enabled_accounts.get(provider.as_str()) else {
                changed |= !provider_settings.priority_accounts.is_empty();
                return false;
            };
            let before = provider_settings.priority_accounts.len();
            provider_settings
                .priority_accounts
                .retain(|account| enabled.contains(account));
            changed |= provider_settings.priority_accounts.len() != before;
            !provider_settings.priority_accounts.is_empty()
        });
    }
    if changed {
        let settings = state.account_routing.lock().unwrap().clone();
        if let Err(err) = save_account_routing_settings(&state.cfg, &settings) {
            error!("failed to save pruned account routing settings: {}", err);
        }
    }
}

fn parse_custom_model_save(
    headers: &HeaderMap,
    body: &Bytes,
) -> Result<(custom_models::CustomModel, Option<String>), String> {
    if is_json_content(headers) {
        let value =
            serde_json::from_slice::<serde_json::Value>(body).map_err(|err| err.to_string())?;
        let original_alias = value
            .get("original_alias")
            .or_else(|| value.get("previous_alias"))
            .and_then(|value| value.as_str())
            .map(|value| value.to_string());
        return custom_model_from_json(&value).map(|model| (model, original_alias));
    }
    let form = serde_urlencoded::from_bytes::<HashMap<String, String>>(body)
        .map_err(|err| err.to_string())?;
    let alias = form.get("alias").cloned().unwrap_or_default();
    let original_alias = form
        .get("original_alias")
        .or_else(|| form.get("previous_alias"))
        .cloned();
    Ok((
        custom_models::CustomModel {
            alias,
            display_name: form.get("display_name").cloned(),
            enabled: form_bool(&form, "enabled", true),
            load_balance: true,
            routes: custom_models::parse_route_groups(
                form.get("routes")
                    .or_else(|| form.get("route_groups"))
                    .map(String::as_str)
                    .unwrap_or_default(),
            ),
            primary_models: custom_models::parse_model_list(
                form.get("primary_models")
                    .map(String::as_str)
                    .unwrap_or_default(),
            ),
            fallback_models: custom_models::parse_model_list(
                form.get("fallback_models")
                    .map(String::as_str)
                    .unwrap_or_default(),
            ),
        },
        original_alias,
    ))
}

fn custom_model_from_json(value: &serde_json::Value) -> Result<custom_models::CustomModel, String> {
    let alias = value
        .get("alias")
        .or_else(|| value.get("id"))
        .and_then(|value| value.as_str())
        .unwrap_or_default()
        .to_string();
    Ok(custom_models::CustomModel {
        alias,
        display_name: value
            .get("display_name")
            .and_then(|value| value.as_str())
            .map(|value| value.to_string()),
        enabled: value
            .get("enabled")
            .and_then(|value| value.as_bool())
            .unwrap_or(true),
        load_balance: value
            .get("load_balance")
            .and_then(|value| value.as_bool())
            .unwrap_or(true),
        routes: route_groups_from_json(value.get("routes").or_else(|| value.get("route_groups")))?,
        primary_models: targets_from_json(
            value.get("primary_models").or_else(|| value.get("models")),
        )?,
        fallback_models: targets_from_json(
            value
                .get("fallback_models")
                .or_else(|| value.get("fallbacks")),
        )?,
    })
}

fn route_groups_from_json(
    value: Option<&serde_json::Value>,
) -> Result<Vec<custom_models::CustomModelRouteGroup>, String> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    if let Some(text) = value.as_str() {
        return Ok(custom_models::parse_route_groups(text));
    }
    let Some(items) = value.as_array() else {
        return Err("routes must be a string or array".to_string());
    };
    let mut groups = Vec::new();
    for item in items {
        if let Some(text) = item.as_str() {
            groups.extend(custom_models::parse_route_groups(text));
            continue;
        }
        if let Some(items) = item.as_array() {
            groups.push(custom_models::CustomModelRouteGroup {
                targets: targets_from_json(Some(&serde_json::Value::Array(items.clone())))?,
            });
            continue;
        }
        let Some(object) = item.as_object() else {
            return Err("route entries must be strings, arrays, or objects".to_string());
        };
        let targets = targets_from_json(object.get("targets").or_else(|| object.get("models")))?;
        groups.push(custom_models::CustomModelRouteGroup { targets });
    }
    Ok(groups)
}

fn targets_from_json(
    value: Option<&serde_json::Value>,
) -> Result<Vec<custom_models::CustomModelTarget>, String> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    if let Some(text) = value.as_str() {
        return Ok(custom_models::parse_model_list(text));
    }
    let Some(items) = value.as_array() else {
        return Err("model lists must be strings or arrays".to_string());
    };
    let mut targets = Vec::new();
    for item in items {
        if let Some(model) = item.as_str() {
            targets.push(custom_models::CustomModelTarget {
                model: model.to_string(),
                account: None,
                account_condition: custom_models::CustomModelAccountCondition::Only,
                enabled: true,
                weight: 1,
            });
            continue;
        }
        let Some(object) = item.as_object() else {
            return Err("model list entries must be strings or objects".to_string());
        };
        targets.push(custom_models::CustomModelTarget {
            model: object
                .get("model")
                .and_then(|value| value.as_str())
                .unwrap_or_default()
                .to_string(),
            account: object
                .get("account")
                .or_else(|| object.get("account_key"))
                .and_then(|value| value.as_str())
                .map(|value| value.to_string()),
            account_condition: object
                .get("account_condition")
                .or_else(|| object.get("account_mode"))
                .or_else(|| object.get("condition"))
                .and_then(|value| value.as_str())
                .map(|value| {
                    if value.eq_ignore_ascii_case("except")
                        || value.eq_ignore_ascii_case("exclude")
                        || value.eq_ignore_ascii_case("without")
                    {
                        custom_models::CustomModelAccountCondition::Except
                    } else {
                        custom_models::CustomModelAccountCondition::Only
                    }
                })
                .unwrap_or_default(),
            enabled: object
                .get("enabled")
                .and_then(|value| value.as_bool())
                .unwrap_or(true),
            weight: object
                .get("weight")
                .and_then(|value| value.as_u64())
                .and_then(|value| u32::try_from(value).ok())
                .unwrap_or(1)
                .max(1),
        });
    }
    Ok(targets)
}

fn parse_alias_body(headers: &HeaderMap, body: &Bytes) -> Option<String> {
    if is_json_content(headers) {
        let value = serde_json::from_slice::<serde_json::Value>(body).ok()?;
        return value
            .get("alias")
            .or_else(|| value.get("id"))
            .and_then(|value| value.as_str())
            .map(|value| value.to_string());
    }
    serde_urlencoded::from_bytes::<HashMap<String, String>>(body)
        .ok()
        .and_then(|form| {
            form.get("alias")
                .cloned()
                .or_else(|| form.get("id").cloned())
        })
}

fn is_json_content(headers: &HeaderMap) -> bool {
    headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.to_ascii_lowercase().contains("application/json"))
        .unwrap_or(false)
}

fn form_bool(form: &HashMap<String, String>, key: &str, default: bool) -> bool {
    form.get(key)
        .map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(default)
}

/// Starts the Codex OAuth login flow and returns the upstream authorization URL.
#[utoipa::path(
    get,
    path = "/login/codex/start",
    responses((
        status = 200,
        description = "OAuth login URL and state token",
        body = crate::source::openapi::LoginStartResponse
    ))
)]
async fn login_start_route(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(response) = require_admin_session_json(&state, &headers) {
        return response;
    }
    target::codex::admin::login_start(State(state))
        .await
        .into_response()
}

/// Accepts the OAuth callback URL and stores the resulting Codex credentials.
#[utoipa::path(
    post,
    path = "/login/codex/submit",
    request_body(
        content = crate::source::openapi::LoginSubmitRequest,
        content_type = "application/x-www-form-urlencoded",
        description = "OAuth callback URL copied from the browser redirect"
    ),
    responses((
        status = 200,
        description = "Credential save result",
        body = crate::source::openapi::ActionResponse
    ))
)]
async fn login_submit_route(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<target::codex::admin::CallbackForm>,
) -> Response {
    if let Some(response) = require_admin_session_json(&state, &headers) {
        return response;
    }
    let response = target::codex::admin::login_submit(State(state), Form(form))
        .await
        .into_response();
    invalidate_model_caches().await;
    response
}

async fn agw_accounts_route(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(response) = require_admin_session_json(&state, &headers) {
        return response;
    }
    target::antigravity::admin::accounts_json(State(state))
        .await
        .into_response()
}

async fn agw_quota_json_route(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(response) = require_admin_session_json(&state, &headers) {
        return response;
    }
    let accounts = quota_snapshot(&state, "antigravity");
    quota_accounts_json_response(&state, "agw", "Antigravity", accounts)
}

async fn agw_login_start_route(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(response) = require_admin_session_json(&state, &headers) {
        return response;
    }
    target::antigravity::admin::login_start(State(state))
        .await
        .into_response()
}

async fn agw_login_submit_route(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<target::antigravity::admin::CallbackForm>,
) -> Response {
    if let Some(response) = require_admin_session_json(&state, &headers) {
        return response;
    }
    let response = target::antigravity::admin::login_submit(State(state), Form(form))
        .await
        .into_response();
    invalidate_model_caches().await;
    response
}

async fn gemini_accounts_route(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(response) = require_admin_session_json(&state, &headers) {
        return response;
    }
    target::gemini::admin::accounts_json(State(state))
        .await
        .into_response()
}

async fn gemini_quota_json_route(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(response) = require_admin_session_json(&state, &headers) {
        return response;
    }
    let accounts = quota_snapshot(&state, "gemini");
    quota_accounts_json_response(&state, "gemini", "Gemini", accounts)
}

async fn minimax_quota_json_route(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(response) = require_admin_session_json(&state, &headers) {
        return response;
    }
    let accounts = quota_snapshot(&state, "minimax");
    quota_accounts_json_response(&state, "minimax", "MiniMax", accounts)
}

async fn deepseek_quota_json_route(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(response) = require_admin_session_json(&state, &headers) {
        return response;
    }
    let accounts = quota_snapshot(&state, "deepseek");
    quota_accounts_json_response(&state, "deepseek", "DeepSeek", accounts)
}

async fn gemini_login_start_route(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(response) = require_admin_session_json(&state, &headers) {
        return response;
    }
    target::gemini::admin::login_start(State(state))
        .await
        .into_response()
}

async fn gemini_login_submit_route(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<target::gemini::admin::CallbackForm>,
) -> Response {
    if let Some(response) = require_admin_session_json(&state, &headers) {
        return response;
    }
    let response = target::gemini::admin::login_submit(State(state), Form(form))
        .await
        .into_response();
    invalidate_model_caches().await;
    response
}

async fn qwen_accounts_route(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(response) = require_admin_session_json(&state, &headers) {
        return response;
    }
    target::qwen::admin::accounts_json(State(state))
        .await
        .into_response()
}

async fn qwen_quota_json_route(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(response) = require_admin_session_json(&state, &headers) {
        return response;
    }
    let accounts = quota_snapshot(&state, "qwen");
    quota_accounts_json_response(&state, "qwen", "Qwen", accounts)
}

async fn qwen_login_start_route(
    State(state): State<AppState>,
    headers: HeaderMap,
    method: Method,
    body: Bytes,
) -> Response {
    if let Some(response) = require_admin_session_json(&state, &headers) {
        return response;
    }
    target::qwen::admin::login_start(State(state), method, body)
        .await
        .into_response()
}

async fn qwen_login_submit_route(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<target::qwen::admin::CallbackForm>,
) -> Response {
    if let Some(response) = require_admin_session_json(&state, &headers) {
        return response;
    }
    let response = target::qwen::admin::login_submit(State(state), headers, Form(form))
        .await
        .into_response();
    invalidate_model_caches().await;
    response
}

async fn qwen_login_status_route(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<target::qwen::admin::LoginStatusQuery>,
) -> Response {
    if let Some(response) = require_admin_session_json(&state, &headers) {
        return response;
    }
    target::qwen::admin::login_status(State(state), Query(query))
        .await
        .into_response()
}

async fn oauth_login_callback_route(
    Path(provider): Path<String>,
    State(state): State<AppState>,
    method: Method,
    headers: HeaderMap,
    OriginalUri(uri): OriginalUri,
) -> Response {
    if let Some(response) = require_admin_session_text(&state, &headers) {
        return response;
    }
    let response = target::oauth::login_callback_route(state, provider, method, headers, uri)
        .await
        .into_response();
    invalidate_model_caches().await;
    response
}

async fn deepseek_accounts_route(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(response) = require_admin_session_json(&state, &headers) {
        return response;
    }
    target::deepseek::admin::accounts_json(State(state))
        .await
        .into_response()
}

async fn minimax_accounts_route(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(response) = require_admin_session_json(&state, &headers) {
        return response;
    }
    target::minimax::admin::accounts_json(State(state))
        .await
        .into_response()
}

async fn minimax_login_start_route(
    State(state): State<AppState>,
    method: Method,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Some(response) = require_admin_session_json(&state, &headers) {
        return response;
    }
    let response = target::minimax::admin::login_start(State(state), method, body)
        .await
        .into_response();
    invalidate_model_caches().await;
    response
}

async fn copilot_accounts_route(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(response) = require_admin_session_json(&state, &headers) {
        return response;
    }
    target::copilot::admin::accounts_json(State(state))
        .await
        .into_response()
}

async fn copilot_quota_json_route(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(response) = require_admin_session_json(&state, &headers) {
        return response;
    }
    let accounts = quota_snapshot(&state, "copilot");
    quota_accounts_json_response(&state, "copilot", "GitHub Copilot", accounts)
}

async fn copilot_login_start_route(
    State(state): State<AppState>,
    headers: HeaderMap,
    method: Method,
    body: Bytes,
) -> Response {
    if let Some(response) = require_admin_session_json(&state, &headers) {
        return response;
    }
    target::copilot::admin::login_start(State(state), method, body)
        .await
        .into_response()
}

async fn copilot_login_submit_route(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<target::copilot::admin::LoginSubmitForm>,
) -> Response {
    if let Some(response) = require_admin_session_json(&state, &headers) {
        return response;
    }
    let response = target::copilot::admin::login_submit(State(state), Form(form))
        .await
        .into_response();
    invalidate_model_caches().await;
    response
}

async fn claude_accounts_route(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(response) = require_admin_session_json(&state, &headers) {
        return response;
    }
    target::claude::admin::accounts_json(State(state))
        .await
        .into_response()
}

async fn claude_quota_json_route(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(response) = require_admin_session_json(&state, &headers) {
        return response;
    }
    let accounts = quota_snapshot(&state, "claude");
    quota_accounts_json_response(&state, "claude", "Claude", accounts)
}

async fn claude_login_start_route(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<target::claude::admin::LoginStartQuery>,
    method: Method,
    body: Bytes,
) -> Response {
    if let Some(response) = require_admin_session_json(&state, &headers) {
        return response;
    }
    target::claude::admin::login_start(State(state), method, Query(query), body)
        .await
        .into_response()
}

async fn claude_login_submit_route(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<target::claude::admin::CallbackForm>,
) -> Response {
    if let Some(response) = require_admin_session_json(&state, &headers) {
        return response;
    }
    let response = target::claude::admin::login_submit(State(state), Form(form))
        .await
        .into_response();
    invalidate_model_caches().await;
    response
}

async fn glm_accounts_route(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(response) = require_admin_session_json(&state, &headers) {
        return response;
    }
    target::glm::admin::accounts_json(State(state))
        .await
        .into_response()
}

async fn glm_quota_json_route(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(response) = require_admin_session_json(&state, &headers) {
        return response;
    }
    let accounts = quota_snapshot(&state, "glm");
    quota_accounts_json_response(&state, "glm", "GLM (Z.AI)", accounts)
}

async fn glm_login_start_route(
    State(state): State<AppState>,
    headers: HeaderMap,
    method: Method,
    body: Bytes,
) -> Response {
    if let Some(response) = require_admin_session_json(&state, &headers) {
        return response;
    }
    let response = target::glm::admin::login_start(State(state), method, body)
        .await
        .into_response();
    invalidate_model_caches().await;
    response
}

async fn deepseek_login_start_route(
    State(state): State<AppState>,
    headers: HeaderMap,
    method: Method,
    body: Bytes,
) -> Response {
    if let Some(response) = require_admin_session_json(&state, &headers) {
        return response;
    }
    let response = target::deepseek::admin::login_start(State(state), method, body)
        .await
        .into_response();
    invalidate_model_caches().await;
    response
}

async fn grok_accounts_route(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(response) = require_admin_session_json(&state, &headers) {
        return response;
    }
    target::grok::admin::accounts_json(State(state))
        .await
        .into_response()
}

async fn grok_quota_route(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(response) = require_admin_session_json(&state, &headers) {
        return response;
    }
    axum::Json(target::grok::admin::cached_quota_json(&state)).into_response()
}

async fn grok_login_start_route(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(response) = require_admin_session_json(&state, &headers) {
        return response;
    }
    target::grok::admin::login_start(State(state))
        .await
        .into_response()
}

async fn grok_login_submit_route(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<target::grok::admin::CallbackForm>,
) -> Response {
    if let Some(response) = require_admin_session_json(&state, &headers) {
        return response;
    }
    let response = target::grok::admin::login_submit(State(state), Form(form))
        .await
        .into_response();
    invalidate_model_caches().await;
    response
}

async fn grok_login_status_route(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<target::grok::admin::LoginStatusQuery>,
) -> Response {
    if let Some(response) = require_admin_session_json(&state, &headers) {
        return response;
    }
    target::grok::admin::login_status(State(state), Query(query))
        .await
        .into_response()
}

async fn usage_summary_route(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(response) = require_admin_session_json(&state, &headers) {
        return response;
    }
    axum::Json(usage_summary_json(&state)).into_response()
}

fn usage_summary_json(state: &AppState) -> serde_json::Value {
    let persisted = state.persisted_stats.lock().unwrap().clone();
    let mut codex = persisted
        .codex
        .into_iter()
        .map(|(key, usage)| serde_json::json!({ "key": key, "usage": usage }))
        .collect::<Vec<_>>();
    let mut antigravity = persisted
        .antigravity
        .into_iter()
        .map(|(key, usage)| serde_json::json!({ "key": key, "usage": usage }))
        .collect::<Vec<_>>();
    let mut gemini = persisted
        .gemini
        .into_iter()
        .map(|(key, usage)| serde_json::json!({ "key": key, "usage": usage }))
        .collect::<Vec<_>>();
    let mut qwen = persisted
        .qwen
        .into_iter()
        .map(|(key, usage)| serde_json::json!({ "key": key, "usage": usage }))
        .collect::<Vec<_>>();
    let mut deepseek = persisted
        .deepseek
        .into_iter()
        .map(|(key, usage)| serde_json::json!({ "key": key, "usage": usage }))
        .collect::<Vec<_>>();
    let mut grok = persisted
        .grok
        .into_iter()
        .map(|(key, usage)| serde_json::json!({ "key": key, "usage": usage }))
        .collect::<Vec<_>>();
    let mut minimax = persisted
        .minimax
        .into_iter()
        .map(|(key, usage)| serde_json::json!({ "key": key, "usage": usage }))
        .collect::<Vec<_>>();
    let mut copilot = persisted
        .copilot
        .into_iter()
        .map(|(key, usage)| serde_json::json!({ "key": key, "usage": usage }))
        .collect::<Vec<_>>();
    let mut claude = persisted
        .claude
        .into_iter()
        .map(|(key, usage)| serde_json::json!({ "key": key, "usage": usage }))
        .collect::<Vec<_>>();
    let mut glm = persisted
        .glm
        .into_iter()
        .map(|(key, usage)| serde_json::json!({ "key": key, "usage": usage }))
        .collect::<Vec<_>>();
    codex.sort_by(|a, b| a["key"].as_str().cmp(&b["key"].as_str()));
    antigravity.sort_by(|a, b| a["key"].as_str().cmp(&b["key"].as_str()));
    gemini.sort_by(|a, b| a["key"].as_str().cmp(&b["key"].as_str()));
    qwen.sort_by(|a, b| a["key"].as_str().cmp(&b["key"].as_str()));
    deepseek.sort_by(|a, b| a["key"].as_str().cmp(&b["key"].as_str()));
    grok.sort_by(|a, b| a["key"].as_str().cmp(&b["key"].as_str()));
    minimax.sort_by(|a, b| a["key"].as_str().cmp(&b["key"].as_str()));
    copilot.sort_by(|a, b| a["key"].as_str().cmp(&b["key"].as_str()));
    claude.sort_by(|a, b| a["key"].as_str().cmp(&b["key"].as_str()));
    glm.sort_by(|a, b| a["key"].as_str().cmp(&b["key"].as_str()));
    serde_json::json!({
        "totals": {
            "requests": persisted.total_requests,
            "errors": persisted.total_errors,
            "prompt_total": persisted.total_prompt_total,
            "prompt_error_total": persisted.total_prompt_error_total,
            "input_tokens": persisted.total_input_tokens,
            "output_tokens": persisted.total_output_tokens,
            "total_tokens": persisted.total_tokens_used,
            "cache_tokens": persisted.total_cache_tokens,
            "reasoning_tokens": persisted.total_reasoning_tokens,
            "first_recorded_at": persisted.first_recorded_at,
            "last_recorded_at": persisted.last_recorded_at
        },
        "providers": {
            "codex": codex,
            "antigravity": antigravity,
            "gemini": gemini,
            "qwen": qwen,
            "deepseek": deepseek,
            "grok": grok,
            "minimax": minimax,
            "copilot": copilot,
            "claude": claude,
            "glm": glm
        }
    })
}

async fn mobile_usage_route(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(response) = require_admin_session_json(&state, &headers) {
        return response;
    }

    let quota = |provider: &str| {
        serde_json::json!({
            "accounts": quota_snapshot(&state, provider)
                .iter()
                .map(compact_mobile_quota_value)
                .collect::<Vec<_>>()
        })
    };
    let grok_accounts = compact_mobile_grok_quota_accounts(&state);
    axum::Json(serde_json::json!({
        "generated_at": now_rfc3339(),
        "session": {
            "enabled": admin_auth::is_enabled(&state.cfg.admin_auth),
            "configured": admin_auth::is_configured(&state.cfg.admin_auth),
            "authenticated": true
        },
        "summary": compact_mobile_usage_summary(usage_summary_json(&state)),
        "quotas": {
            "codex": quota("codex"),
            "agw": quota("antigravity"),
            "gemini": quota("gemini"),
            "qwen": quota("qwen"),
            "deepseek": quota("deepseek"),
            "minimax": quota("minimax"),
            "grok": { "accounts": grok_accounts },
            "copilot": quota("copilot"),
            "claude": quota("claude"),
            "glm": quota("glm")
        }
    }))
    .into_response()
}

fn compact_mobile_usage_summary(summary: serde_json::Value) -> serde_json::Value {
    let totals = summary
        .get("totals")
        .map(compact_scalar_object)
        .unwrap_or_else(|| serde_json::json!({}));
    let mut providers = serde_json::Map::new();
    if let Some(source) = summary
        .get("providers")
        .and_then(serde_json::Value::as_object)
    {
        for (provider, accounts) in source {
            let compact_accounts = accounts
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|account| {
                    let source = account.as_object()?;
                    let mut compact = serde_json::Map::new();
                    for key in ["key", "label", "model", "account"] {
                        if let Some(value) = source.get(key).filter(|value| is_json_scalar(value)) {
                            compact.insert(key.to_string(), value.clone());
                        }
                    }
                    if let Some(usage) = source.get("usage") {
                        compact.insert("usage".to_string(), compact_scalar_object(usage));
                    }
                    Some(serde_json::Value::Object(compact))
                })
                .collect::<Vec<_>>();
            providers.insert(provider.clone(), serde_json::Value::Array(compact_accounts));
        }
    }
    serde_json::json!({
        "totals": totals,
        "providers": providers
    })
}

fn compact_mobile_grok_quota_accounts(state: &AppState) -> Vec<serde_json::Value> {
    let source = target::grok::admin::cached_quota_json(state);
    let Some(account) = source.get("account").and_then(serde_json::Value::as_object) else {
        return Vec::new();
    };
    let mut normalized = account.clone();
    let limits = source
        .pointer("/kinds/DEFAULT_TEXT/rate_limits")
        .and_then(serde_json::Value::as_object)
        .into_iter()
        .flatten()
        .filter_map(|(scope, bucket)| {
            let mut compact = compact_mobile_quota_value(bucket).as_object()?.clone();
            compact.insert(
                "label".to_string(),
                serde_json::Value::String(scope.clone()),
            );
            Some(serde_json::Value::Object(compact))
        })
        .collect::<Vec<_>>();
    if !limits.is_empty() {
        normalized.insert("limits".to_string(), serde_json::Value::Array(limits));
    }
    vec![compact_mobile_quota_value(&serde_json::Value::Object(
        normalized,
    ))]
}

fn compact_mobile_quota_value(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Array(values) => {
            serde_json::Value::Array(values.iter().map(compact_mobile_quota_value).collect())
        }
        serde_json::Value::Object(source) => {
            let mut compact = serde_json::Map::new();
            for (key, value) in source {
                if MOBILE_QUOTA_SCALAR_KEYS.contains(&key.as_str()) && is_json_scalar(value) {
                    compact.insert(key.clone(), value.clone());
                } else if MOBILE_QUOTA_CONTAINER_KEYS.contains(&key.as_str()) {
                    compact.insert(key.clone(), compact_mobile_quota_value(value));
                } else if key == "rate_limit_reset_credits" {
                    if let Some(reset_credits) = compact_mobile_rate_limit_reset_credits(value) {
                        compact.insert(key.clone(), reset_credits);
                    }
                } else if key == "kinds" || key == "rate_limits" {
                    let children = value
                        .as_object()
                        .map(|children| {
                            children
                                .iter()
                                .map(|(name, child)| {
                                    (name.clone(), compact_mobile_quota_value(child))
                                })
                                .collect::<serde_json::Map<_, _>>()
                        })
                        .unwrap_or_default();
                    compact.insert(key.clone(), serde_json::Value::Object(children));
                }
            }
            serde_json::Value::Object(compact)
        }
        _ => value.clone(),
    }
}

fn compact_mobile_rate_limit_reset_credits(value: &serde_json::Value) -> Option<serde_json::Value> {
    let source = value.as_object()?;
    let mut compact = serde_json::Map::new();

    if let Some(available_count) = source
        .get("available_count")
        .and_then(serde_json::Value::as_i64)
    {
        compact.insert(
            "available_count".to_string(),
            serde_json::Value::from(available_count),
        );
    }

    if let Some(credits) = source.get("credits").and_then(serde_json::Value::as_array) {
        let credits = credits
            .iter()
            .filter_map(compact_mobile_rate_limit_reset_credit)
            .collect::<Vec<_>>();
        compact.insert("credits".to_string(), serde_json::Value::Array(credits));
    }

    Some(serde_json::Value::Object(compact))
}

fn compact_mobile_rate_limit_reset_credit(value: &serde_json::Value) -> Option<serde_json::Value> {
    let source = value.as_object()?;
    let mut compact = serde_json::Map::new();

    for key in MOBILE_RATE_LIMIT_RESET_CREDIT_SCALAR_KEYS {
        if let Some(value) = source.get(key).filter(|value| value.is_string()) {
            compact.insert(key.to_string(), value.clone());
        }
    }

    (!compact.is_empty()).then_some(serde_json::Value::Object(compact))
}

fn compact_scalar_object(value: &serde_json::Value) -> serde_json::Value {
    serde_json::Value::Object(
        value
            .as_object()
            .into_iter()
            .flatten()
            .filter(|(_, value)| is_json_scalar(value))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect(),
    )
}

fn is_json_scalar(value: &serde_json::Value) -> bool {
    matches!(
        value,
        serde_json::Value::Null
            | serde_json::Value::Bool(_)
            | serde_json::Value::Number(_)
            | serde_json::Value::String(_)
    )
}

#[cfg(test)]
mod mobile_usage_tests {
    use super::{compact_mobile_quota_value, compact_mobile_usage_summary};

    #[test]
    fn compact_summary_keeps_scalar_usage_and_drops_nested_history() {
        let compact = compact_mobile_usage_summary(serde_json::json!({
            "totals": {
                "requests": 12,
                "total_tokens": 345,
                "nested": { "unused": true }
            },
            "providers": {
                "codex": [{
                    "key": "account.json",
                    "usage": {
                        "requests": 4,
                        "total_tokens": 99,
                        "models": { "gpt": { "requests": 4 } }
                    },
                    "unused": "drop"
                }]
            }
        }));

        assert_eq!(compact["totals"]["requests"], 12);
        assert!(compact["totals"].get("nested").is_none());
        assert_eq!(compact["providers"]["codex"][0]["key"], "account.json");
        assert_eq!(
            compact["providers"]["codex"][0]["usage"]["total_tokens"],
            99
        );
        assert!(compact["providers"]["codex"][0]["usage"]
            .get("models")
            .is_none());
    }

    #[test]
    fn compact_quota_keeps_mobile_bars_and_removes_private_fields() {
        let compact = compact_mobile_quota_value(&serde_json::json!({
            "label": "Primary",
            "access_token": "secret",
            "code_generation": {
                "five_hour": {
                    "used_percent": 42.5,
                    "reset_label": "2h",
                    "raw": { "unused": true }
                }
            },
            "kinds": {
                "DEFAULT_TEXT": {
                    "rate_limits": {
                        "requests": {
                            "limit": 100,
                            "remaining": 75
                        }
                    }
                }
            }
        }));

        assert_eq!(compact["label"], "Primary");
        assert!(compact.get("access_token").is_none());
        assert_eq!(
            compact["code_generation"]["five_hour"]["used_percent"],
            42.5
        );
        assert_eq!(
            compact["kinds"]["DEFAULT_TEXT"]["rate_limits"]["requests"]["limit"],
            100
        );
    }

    #[test]
    fn compact_quota_keeps_safe_reset_credit_metadata_and_drops_sensitive_fields() {
        let account = compact_mobile_quota_value(&serde_json::json!({
            "label": "Primary",
            "rate_limit_reset_credits": {
                "available_count": 2,
                "access_token": "summary-access-token",
                "refresh_token": "summary-refresh-token",
                "cookie": "summary-cookie",
                "cookies": { "session": "summary-cookie" },
                "secret": "summary-secret",
                "arbitrary_upstream_field": { "secret": "do-not-expose" },
                "credits": [{
                    "id": "credit-123",
                    "reset_type": "five_hour",
                    "status": "available",
                    "granted_at": "2026-09-09T00:00:00Z",
                    "expires_at": "2026-09-10T00:00:00Z",
                    "title": "Reset 5-hour limit",
                    "description": "Restore the current rate limit window.",
                    "access_token": "credit-access-token",
                    "refresh_token": "credit-refresh-token",
                    "cookie": "credit-cookie",
                    "cookies": { "session": "credit-cookie" },
                    "secret": "credit-secret",
                    "arbitrary_upstream_field": { "token": "do-not-expose" }
                }]
            }
        }));
        let mobile_response = serde_json::json!({
            "quotas": {
                "codex": {
                    "accounts": [account]
                }
            }
        });

        let summary =
            &mobile_response["quotas"]["codex"]["accounts"][0]["rate_limit_reset_credits"];
        let credit = &summary["credits"][0];

        assert_eq!(summary["available_count"], 2);
        assert_eq!(credit["id"], "credit-123");
        assert_eq!(credit["reset_type"], "five_hour");
        assert_eq!(credit["status"], "available");
        assert_eq!(credit["granted_at"], "2026-09-09T00:00:00Z");
        assert_eq!(credit["expires_at"], "2026-09-10T00:00:00Z");
        assert_eq!(credit["title"], "Reset 5-hour limit");
        assert_eq!(
            credit["description"],
            "Restore the current rate limit window."
        );
        assert_eq!(summary.as_object().map(|value| value.len()), Some(2));
        assert_eq!(credit.as_object().map(|value| value.len()), Some(7));

        for key in [
            "access_token",
            "refresh_token",
            "cookie",
            "cookies",
            "secret",
            "arbitrary_upstream_field",
        ] {
            assert!(summary.get(key).is_none(), "summary leaked {key}");
            assert!(credit.get(key).is_none(), "credit leaked {key}");
        }
    }
}

async fn usage_filter_summary_route(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<usage_store::UsageFilterSummaryQuery>,
) -> Response {
    if let Some(response) = require_admin_session_json(&state, &headers) {
        return response;
    }
    let cfg = state.cfg.clone();
    let summary =
        match tokio::task::spawn_blocking(move || usage_store::summarize_filtered(&cfg, &query))
            .await
        {
            Ok(result) => result,
            Err(err) => Err(format!("usage summary worker failed: {}", err)),
        };
    match summary {
        Ok(summary) => axum::Json(serde_json::json!({ "summary": summary })).into_response(),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            [("Content-Type", "application/json")],
            openai_error_body(&err, "server_error", None),
        )
            .into_response(),
    }
}

async fn usage_history_route(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<usage_store::UsageHistoryQuery>,
) -> Response {
    if let Some(response) = require_admin_session_json(&state, &headers) {
        return response;
    }
    let cfg = state.cfg.clone();
    let events = match tokio::task::spawn_blocking(move || usage_store::load(&cfg, &query)).await {
        Ok(result) => result,
        Err(err) => Err(format!("history worker failed: {}", err)),
    };
    match events {
        Ok(events) => axum::Json(serde_json::json!({ "events": events })).into_response(),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            [("Content-Type", "application/json")],
            openai_error_body(&err, "server_error", None),
        )
            .into_response(),
    }
}

#[derive(Deserialize)]
struct ContextHistoryQuery {
    #[serde(default = "default_context_hours")]
    hours: u64,
    #[serde(default = "default_context_bucket_minutes")]
    bucket_minutes: u64,
    #[serde(default)]
    account_key: Option<String>,
    #[serde(default)]
    per_model: bool,
}

fn default_context_hours() -> u64 {
    24
}
fn default_context_bucket_minutes() -> u64 {
    5
}

async fn context_history_route(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ContextHistoryQuery>,
) -> Response {
    if let Some(response) = require_admin_session_json(&state, &headers) {
        return response;
    }
    let hours = query.hours.max(1).min(720);
    let requested_bucket_minutes = query.bucket_minutes.max(1).min(60);
    let max_buckets = 720;
    let minimum_bucket_minutes = ((hours * 60) + max_buckets - 1) / max_buckets;
    let bucket_minutes = requested_bucket_minutes.max(minimum_bucket_minutes).min(60);
    let account_filter = query
        .account_key
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty());
    let per_model = query.per_model;

    let bucket_secs = bucket_minutes * 60;
    let cutoff = chrono::Utc::now() - chrono::Duration::hours(hours as i64);
    let cutoff_str = cutoff.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let requested_start_ts = (cutoff.timestamp() / bucket_secs as i64) * bucket_secs as i64;
    let end_bucket_ts = (chrono::Utc::now().timestamp() / bucket_secs as i64) * bucket_secs as i64;
    let requested_buckets =
        ((end_bucket_ts - requested_start_ts) / bucket_secs as i64 + 1).max(1) as usize;
    let num_buckets = requested_buckets.min(max_buckets as usize);
    let start_ts = end_bucket_ts - (num_buckets.saturating_sub(1) as i64 * bucket_secs as i64);

    let cfg = state.cfg.clone();
    let account_filter_for_query = account_filter.map(str::to_string);
    let aggregate = tokio::task::spawn_blocking(move || {
        usage_store::aggregate_context(
            &cfg,
            &cutoff_str,
            bucket_secs,
            account_filter_for_query.as_deref(),
            per_model,
        )
    })
    .await;
    let rows = match aggregate {
        Ok(Ok(rows)) => rows,
        Ok(Err(err)) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                [("Content-Type", "application/json")],
                openai_error_body(&err, "server_error", None),
            )
                .into_response();
        }
        Err(err) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                [("Content-Type", "application/json")],
                openai_error_body(
                    &format!("history worker failed: {}", err),
                    "server_error",
                    None,
                ),
            )
                .into_response();
        }
    };
    if rows.is_empty() {
        return axum::Json(serde_json::json!({
            "labels": [],
            "buckets": [],
            "models": {},
            "hours": hours,
            "bucket_minutes": bucket_minutes
        }))
        .into_response();
    }

    let mut labels = Vec::with_capacity(num_buckets);
    let label_format = if hours > 48 {
        "%b %-d, %Y, %H:%M:%S"
    } else {
        "%H:%M:%S"
    };
    for i in 0..num_buckets {
        let bucket_start = start_ts + (i as i64 * bucket_secs as i64);
        let dt = chrono::DateTime::from_timestamp(bucket_start, 0)
            .unwrap_or(chrono::DateTime::UNIX_EPOCH);
        labels.push(dt.format(label_format).to_string());
    }

    if per_model {
        let mut model_data: HashMap<String, Vec<serde_json::Value>> = HashMap::new();

        for row in rows {
            let model = row.model.unwrap_or_else(|| "unknown".to_string());
            let bucket_idx = ((row.bucket_start - start_ts) / bucket_secs as i64) as usize;
            if bucket_idx >= num_buckets {
                continue;
            }

            let buckets = model_data.entry(model).or_insert_with(|| {
                vec![serde_json::json!({"input": 0u64, "output": 0u64, "cache": 0u64, "reasoning": 0u64}); num_buckets]
            });

            let b = &mut buckets[bucket_idx];
            b["input"] = serde_json::json!(row.input_tokens);
            b["output"] = serde_json::json!(row.output_tokens);
            b["cache"] = serde_json::json!(row.cache_tokens);
            b["reasoning"] = serde_json::json!(row.reasoning_tokens);
        }

        return axum::Json(serde_json::json!({
            "labels": labels,
            "models": model_data,
            "hours": hours,
            "bucket_minutes": bucket_minutes
        }))
        .into_response();
    }

    // Default: aggregate all together
    let mut buckets = vec![
        serde_json::json!({
            "input_tokens": 0u64,
            "output_tokens": 0u64,
            "total_tokens": 0u64,
            "cache_tokens": 0u64,
            "reasoning_tokens": 0u64,
            "request_count": 0u64,
        });
        num_buckets
    ];

    for row in rows {
        let bucket_idx = ((row.bucket_start - start_ts) / bucket_secs as i64) as usize;
        if bucket_idx >= num_buckets {
            continue;
        }
        let b = &mut buckets[bucket_idx];
        b["input_tokens"] = serde_json::json!(row.input_tokens);
        b["output_tokens"] = serde_json::json!(row.output_tokens);
        b["total_tokens"] = serde_json::json!(row.total_tokens);
        b["cache_tokens"] = serde_json::json!(row.cache_tokens);
        b["reasoning_tokens"] = serde_json::json!(row.reasoning_tokens);
        b["request_count"] = serde_json::json!(row.request_count);
    }

    axum::Json(serde_json::json!({
        "labels": labels,
        "buckets": buckets,
        "hours": hours,
        "bucket_minutes": bucket_minutes
    }))
    .into_response()
}

async fn temp_file_route(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Response {
    if let Some(response) = require_admin_session_text(&state, &headers) {
        return response;
    }
    if name.is_empty() || name.contains('/') || name.contains('\\') || name.contains("..") {
        return (StatusCode::BAD_REQUEST, "invalid file name").into_response();
    }

    let path = generated_temp_download_dir().join(&name);
    let body = match std::fs::read(&path) {
        Ok(body) => body,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return (StatusCode::NOT_FOUND, "file not found").into_response();
        }
        Err(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "failed to read temp file",
            )
                .into_response();
        }
    };

    let content_type = if name.ends_with(".png") {
        "image/png"
    } else if name.ends_with(".jpg") || name.ends_with(".jpeg") {
        "image/jpeg"
    } else if name.ends_with(".webp") {
        "image/webp"
    } else if name.ends_with(".gif") {
        "image/gif"
    } else {
        "application/octet-stream"
    };

    (
        StatusCode::OK,
        [
            ("Content-Type", content_type),
            ("Content-Disposition", "attachment"),
            ("Cache-Control", "no-store"),
        ],
        body,
    )
        .into_response()
}

/// Deletes a saved credential file and reloads the in-memory token list.
#[utoipa::path(
    post,
    path = "/credentials/delete",
    request_body(
        content = crate::source::openapi::DeleteCredentialRequest,
        content_type = "application/x-www-form-urlencoded",
        description = "Credential filename from the auth directory"
    ),
    security(("bearer_auth" = [])),
    responses(
        (
            status = 200,
            description = "Delete result",
            body = crate::source::openapi::ActionResponse
        ),
        (
            status = 401,
            description = "Missing or invalid proxy API key",
            body = crate::source::openapi::ActionResponse
        )
    )
)]
async fn delete_credential_route(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<target::codex::admin::DeleteForm>,
) -> Response {
    if let Some(response) = require_admin_session_json(&state, &headers) {
        return response;
    }
    let response = target::codex::admin::delete_credential(State(state.clone()), Form(form))
        .await
        .into_response();
    invalidate_model_caches().await;
    prune_account_routing_settings(&state);
    response
}

/// Enables or disables a saved credential file and persists the disabled list.
#[utoipa::path(
    post,
    path = "/credentials/toggle",
    request_body(
        content = crate::source::openapi::ToggleCredentialRequest,
        content_type = "application/x-www-form-urlencoded",
        description = "Credential filename and target enabled state"
    ),
    security(("bearer_auth" = [])),
    responses(
        (
            status = 200,
            description = "Toggle result",
            body = crate::source::openapi::ActionResponse
        ),
        (
            status = 401,
            description = "Missing or invalid proxy API key",
            body = crate::source::openapi::ActionResponse
        )
    )
)]
async fn toggle_credential_route(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<target::codex::admin::ToggleForm>,
) -> Response {
    if let Some(response) = require_admin_session_json(&state, &headers) {
        return response;
    }
    let response = target::codex::admin::toggle_credential(State(state.clone()), Form(form))
        .await
        .into_response();
    invalidate_model_caches().await;
    prune_account_routing_settings(&state);
    response
}

pub(crate) fn model_from_request_value(value: &serde_json::Value) -> Option<String> {
    value
        .get("model")
        .and_then(|model| model.as_str())
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .map(|model| model.to_string())
}

pub(crate) fn prompt_metrics_from_request_value(value: &serde_json::Value) -> PromptMetrics {
    let mut metrics = PromptMetrics::default();
    if let Some(instructions) = value
        .get("instructions")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|text| !text.is_empty())
    {
        metrics.input_chars += instructions.chars().count() as u64;
        metrics.prompt_items += 1;
        metrics.is_prompt = true;
    }

    if let Some(input) = value.get("input") {
        append_prompt_value(&mut metrics, input);
    }

    if let Some(messages) = value.get("messages") {
        append_prompt_value(&mut metrics, messages);
    }

    if !metrics.is_prompt {
        metrics.is_prompt = metrics.input_chars > 0 || metrics.prompt_items > 0;
    }

    metrics
}

fn append_prompt_value(metrics: &mut PromptMetrics, value: &serde_json::Value) {
    match value {
        serde_json::Value::String(text) => {
            let text = text.trim();
            if !text.is_empty() {
                metrics.input_chars += text.chars().count() as u64;
                metrics.prompt_items += 1;
                metrics.is_prompt = true;
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                append_prompt_value(metrics, item);
            }
        }
        serde_json::Value::Object(map) => {
            let mut found_direct_text = false;
            if let Some(text) = map
                .get("text")
                .and_then(|value| value.as_str())
                .or_else(|| map.get("input_text").and_then(|value| value.as_str()))
                .or_else(|| map.get("output_text").and_then(|value| value.as_str()))
                .or_else(|| {
                    map.get("content").and_then(|value| {
                        if value.is_string() {
                            value.as_str()
                        } else {
                            None
                        }
                    })
                })
            {
                let text = text.trim();
                if !text.is_empty() {
                    metrics.input_chars += text.chars().count() as u64;
                    metrics.prompt_items += 1;
                    metrics.is_prompt = true;
                    found_direct_text = true;
                }
            }
            // Only recurse into content array if no direct text was found —
            // avoids double-counting when the same content is expressed both
            // as a top-level text field and as a structured content array.
            if !found_direct_text {
                if let Some(content) = map.get("content").filter(|value| value.is_array()) {
                    append_prompt_value(metrics, content);
                }
            }
            if let Some(input) = map.get("input") {
                append_prompt_value(metrics, input);
            }
            if let Some(messages) = map.get("messages") {
                append_prompt_value(metrics, messages);
            }
        }
        _ => {}
    }
}

pub(crate) fn usage_metrics_from_response_value(value: &serde_json::Value) -> UsageMetrics {
    let usage = value
        .get("usage")
        .cloned()
        .or_else(|| {
            value
                .get("message")
                .and_then(|message| message.get("usage"))
                .cloned()
        })
        .or_else(|| {
            value
                .get("response")
                .and_then(|resp| resp.get("usage"))
                .cloned()
        })
        .unwrap_or(serde_json::Value::Null);

    let input_tokens = usage
        .get("input_tokens")
        .and_then(|v| v.as_u64())
        .or_else(|| usage.get("prompt_tokens").and_then(|v| v.as_u64()))
        .unwrap_or(0);
    let output_tokens = usage
        .get("output_tokens")
        .and_then(|v| v.as_u64())
        .or_else(|| usage.get("completion_tokens").and_then(|v| v.as_u64()))
        .unwrap_or(0);
    let total_tokens = usage
        .get("total_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(input_tokens + output_tokens);
    let cache_tokens = usage
        .get("input_tokens_details")
        .and_then(|v| v.get("cached_tokens"))
        .and_then(|v| v.as_u64())
        .or_else(|| usage.get("cache_tokens").and_then(|v| v.as_u64()))
        .or_else(|| {
            let read = usage
                .get("cache_read_input_tokens")
                .and_then(|v| v.as_u64());
            let create = usage
                .get("cache_creation_input_tokens")
                .and_then(|v| v.as_u64());
            match (read, create) {
                (Some(read), Some(create)) => Some(read.saturating_add(create)),
                (Some(read), None) => Some(read),
                (None, Some(create)) => Some(create),
                _ => None,
            }
        })
        .unwrap_or(0);
    let reasoning_tokens = usage
        .get("output_tokens_details")
        .and_then(|v| v.get("reasoning_tokens"))
        .and_then(|v| v.as_u64())
        .or_else(|| usage.get("reasoning_tokens").and_then(|v| v.as_u64()))
        .unwrap_or(0);

    UsageMetrics {
        input_tokens,
        output_tokens,
        total_tokens,
        cache_tokens,
        reasoning_tokens,
        raw_usage: if usage.is_null() { None } else { Some(usage) },
    }
}

pub(crate) fn apply_estimated_usage_fallback(
    metrics: &mut UsageMetrics,
    prompt: &PromptMetrics,
    output_text: &str,
) {
    let output_chars = output_text.trim().chars().count() as u64;
    let mut estimated = false;

    if metrics.input_tokens == 0 {
        let input_tokens = estimated_tokens_from_chars(prompt.input_chars);
        if input_tokens > 0 {
            metrics.input_tokens = input_tokens;
            estimated = true;
        }
    }

    if metrics.output_tokens == 0 {
        let output_tokens = estimated_tokens_from_chars(output_chars);
        if output_tokens > 0 {
            metrics.output_tokens = output_tokens;
            estimated = true;
        }
    }

    if metrics.total_tokens == 0 {
        metrics.total_tokens = metrics.input_tokens.saturating_add(metrics.output_tokens);
    }

    if !estimated {
        return;
    }

    let estimated_usage = serde_json::json!({
        "provider": "qwen",
        "input_chars": prompt.input_chars,
        "output_chars": output_chars,
        "input_tokens": metrics.input_tokens,
        "output_tokens": metrics.output_tokens,
        "total_tokens": metrics.total_tokens
    });

    metrics.raw_usage = Some(match metrics.raw_usage.take() {
        Some(serde_json::Value::Object(mut usage)) => {
            usage.insert("estimated_usage".to_string(), estimated_usage);
            serde_json::Value::Object(usage)
        }
        Some(usage) => serde_json::json!({
            "upstream_usage": usage,
            "estimated_usage": estimated_usage
        }),
        None => serde_json::json!({
            "estimated_usage": estimated_usage
        }),
    });
}

fn estimated_tokens_from_chars(chars: u64) -> u64 {
    if chars == 0 {
        0
    } else {
        chars.saturating_add(3) / 4
    }
}

fn usage_metrics_from_sse_response_body(body: &Bytes) -> Option<UsageMetrics> {
    let text = String::from_utf8_lossy(body);
    for line in text.lines() {
        let data = match line.strip_prefix("data: ") {
            Some(data) => data.trim(),
            None => continue,
        };
        if data == "[DONE]" {
            break;
        }
        let value: serde_json::Value = match serde_json::from_str(data) {
            Ok(value) => value,
            Err(_) => continue,
        };
        if value.get("type").and_then(|v| v.as_str()) == Some("response.completed") {
            if let Some(response) = value.get("response") {
                return Some(usage_metrics_from_response_value(response));
            }
        }
    }
    None
}

fn sse_error_message(body: &Bytes) -> Option<String> {
    let text = String::from_utf8_lossy(body);
    for line in text.lines() {
        let Some(data) = line.strip_prefix("data: ").map(str::trim) else {
            continue;
        };
        if data == "[DONE]" {
            continue;
        }
        let value: serde_json::Value = match serde_json::from_str(data) {
            Ok(value) => value,
            Err(_) => continue,
        };
        if let Some(message) = sse_error_from_value(&value) {
            return Some(message);
        }
    }
    None
}

fn sse_error_from_value(value: &serde_json::Value) -> Option<String> {
    let event_type = value.get("type").and_then(|v| v.as_str()).unwrap_or("");
    let error = value.get("error");
    let response_error = value
        .get("response")
        .and_then(|response| response.get("error"));
    if event_type != "error"
        && event_type != "response.failed"
        && error.is_none()
        && response_error.is_none()
    {
        return None;
    }
    Some(
        error
            .and_then(|error| error.get("message"))
            .or_else(|| value.get("message"))
            .or_else(|| response_error.and_then(|error| error.get("message")))
            .and_then(|message| message.as_str())
            .unwrap_or("upstream returned an SSE error")
            .to_string(),
    )
}

type ReqwestByteStream =
    Pin<Box<dyn Stream<Item = Result<Bytes, reqwest::Error>> + Send + 'static>>;

async fn read_sse_prelude(
    stream: &mut ReqwestByteStream,
    timeout: Duration,
) -> Result<Bytes, String> {
    const MAX_PRELUDE_BYTES: usize = 1024 * 1024;

    let deadline = tokio::time::Instant::now() + timeout;
    let mut prefix = BytesMut::new();
    let mut parser = BytesMut::new();
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err(format!(
                "upstream first-event timeout after {} seconds",
                timeout.as_secs()
            ));
        }
        let next = tokio::time::timeout(remaining, stream.next())
            .await
            .map_err(|_| {
                format!(
                    "upstream first-event timeout after {} seconds",
                    timeout.as_secs()
                )
            })?;
        let chunk = match next {
            Some(Ok(chunk)) => chunk,
            Some(Err(err)) => return Err(format!("upstream stream read failed: {}", err)),
            None => return Err("upstream stream ended before producing a response".to_string()),
        };
        prefix.extend_from_slice(&chunk);
        parser.extend_from_slice(&chunk);
        if prefix.len() > MAX_PRELUDE_BYTES {
            return Err("upstream produced an oversized SSE prelude".to_string());
        }

        while let Some((event_end, delimiter_len)) = find_sse_event_boundary(&parser) {
            let raw_event = parser.split_to(event_end + delimiter_len);
            let event = &raw_event[..event_end];
            let text = String::from_utf8_lossy(event);
            let data = text
                .lines()
                .filter_map(|line| {
                    line.trim_end_matches('\r')
                        .strip_prefix("data:")
                        .map(str::trim_start)
                })
                .collect::<Vec<_>>()
                .join("\n");
            if data.is_empty() {
                continue;
            }
            if data == "[DONE]" {
                return Ok(prefix.freeze());
            }
            let Ok(value) = serde_json::from_str::<serde_json::Value>(&data) else {
                continue;
            };
            if let Some(message) = sse_error_from_value(&value) {
                return Err(message);
            }
            let event_type = value
                .get("type")
                .and_then(|value| value.as_str())
                .unwrap_or("");
            if !matches!(
                event_type,
                "" | "ping" | "response.created" | "response.in_progress"
            ) {
                return Ok(prefix.freeze());
            }
        }
    }
}

async fn proxy(
    State(state): State<AppState>,
    method: Method,
    headers: HeaderMap,
    OriginalUri(uri): OriginalUri,
    body: Body,
) -> impl IntoResponse {
    let raw_path = uri.path().to_string();
    let source_api = detect_source_api(&raw_path);

    let Some(authenticated) = authenticate_api_key(&state, &headers) else {
        return if matches!(source_api, SourceApi::V1) {
            (
                StatusCode::UNAUTHORIZED,
                [(
                    axum::http::header::CONTENT_TYPE.as_str(),
                    "application/json",
                )],
                openai_error_body(
                    "Missing bearer authentication in header",
                    "invalid_request_error",
                    None,
                ),
            )
                .into_response()
        } else {
            (StatusCode::UNAUTHORIZED, "unauthorized").into_response()
        };
    };
    let mut state = state;
    state.request_api_key_id = authenticated.id.clone();

    let content_length = valid_content_length(&headers);
    if content_length.is_some_and(|length| length > state.cfg.max_request_body_bytes as u64) {
        warn!(
            path = %raw_path,
            content_length,
            max_request_body_bytes = state.cfg.max_request_body_bytes,
            "rejecting oversized request body from Content-Length"
        );
        return oversized_request_body_response(
            source_api,
            state.cfg.max_request_body_bytes,
            content_length,
        );
    }

    // Read full body (small/simple proxy)
    let body_bytes = match axum::body::to_bytes(body, state.cfg.max_request_body_bytes).await {
        Ok(b) => b,
        Err(err) => {
            if is_length_limit_error(&err) {
                warn!(
                    path = %raw_path,
                    content_length,
                    max_request_body_bytes = state.cfg.max_request_body_bytes,
                    "rejecting request body that exceeded the streaming limit"
                );
                return oversized_request_body_response(
                    source_api,
                    state.cfg.max_request_body_bytes,
                    content_length,
                );
            } else {
                warn!(path = %raw_path, error = %err, "request body read failed");
                return invalid_request_body_response(source_api);
            }
        }
    };

    let routed = match route_request(&raw_path, &uri, &method, &headers, body_bytes) {
        Ok(r) => r,
        Err(e) => {
            return if matches!(source_api, SourceApi::V1) {
                (
                    e.status,
                    [(
                        axum::http::header::CONTENT_TYPE.as_str(),
                        "application/json",
                    )],
                    openai_error_body(e.message, "invalid_request_error", None),
                )
                    .into_response()
            } else {
                (e.status, e.message).into_response()
            };
        }
    };
    if let Some(provider) = target_provider_access_key(routed.target) {
        if !authenticated.access.allows_provider(provider) {
            return if matches!(source_api, SourceApi::V1) {
                (
                    StatusCode::FORBIDDEN,
                    [(
                        axum::http::header::CONTENT_TYPE.as_str(),
                        "application/json",
                    )],
                    openai_error_body(
                        &format!("API key does not have access to {}", provider),
                        "permission_error",
                        None,
                    ),
                )
                    .into_response()
            } else {
                (StatusCode::FORBIDDEN, "API key provider access denied").into_response()
            };
        }
    }
    let request_value_for_limits: Option<serde_json::Value> =
        serde_json::from_slice(&routed.upstream_body).ok();
    let prompt_for_limits = request_value_for_limits
        .as_ref()
        .map(prompt_metrics_from_request_value)
        .unwrap_or_default();
    if let Some(response) = api_key_prompt_limit_response(
        source_api,
        &authenticated.access,
        None,
        None,
        prompt_for_limits.estimated_input_tokens(),
    ) {
        return response;
    }
    if let Some(provider) = target_provider_access_key(routed.target) {
        if let Some(response) = api_key_prompt_limit_response(
            source_api,
            &authenticated.access,
            Some(provider),
            None,
            prompt_for_limits.estimated_input_tokens(),
        ) {
            return response;
        }
    }
    if let Some(response) = api_key_account_prompt_limit_response_for_target(
        source_api,
        &state,
        &authenticated.access,
        routed.target,
        prompt_for_limits.estimated_input_tokens(),
    ) {
        return response;
    }
    match routed.target {
        TargetModel::CodexModels => {
            let state = scoped_state_for_api_key_access(
                state,
                &authenticated.access,
                Some(TargetModel::Codex),
            );
            return codex_models_response(state, headers).await;
        }
        TargetModel::UnifiedV1Models => {
            return unified_v1_models_response(
                state,
                headers,
                &routed.upstream_path,
                &authenticated.access,
            )
            .await;
        }
        TargetModel::Custom => {
            return custom_model_response(
                state,
                headers,
                &routed.upstream_path,
                routed.upstream_body,
                routed.response_mode,
                &authenticated.access,
                prompt_for_limits,
            )
            .await;
        }
        _ => {}
    }
    let state = scoped_state_for_api_key_request(
        state,
        &authenticated.access,
        Some(routed.target),
        Some(prompt_for_limits.estimated_input_tokens()),
    );
    match routed.target {
        TargetModel::Antigravity => {
            return match routed.upstream_path.as_str() {
                "responses/compact" => {
                    target::antigravity::api::compact(State(state), headers, routed.upstream_body)
                        .await
                        .into_response()
                }
                _ => {
                    target::antigravity::api::responses(State(state), headers, routed.upstream_body)
                        .await
                        .into_response()
                }
            };
        }
        TargetModel::Gemini => {
            return target::gemini::api::responses(State(state), headers, routed.upstream_body)
                .await
                .into_response();
        }
        TargetModel::Qwen => {
            return target::qwen::api::responses(State(state), headers, routed.upstream_body)
                .await
                .into_response();
        }
        TargetModel::DeepSeek => {
            return target::deepseek::api::responses(State(state), headers, routed.upstream_body)
                .await
                .into_response();
        }
        TargetModel::Grok => {
            return match routed.upstream_path.as_str() {
                "images/generations" => target::grok::api::image_generations(
                    State(state),
                    headers,
                    routed.upstream_body,
                )
                .await
                .into_response(),
                "videos/generations" => target::grok::api::video_generations(
                    State(state),
                    headers,
                    routed.upstream_body,
                )
                .await
                .into_response(),
                _ => target::grok::api::responses(State(state), headers, routed.upstream_body)
                    .await
                    .into_response(),
            };
        }
        TargetModel::MiniMax => {
            return match routed.upstream_path.as_str() {
                "anthropic/v1/messages" => target::minimax::anthropic::messages(
                    State(state),
                    headers,
                    routed.upstream_body,
                )
                .await
                .into_response(),
                "images/generations" => target::minimax::media::image_generations(
                    State(state),
                    headers,
                    routed.upstream_body,
                )
                .await
                .into_response(),
                "videos/generations" => target::minimax::media::video_generations(
                    State(state),
                    headers,
                    routed.upstream_body,
                )
                .await
                .into_response(),
                path if path.starts_with("videos/") => target::minimax::media::video_status(
                    State(state),
                    headers,
                    path.trim_start_matches("videos/"),
                )
                .await
                .into_response(),
                _ => target::minimax::responses_native::responses(
                    State(state),
                    headers,
                    routed.upstream_body,
                )
                .await
                .into_response(),
            };
        }
        TargetModel::Copilot => {
            return match routed.upstream_path.as_str() {
                "anthropic/v1/messages" => {
                    target::copilot::api::messages(State(state), headers, routed.upstream_body)
                        .await
                        .into_response()
                }
                _ => target::copilot::api::responses(State(state), headers, routed.upstream_body)
                    .await
                    .into_response(),
            };
        }
        TargetModel::Claude => {
            return match routed.upstream_path.as_str() {
                "anthropic/v1/messages" => {
                    target::claude::api::messages(State(state), headers, routed.upstream_body)
                        .await
                        .into_response()
                }
                _ => target::claude::api::responses(State(state), headers, routed.upstream_body)
                    .await
                    .into_response(),
            };
        }
        TargetModel::Glm => {
            return match routed.upstream_path.as_str() {
                "anthropic/v1/messages" => {
                    target::glm::anthropic::messages(State(state), headers, routed.upstream_body)
                        .await
                        .into_response()
                }
                "chat/completions" => {
                    target::glm::api::chat_completions(State(state), headers, routed.upstream_body)
                        .await
                        .into_response()
                }
                _ => target::glm::api::responses(State(state), headers, routed.upstream_body)
                    .await
                    .into_response(),
            };
        }
        TargetModel::Codex => {}
        TargetModel::Custom | TargetModel::CodexModels | TargetModel::UnifiedV1Models => {
            unreachable!("special targets return before provider dispatch")
        }
    }
    let upstream = match routed.target {
        TargetModel::Codex => target::codex::gateway::build_upstream_url(
            &state.cfg.upstream_base,
            &routed.upstream_path,
            routed.upstream_query.as_deref(),
        ),
        TargetModel::Antigravity
        | TargetModel::Gemini
        | TargetModel::Qwen
        | TargetModel::DeepSeek
        | TargetModel::Grok
        | TargetModel::MiniMax
        | TargetModel::Copilot
        | TargetModel::Claude
        | TargetModel::Glm
        | TargetModel::Custom
        | TargetModel::CodexModels
        | TargetModel::UnifiedV1Models => unreachable!("non-codex targets return earlier"),
    };
    let token_candidates = candidate_tokens_with_reservation(&state, true);
    if token_candidates.is_empty() {
        return codex_error_response(
            source_api,
            StatusCode::SERVICE_UNAVAILABLE,
            "No eligible Codex credentials configured or enabled for this API key",
        );
    }
    let request_value: Option<serde_json::Value> =
        serde_json::from_slice(&routed.upstream_body).ok();
    let prompt = request_value
        .as_ref()
        .map(prompt_metrics_from_request_value)
        .unwrap_or_default();
    let model = request_value.as_ref().and_then(model_from_request_value);
    let mut last_error: Option<(StatusCode, String)> = None;
    let mut selected_response = None;

    for (attempt_idx, (_token_idx, token)) in token_candidates.iter().enumerate() {
        let session_id = Uuid::new_v4().to_string();
        let codex_context = codex_usage_context(
            token,
            model.clone(),
            routed.upstream_path.clone(),
            prompt.clone(),
        );
        record_codex_request(&state, &codex_context);

        let body_bytes = target::codex::gateway::build_request_body(
            &method,
            &routed.upstream_path,
            &headers,
            routed.upstream_body.clone(),
            &session_id,
        );
        let mut req = state
            .client
            .request(method.clone(), upstream.clone())
            .body(body_bytes);

        // Copy headers except hop-by-hop/auth and proxy-edge client headers; set upstream auth
        for (k, v) in headers.iter() {
            if should_drop_incoming_header(k.as_str()) {
                continue;
            }
            req = req.header(k, v);
        }
        req = req.header("Authorization", format!("Bearer {}", token.token));
        req = target::codex::gateway::apply_default_headers(
            req,
            &headers,
            token.account_id.as_deref(),
            &session_id,
        );

        let resp = match req.send().await {
            Ok(r) => r,
            Err(err) => {
                let message = format!("upstream send failed: {}", err);
                error!("upstream error: {}", err);
                record_codex_error(&state, &codex_context, &message);
                last_error = Some((StatusCode::BAD_GATEWAY, message));
                if attempt_idx + 1 < token_candidates.len() {
                    continue;
                }
                break;
            }
        };

        let status = resp.status();
        if status.as_u16() >= 400 {
            let retry_after = resp
                .headers()
                .get(axum::http::header::RETRY_AFTER)
                .and_then(|value| value.to_str().ok())
                .map(str::to_string);
            let mut out_headers = HeaderMap::new();
            for (k, v) in resp.headers().iter() {
                let name = k.as_str().to_ascii_lowercase();
                if is_hop_header(&name) || name == "content-encoding" || name == "content-length" {
                    continue;
                }
                out_headers.insert(k, v.clone());
            }
            let body_bytes = match resp.bytes().await {
                Ok(b) => b,
                Err(err) => {
                    let message = format!("upstream error body read failed: {}", err);
                    error!("{}", message);
                    record_codex_error(&state, &codex_context, &message);
                    last_error = Some((StatusCode::BAD_GATEWAY, message));
                    if attempt_idx + 1 < token_candidates.len() {
                        continue;
                    }
                    break;
                }
            };
            let message = upstream_failure_message(status, retry_after.as_deref(), &body_bytes);
            record_codex_error(&state, &codex_context, &message);
            if attempt_idx + 1 < token_candidates.len()
                && should_retry_account_error(status, &message)
            {
                last_error = Some((status, message));
                continue;
            }
            return if matches!(source_api, SourceApi::V1) {
                let mut headers = out_headers;
                headers.insert(
                    axum::http::header::CONTENT_TYPE,
                    HeaderValue::from_static("application/json"),
                );
                (
                    status,
                    headers,
                    upstream_error_to_openai(status, &body_bytes),
                )
                    .into_response()
            } else {
                (status, out_headers, body_bytes).into_response()
            };
        }

        let is_sse = resp
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.contains("text/event-stream"));
        if is_sse && matches!(routed.response_mode, ResponseMode::SseToJson) {
            let body_bytes = match resp.bytes().await {
                Ok(body) => body,
                Err(err) => {
                    let message = format!("upstream body read failed: {}", err);
                    record_codex_error(&state, &codex_context, &message);
                    last_error = Some((StatusCode::BAD_GATEWAY, message));
                    if attempt_idx + 1 < token_candidates.len() {
                        continue;
                    }
                    break;
                }
            };
            if let Some(message) = sse_error_message(&body_bytes) {
                let affects_account_health = codex_error_affects_account_health(&message);
                record_codex_error_with_health(
                    &state,
                    &codex_context,
                    &message,
                    affects_account_health,
                );
                last_error = Some((StatusCode::BAD_GATEWAY, message.clone()));
                if affects_account_health && attempt_idx + 1 < token_candidates.len() {
                    continue;
                }
                let status = codex_error_status_from_message(&message, StatusCode::BAD_GATEWAY);
                return codex_error_response(source_api, status, message);
            }
            let metrics = usage_metrics_from_sse_response_body(&body_bytes).unwrap_or_default();
            record_usage_success(&state, &codex_context, &metrics);
            return (
                status,
                [("Content-Type", "application/json")],
                sse_to_response_json(&body_bytes),
            )
                .into_response();
        }

        if is_sse {
            let mut out_headers = HeaderMap::new();
            for (key, value) in resp.headers().iter() {
                let name = key.as_str().to_ascii_lowercase();
                if !is_hop_header(&name) && name != "content-encoding" && name != "content-length" {
                    out_headers.insert(key.clone(), value.clone());
                }
            }
            let mut upstream: ReqwestByteStream = Box::pin(resp.bytes_stream());
            let first_event_timeout =
                Duration::from_secs(state.cfg.upstream_first_event_timeout_seconds.max(1));
            match read_sse_prelude(&mut upstream, first_event_timeout).await {
                Ok(prefix) => {
                    return codex_live_stream_response(
                        state.clone(),
                        codex_context,
                        source_api,
                        status,
                        out_headers,
                        prefix,
                        upstream,
                    );
                }
                Err(message) => {
                    let status = codex_error_status_from_message(&message, StatusCode::BAD_GATEWAY);
                    let affects_account_health = codex_error_affects_account_health(&message);
                    record_codex_error_with_health(
                        &state,
                        &codex_context,
                        &message,
                        affects_account_health,
                    );
                    last_error = Some((StatusCode::BAD_GATEWAY, message.clone()));
                    if affects_account_health && attempt_idx + 1 < token_candidates.len() {
                        continue;
                    }
                    return codex_error_response(source_api, status, message);
                }
            }
        }

        selected_response = Some((resp, codex_context));
        break;
    }

    let Some((resp, codex_context)) = selected_response else {
        let (status, message) = last_error.unwrap_or_else(|| {
            (
                StatusCode::BAD_GATEWAY,
                "All Codex accounts failed".to_string(),
            )
        });
        let status = codex_error_status_from_message(&message, status);
        return codex_error_response(
            source_api,
            status,
            format!("All Codex accounts failed; last error: {}", message),
        );
    };

    let status = resp.status();
    let mut out_headers = HeaderMap::new();
    for (k, v) in resp.headers().iter() {
        let name = k.as_str().to_ascii_lowercase();
        if is_hop_header(&name) || name == "content-encoding" || name == "content-length" {
            continue;
        }
        out_headers.insert(k, v.clone());
    }

    if status.as_u16() >= 400 {
        record_codex_error(
            &state,
            &codex_context,
            format!("upstream status {}", status),
        );
        let body_bytes = match resp.bytes().await {
            Ok(b) => b,
            Err(err) => {
                error!("upstream error body read failed: {}", err);
                return if matches!(source_api, SourceApi::V1) {
                    (
                        StatusCode::BAD_GATEWAY,
                        [(
                            axum::http::header::CONTENT_TYPE.as_str(),
                            "application/json",
                        )],
                        openai_error_body("Upstream error", "server_error", None),
                    )
                        .into_response()
                } else {
                    (
                        StatusCode::BAD_GATEWAY,
                        "upstream error (failed to read body)",
                    )
                        .into_response()
                };
            }
        };
        return if matches!(source_api, SourceApi::V1) {
            let mut headers = out_headers;
            headers.insert(
                axum::http::header::CONTENT_TYPE,
                HeaderValue::from_static("application/json"),
            );
            (
                status,
                headers,
                upstream_error_to_openai(status, &body_bytes),
            )
                .into_response()
        } else {
            (status, out_headers, body_bytes).into_response()
        };
    }

    if matches!(routed.response_mode, ResponseMode::SseToJson) {
        let body_bytes = match resp.bytes().await {
            Ok(b) => b,
            Err(err) => {
                error!("upstream body read failed: {}", err);
                record_codex_error(&state, &codex_context, "failed to read upstream body");
                return (StatusCode::BAD_GATEWAY, "upstream error").into_response();
            }
        };
        if let Some(message) = sse_error_message(&body_bytes) {
            let status = codex_error_status_from_message(&message, StatusCode::BAD_GATEWAY);
            let affects_account_health = codex_error_affects_account_health(&message);
            record_codex_error_with_health(
                &state,
                &codex_context,
                &message,
                affects_account_health,
            );
            return codex_error_response(source_api, status, message);
        }
        let metrics = usage_metrics_from_sse_response_body(&body_bytes).unwrap_or_default();
        record_usage_success(&state, &codex_context, &metrics);
        let json_body = sse_to_response_json(&body_bytes);
        let mut headers = out_headers;
        headers.insert(
            axum::http::header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        return (status, headers, json_body).into_response();
    }

    if matches!(source_api, SourceApi::Codex) && method == Method::GET {
        if is_codex_models_list_path(&raw_path) {
            let body_bytes = match resp.bytes().await {
                Ok(b) => b,
                Err(err) => {
                    error!("upstream body read failed: {}", err);
                    return (StatusCode::BAD_GATEWAY, "upstream error").into_response();
                }
            };
            let body_bytes = augment_codex_models_json(&body_bytes, &state);
            let mut headers = out_headers;
            headers.insert(
                axum::http::header::CONTENT_TYPE,
                HeaderValue::from_static("application/json"),
            );
            return (status, headers, body_bytes).into_response();
        }
    }

    if matches!(source_api, SourceApi::V1) && method == Method::GET {
        if is_v1_models_list_path(&raw_path) || v1_model_retrieve_id(&raw_path).is_some() {
            let body_bytes = match resp.bytes().await {
                Ok(b) => b,
                Err(err) => {
                    error!("upstream body read failed: {}", err);
                    return (
                        StatusCode::BAD_GATEWAY,
                        [(
                            axum::http::header::CONTENT_TYPE.as_str(),
                            "application/json",
                        )],
                        openai_error_body("Upstream error", "server_error", None),
                    )
                        .into_response();
                }
            };
            let converted = if let Some(model_id) = v1_model_retrieve_id(&raw_path) {
                model_retrieve_to_openai_json(&body_bytes, &model_id).map_err(|e| {
                    if e.contains("does not exist") {
                        (StatusCode::NOT_FOUND, e)
                    } else {
                        (StatusCode::BAD_GATEWAY, e)
                    }
                })
            } else {
                models_list_to_openai_json(&body_bytes).map_err(|e| (StatusCode::BAD_GATEWAY, e))
            };
            return match converted {
                Ok(json_body) => {
                    let mut headers = out_headers;
                    headers.insert(
                        axum::http::header::CONTENT_TYPE,
                        HeaderValue::from_static("application/json"),
                    );
                    (status, headers, json_body).into_response()
                }
                Err((mapped_status, mapped_message)) => (
                    mapped_status,
                    [(
                        axum::http::header::CONTENT_TYPE.as_str(),
                        "application/json",
                    )],
                    openai_error_body(
                        &mapped_message,
                        if mapped_status == StatusCode::NOT_FOUND {
                            "invalid_request_error"
                        } else {
                            "server_error"
                        },
                        if mapped_status == StatusCode::NOT_FOUND {
                            Some("model_not_found")
                        } else {
                            None
                        },
                    ),
                )
                    .into_response(),
            };
        }
    }

    // Stream response body back
    let stats_state = state.clone();
    let stream_context = codex_context.clone();
    let content_type = out_headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.to_ascii_lowercase());
    let mut usage_tracker = CodexSseUsageTracker::new(stats_state.clone(), stream_context.clone());
    let stream = resp.bytes_stream().map(move |chunk| {
        if let Err(ref err) = chunk {
            error!("stream chunk error: {}", err);
            usage_tracker.fail("stream chunk error");
        }
        if let Ok(ref bytes) = chunk {
            usage_tracker.push(bytes);
        }
        chunk.map_err(|_| std::io::Error::new(std::io::ErrorKind::Other, "stream"))
    });
    let stream = if matches!(source_api, SourceApi::V1)
        && content_type
            .as_deref()
            .map(|value| value.contains("text/event-stream"))
            .unwrap_or(false)
    {
        compat_v1_sse_stream(stream).left_stream()
    } else {
        stream.right_stream()
    };
    let body = Body::from_stream(stream);
    if matches!(source_api, SourceApi::V1)
        && !out_headers.contains_key(axum::http::header::CONTENT_TYPE)
    {
        out_headers.insert(
            axum::http::header::CONTENT_TYPE,
            HeaderValue::from_static("text/event-stream"),
        );
    }
    (status, out_headers, body).into_response()
}

fn valid_content_length(headers: &HeaderMap) -> Option<u64> {
    let mut parsed = None;
    for value in headers.get_all(axum::http::header::CONTENT_LENGTH).iter() {
        let value = value.to_str().ok()?;
        for item in value.split(',') {
            let item = item.trim();
            if item.is_empty() || !item.bytes().all(|byte| byte.is_ascii_digit()) {
                return None;
            }
            let length = item.parse::<u64>().ok()?;
            if parsed.is_some_and(|previous| previous != length) {
                return None;
            }
            parsed = Some(length);
        }
    }
    parsed
}

fn is_length_limit_error(error: &(dyn std::error::Error + 'static)) -> bool {
    let mut current = Some(error);
    while let Some(error) = current {
        if error.is::<http_body_util::LengthLimitError>() {
            return true;
        }
        current = error.source();
    }
    false
}

fn oversized_request_body_response(
    source_api: SourceApi,
    limit: usize,
    content_length: Option<u64>,
) -> Response {
    let message = match content_length {
        Some(content_length) if content_length > limit as u64 => format!(
            "Request body is too large: Content-Length is {content_length} bytes, but the configured maximum is {limit} bytes. Reduce inline images or attachments, or continue in a clean session."
        ),
        Some(content_length) => format!(
            "Request body exceeded the configured maximum of {limit} bytes while streaming. Content-Length reported {content_length} bytes. Reduce inline images or attachments, or continue in a clean session."
        ),
        None => format!(
            "Request body is too large: it exceeded the configured maximum of {limit} bytes while streaming. Reduce inline images or attachments, or continue in a clean session."
        ),
    };

    if matches!(source_api, SourceApi::V1) {
        let mut details = serde_json::Map::new();
        details.insert("limit_bytes".to_string(), serde_json::json!(limit));
        if let Some(content_length) = content_length {
            details.insert(
                "content_length_bytes".to_string(),
                serde_json::json!(content_length),
            );
        }
        let body = serde_json::json!({
            "error": {
                "message": message,
                "type": "invalid_request_error",
                "param": serde_json::Value::Null,
                "code": "request_body_too_large",
                "details": details,
            }
        });
        (
            StatusCode::PAYLOAD_TOO_LARGE,
            [(
                axum::http::header::CONTENT_TYPE.as_str(),
                "application/json",
            )],
            Bytes::from(serde_json::to_vec(&body).unwrap_or_default()),
        )
            .into_response()
    } else {
        (StatusCode::PAYLOAD_TOO_LARGE, message).into_response()
    }
}

fn invalid_request_body_response(source_api: SourceApi) -> Response {
    if matches!(source_api, SourceApi::V1) {
        (
            StatusCode::BAD_REQUEST,
            [(
                axum::http::header::CONTENT_TYPE.as_str(),
                "application/json",
            )],
            openai_error_body("Invalid request body", "invalid_request_error", None),
        )
            .into_response()
    } else {
        (StatusCode::BAD_REQUEST, "invalid body").into_response()
    }
}

#[cfg(test)]
mod request_body_limit_tests {
    use super::{
        invalid_request_body_response, is_length_limit_error, oversized_request_body_response,
        valid_content_length, SourceApi,
    };
    use axum::{
        body::{to_bytes, Body},
        http::{header::CONTENT_LENGTH, HeaderMap, HeaderValue, StatusCode},
    };

    #[test]
    fn content_length_is_used_only_when_valid_and_unambiguous() {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_LENGTH, HeaderValue::from_static("16777217"));
        assert_eq!(valid_content_length(&headers), Some(16_777_217));

        headers.insert(CONTENT_LENGTH, HeaderValue::from_static("42, 42"));
        assert_eq!(valid_content_length(&headers), Some(42));

        headers.insert(CONTENT_LENGTH, HeaderValue::from_static("42, 43"));
        assert_eq!(valid_content_length(&headers), None);

        headers.insert(CONTENT_LENGTH, HeaderValue::from_static("unknown"));
        assert_eq!(valid_content_length(&headers), None);
    }

    #[tokio::test]
    async fn streaming_limit_accepts_the_boundary_and_classifies_only_overflow() {
        let accepted = to_bytes(Body::from(vec![0_u8; 8]), 8).await.unwrap();
        assert_eq!(accepted.len(), 8);

        let overflow = to_bytes(Body::from(vec![0_u8; 9]), 8).await.unwrap_err();
        assert!(is_length_limit_error(&overflow));

        let unrelated = std::io::Error::new(std::io::ErrorKind::Other, "body read failed");
        assert!(!is_length_limit_error(&unrelated));
    }

    #[tokio::test]
    async fn v1_oversized_response_is_structured_and_actionable() {
        let response = oversized_request_body_response(SourceApi::V1, 16, Some(17));
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();

        assert_eq!(value["error"]["code"], "request_body_too_large");
        assert_eq!(value["error"]["details"]["limit_bytes"], 16);
        assert_eq!(value["error"]["details"]["content_length_bytes"], 17);
        assert!(value["error"]["message"]
            .as_str()
            .unwrap()
            .contains("continue in a clean session"));
    }

    #[tokio::test]
    async fn codex_oversized_response_is_actionable_text() {
        let response = oversized_request_body_response(SourceApi::Codex, 16, None);
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();

        assert!(text.contains("maximum of 16 bytes"));
        assert!(text.contains("continue in a clean session"));
    }

    #[test]
    fn unrelated_body_read_errors_remain_bad_request() {
        assert_eq!(
            invalid_request_body_response(SourceApi::V1).status(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            invalid_request_body_response(SourceApi::Codex).status(),
            StatusCode::BAD_REQUEST
        );
    }
}

fn codex_live_stream_response(
    state: AppState,
    context: UsageContext,
    source_api: SourceApi,
    status: StatusCode,
    mut out_headers: HeaderMap,
    prefix: Bytes,
    upstream: ReqwestByteStream,
) -> Response {
    let content_type = out_headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.to_ascii_lowercase());
    let mut usage_tracker = CodexSseUsageTracker::new(state, context);
    let combined = stream::once(async move { Ok::<Bytes, reqwest::Error>(prefix) }).chain(upstream);
    let tracked = combined.map(move |chunk| {
        match &chunk {
            Ok(bytes) => usage_tracker.push(bytes),
            Err(err) => {
                error!("stream chunk error: {}", err);
                usage_tracker.fail("stream chunk error");
            }
        }
        chunk.map_err(|_| std::io::Error::new(std::io::ErrorKind::Other, "stream"))
    });
    let tracked: Pin<Box<dyn Stream<Item = Result<Bytes, std::io::Error>> + Send + 'static>> =
        Box::pin(tracked);
    let stream = if matches!(source_api, SourceApi::V1)
        && content_type
            .as_deref()
            .is_some_and(|value| value.contains("text/event-stream"))
    {
        compat_v1_sse_stream(tracked).left_stream()
    } else {
        tracked.right_stream()
    };
    if matches!(source_api, SourceApi::V1)
        && !out_headers.contains_key(axum::http::header::CONTENT_TYPE)
    {
        out_headers.insert(
            axum::http::header::CONTENT_TYPE,
            HeaderValue::from_static("text/event-stream"),
        );
    }
    (status, out_headers, Body::from_stream(stream)).into_response()
}

async fn custom_model_response(
    state: AppState,
    headers: HeaderMap,
    upstream_path: &str,
    body: Bytes,
    response_mode: ResponseMode,
    access: &api_keys::ApiKeyAccess,
    prompt: PromptMetrics,
) -> axum::response::Response {
    if upstream_path != "responses" {
        return (
            StatusCode::BAD_REQUEST,
            [("Content-Type", "application/json")],
            openai_error_body(
                "custom models currently support /responses requests",
                "invalid_request_error",
                None,
            ),
        )
            .into_response();
    }

    let request_value: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(value) => value,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                [("Content-Type", "application/json")],
                openai_error_body("Invalid request body", "invalid_request_error", None),
            )
                .into_response();
        }
    };
    let alias = request_value
        .get("model")
        .and_then(|value| value.as_str())
        .map(custom_models::normalize_alias)
        .unwrap_or_default();
    if alias.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            [("Content-Type", "application/json")],
            openai_error_body(
                "custom model alias is required",
                "invalid_request_error",
                None,
            ),
        )
            .into_response();
    }

    let Some(custom_model) = find_custom_model(&state, &alias) else {
        return (
            StatusCode::NOT_FOUND,
            [("Content-Type", "application/json")],
            openai_error_body(
                &format!("The custom model '{}' does not exist", alias),
                "invalid_request_error",
                Some("model_not_found"),
            ),
        )
            .into_response();
    };
    if !custom_model.enabled {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            [("Content-Type", "application/json")],
            openai_error_body(
                &format!("The custom model '{}' is disabled", alias),
                "server_error",
                None,
            ),
        )
            .into_response();
    }

    let prompt_tokens = prompt.estimated_input_tokens();
    let candidates = custom_model_candidate_order(&state, &custom_model)
        .into_iter()
        .filter(|candidate| {
            target_provider_access_key(source::v1::provider::target_from_model(&candidate.model))
                .is_some_and(|provider| {
                    access.allows_provider(provider)
                        && api_key_prompt_limit_violation(
                            access,
                            Some(provider),
                            None,
                            prompt_tokens,
                        )
                        .is_none()
                })
        })
        .collect::<Vec<_>>();
    if candidates.is_empty() {
        return (
            StatusCode::FORBIDDEN,
            [("Content-Type", "application/json")],
            openai_error_body(
                &format!(
                    "API key does not have access to any target for custom model '{}'",
                    alias
                ),
                "permission_error",
                None,
            ),
        )
            .into_response();
    }

    let mut failures = Vec::new();
    for (idx, candidate) in candidates.iter().enumerate() {
        let is_last = idx + 1 == candidates.len();
        let target = source::v1::provider::target_from_model(&candidate.model);
        if matches!(
            target,
            TargetModel::Custom | TargetModel::CodexModels | TargetModel::UnifiedV1Models
        ) {
            failures.push(format!("{}: unsupported target", candidate.model));
            continue;
        }

        let candidate_label = custom_models::target_label(candidate);
        let candidate_body = match rewrite_request_model(&body, &candidate.model) {
            Ok(body) => body,
            Err(err) => {
                failures.push(format!("{}: {}", candidate_label, err));
                continue;
            }
        };
        let scoped_state =
            match scoped_state_for_custom_target_account(state.clone(), target, candidate) {
                Ok(state) => state,
                Err(err) => {
                    failures.push(format!("{}: {}", candidate_label, err));
                    continue;
                }
            };
        let scoped_state = scoped_state_for_api_key_request(
            scoped_state,
            access,
            Some(target),
            Some(prompt_tokens),
        );
        let response = dispatch_custom_target(
            scoped_state,
            headers.clone(),
            upstream_path,
            target,
            candidate_body,
            response_mode,
        )
        .await;
        let status = response.status();
        if status.is_success() || !should_custom_model_fallback(status) || is_last {
            return response;
        }

        let failure_body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .ok()
            .and_then(|bytes| String::from_utf8(bytes.to_vec()).ok())
            .unwrap_or_default();
        failures.push(format!(
            "{} returned {}{}",
            candidate_label,
            status,
            if failure_body.trim().is_empty() {
                String::new()
            } else {
                format!(": {}", failure_body.trim())
            }
        ));
    }

    (
        StatusCode::BAD_GATEWAY,
        [("Content-Type", "application/json")],
        openai_error_body(
            &format!(
                "All targets failed for custom model '{}': {}",
                alias,
                failures.join(" | ")
            ),
            "server_error",
            None,
        ),
    )
        .into_response()
}

fn find_custom_model(state: &AppState, alias: &str) -> Option<custom_models::CustomModel> {
    let alias = custom_models::normalize_alias(alias);
    state
        .custom_models
        .lock()
        .unwrap()
        .iter()
        .find(|model| model.alias.eq_ignore_ascii_case(&alias))
        .cloned()
}

fn custom_model_candidate_order(
    state: &AppState,
    model: &custom_models::CustomModel,
) -> Vec<custom_models::CustomModelTarget> {
    let mut out = Vec::new();
    for (group_idx, group) in model.routes.iter().enumerate() {
        let mut targets = group
            .targets
            .iter()
            .filter(|target| target.enabled)
            .cloned()
            .collect::<Vec<_>>();
        if targets.len() > 1 {
            let rr_key = format!("{}:{}", model.alias, group_idx);
            let start_idx = {
                let mut rr = state.custom_model_rr.lock().unwrap();
                let len = targets.len();
                let value = rr.entry(rr_key.clone()).or_insert(0);
                *value %= len;
                *value
            };
            targets.sort_by(|left, right| {
                let left_score = custom_model_target_score(state, left);
                let right_score = custom_model_target_score(state, right);
                if left_score.is_better_than(&right_score) {
                    std::cmp::Ordering::Less
                } else if right_score.is_better_than(&left_score) {
                    std::cmp::Ordering::Greater
                } else {
                    std::cmp::Ordering::Equal
                }
            });
            if start_idx > 0 && start_idx < targets.len() {
                targets.rotate_left(start_idx);
            }
            let mut rr = state.custom_model_rr.lock().unwrap();
            rr.insert(rr_key, (start_idx + 1) % targets.len());
        }
        out.extend(targets);
    }
    out
}

fn custom_model_target_score(
    state: &AppState,
    target: &custom_models::CustomModelTarget,
) -> AccountSelectionScore {
    let mut score = match target.account.as_deref() {
        Some(account) if !account.trim().is_empty() => match target.account_condition {
            custom_models::CustomModelAccountCondition::Only => {
                best_provider_score_for_model_account(state, &target.model, account)
            }
            custom_models::CustomModelAccountCondition::Except => {
                best_provider_score_for_model_except_account(state, &target.model, account)
            }
        },
        _ => best_provider_score_for_model(state, &target.model),
    };
    let weight = u64::from(target.weight.max(1));
    score.active_requests /= weight;
    score.recent_requests /= weight;
    score
}

fn custom_account_condition_matches(
    is_match: bool,
    condition: custom_models::CustomModelAccountCondition,
) -> bool {
    match condition {
        custom_models::CustomModelAccountCondition::Only => is_match,
        custom_models::CustomModelAccountCondition::Except => !is_match,
    }
}

fn custom_account_condition_error(
    provider: &str,
    account_filter: &str,
    condition: custom_models::CustomModelAccountCondition,
) -> String {
    match condition {
        custom_models::CustomModelAccountCondition::Only => {
            format!("no {} account matched '{}'", provider, account_filter)
        }
        custom_models::CustomModelAccountCondition::Except => {
            format!(
                "no {} account remained after excluding '{}'",
                provider, account_filter
            )
        }
    }
}

fn best_provider_score_for_model_account(
    state: &AppState,
    model: &str,
    account_filter: &str,
) -> AccountSelectionScore {
    match source::v1::provider::target_from_model(model) {
        TargetModel::Codex => {
            let tokens = state.tokens.lock().unwrap().clone();
            best_score_for_len(
                tokens.len(),
                |idx| {
                    tokens[idx].enabled && codex_token_matches_account(&tokens[idx], account_filter)
                },
                |idx| codex_token_selection_score(state, idx, &tokens[idx]),
            )
        }
        TargetModel::Antigravity => {
            let accounts = state.agw_accounts.lock().unwrap().clone();
            best_score_for_len(
                accounts.len(),
                |idx| {
                    accounts[idx].enabled
                        && antigravity_account_matches(&accounts[idx], account_filter)
                },
                |idx| antigravity_account_selection_score(state, &accounts[idx]),
            )
        }
        TargetModel::Gemini => {
            let accounts = state.gemini_accounts.lock().unwrap().clone();
            best_score_for_len(
                accounts.len(),
                |idx| {
                    accounts[idx].enabled && gemini_account_matches(&accounts[idx], account_filter)
                },
                |idx| gemini_account_selection_score(state, &accounts[idx]),
            )
        }
        TargetModel::Qwen => {
            let accounts = state.qwen_accounts.lock().unwrap().clone();
            best_score_for_len(
                accounts.len(),
                |idx| accounts[idx].enabled && qwen_account_matches(&accounts[idx], account_filter),
                |idx| qwen_account_selection_score(state, &accounts[idx]),
            )
        }
        TargetModel::DeepSeek => {
            let accounts = state.deepseek_accounts.lock().unwrap().clone();
            best_score_for_len(
                accounts.len(),
                |idx| {
                    accounts[idx].enabled
                        && deepseek_account_matches(&accounts[idx], account_filter)
                },
                |idx| deepseek_account_selection_score(state, &accounts[idx]),
            )
        }
        TargetModel::Grok => {
            let accounts = state.grok_accounts.lock().unwrap().clone();
            best_score_for_len(
                accounts.len(),
                |idx| accounts[idx].enabled && grok_account_matches(&accounts[idx], account_filter),
                |idx| grok_account_selection_score(state, &accounts[idx]),
            )
        }
        TargetModel::MiniMax => {
            let accounts = state.minimax_accounts.lock().unwrap().clone();
            best_score_for_len(
                accounts.len(),
                |idx| {
                    accounts[idx].enabled && minimax_account_matches(&accounts[idx], account_filter)
                },
                |idx| minimax_account_selection_score(state, &accounts[idx]),
            )
        }
        TargetModel::Copilot => {
            let accounts = state.copilot_accounts.lock().unwrap().clone();
            best_score_for_len(
                accounts.len(),
                |idx| {
                    accounts[idx].enabled && copilot_account_matches(&accounts[idx], account_filter)
                },
                |idx| copilot_account_selection_score(state, &accounts[idx]),
            )
        }
        TargetModel::Claude => {
            let accounts = state.claude_accounts.lock().unwrap().clone();
            best_score_for_len(
                accounts.len(),
                |idx| {
                    accounts[idx].enabled && claude_account_matches(&accounts[idx], account_filter)
                },
                |idx| claude_account_selection_score(state, &accounts[idx]),
            )
        }
        TargetModel::Glm => {
            let accounts = state.glm_accounts.lock().unwrap().clone();
            best_score_for_len(
                accounts.len(),
                |idx| accounts[idx].enabled && glm_account_matches(&accounts[idx], account_filter),
                |idx| glm_account_selection_score(state, &accounts[idx]),
            )
        }
        TargetModel::Custom | TargetModel::CodexModels | TargetModel::UnifiedV1Models => {
            AccountSelectionScore {
                quota_pressure: Some(f64::INFINITY),
                active_requests: u64::MAX,
                recent_requests: u64::MAX,
            }
        }
    }
}

fn best_provider_score_for_model_except_account(
    state: &AppState,
    model: &str,
    account_filter: &str,
) -> AccountSelectionScore {
    match source::v1::provider::target_from_model(model) {
        TargetModel::Codex => {
            let tokens = state.tokens.lock().unwrap().clone();
            best_score_for_len(
                tokens.len(),
                |idx| {
                    tokens[idx].enabled
                        && !codex_token_matches_account(&tokens[idx], account_filter)
                },
                |idx| codex_token_selection_score(state, idx, &tokens[idx]),
            )
        }
        TargetModel::Antigravity => {
            let accounts = state.agw_accounts.lock().unwrap().clone();
            best_score_for_len(
                accounts.len(),
                |idx| {
                    accounts[idx].enabled
                        && !antigravity_account_matches(&accounts[idx], account_filter)
                },
                |idx| antigravity_account_selection_score(state, &accounts[idx]),
            )
        }
        TargetModel::Gemini => {
            let accounts = state.gemini_accounts.lock().unwrap().clone();
            best_score_for_len(
                accounts.len(),
                |idx| {
                    accounts[idx].enabled && !gemini_account_matches(&accounts[idx], account_filter)
                },
                |idx| gemini_account_selection_score(state, &accounts[idx]),
            )
        }
        TargetModel::Qwen => {
            let accounts = state.qwen_accounts.lock().unwrap().clone();
            best_score_for_len(
                accounts.len(),
                |idx| {
                    accounts[idx].enabled && !qwen_account_matches(&accounts[idx], account_filter)
                },
                |idx| qwen_account_selection_score(state, &accounts[idx]),
            )
        }
        TargetModel::DeepSeek => {
            let accounts = state.deepseek_accounts.lock().unwrap().clone();
            best_score_for_len(
                accounts.len(),
                |idx| {
                    accounts[idx].enabled
                        && !deepseek_account_matches(&accounts[idx], account_filter)
                },
                |idx| deepseek_account_selection_score(state, &accounts[idx]),
            )
        }
        TargetModel::Grok => {
            let accounts = state.grok_accounts.lock().unwrap().clone();
            best_score_for_len(
                accounts.len(),
                |idx| {
                    accounts[idx].enabled && !grok_account_matches(&accounts[idx], account_filter)
                },
                |idx| grok_account_selection_score(state, &accounts[idx]),
            )
        }
        TargetModel::MiniMax => {
            let accounts = state.minimax_accounts.lock().unwrap().clone();
            best_score_for_len(
                accounts.len(),
                |idx| {
                    accounts[idx].enabled
                        && !minimax_account_matches(&accounts[idx], account_filter)
                },
                |idx| minimax_account_selection_score(state, &accounts[idx]),
            )
        }
        TargetModel::Copilot => {
            let accounts = state.copilot_accounts.lock().unwrap().clone();
            best_score_for_len(
                accounts.len(),
                |idx| {
                    accounts[idx].enabled
                        && !copilot_account_matches(&accounts[idx], account_filter)
                },
                |idx| copilot_account_selection_score(state, &accounts[idx]),
            )
        }
        TargetModel::Claude => {
            let accounts = state.claude_accounts.lock().unwrap().clone();
            best_score_for_len(
                accounts.len(),
                |idx| {
                    accounts[idx].enabled && !claude_account_matches(&accounts[idx], account_filter)
                },
                |idx| claude_account_selection_score(state, &accounts[idx]),
            )
        }
        TargetModel::Glm => {
            let accounts = state.glm_accounts.lock().unwrap().clone();
            best_score_for_len(
                accounts.len(),
                |idx| accounts[idx].enabled && !glm_account_matches(&accounts[idx], account_filter),
                |idx| glm_account_selection_score(state, &accounts[idx]),
            )
        }
        TargetModel::Custom | TargetModel::CodexModels | TargetModel::UnifiedV1Models => {
            AccountSelectionScore {
                quota_pressure: Some(f64::INFINITY),
                active_requests: u64::MAX,
                recent_requests: u64::MAX,
            }
        }
    }
}

fn best_provider_score_for_model(state: &AppState, model: &str) -> AccountSelectionScore {
    match source::v1::provider::target_from_model(model) {
        TargetModel::Codex => {
            let tokens = state.tokens.lock().unwrap().clone();
            best_score_for_len(
                tokens.len(),
                |idx| tokens[idx].enabled,
                |idx| codex_token_selection_score(state, idx, &tokens[idx]),
            )
        }
        TargetModel::Antigravity => {
            let accounts = state.agw_accounts.lock().unwrap().clone();
            best_score_for_len(
                accounts.len(),
                |idx| accounts[idx].enabled,
                |idx| antigravity_account_selection_score(state, &accounts[idx]),
            )
        }
        TargetModel::Gemini => {
            let accounts = state.gemini_accounts.lock().unwrap().clone();
            best_score_for_len(
                accounts.len(),
                |idx| accounts[idx].enabled,
                |idx| gemini_account_selection_score(state, &accounts[idx]),
            )
        }
        TargetModel::Qwen => {
            let accounts = state.qwen_accounts.lock().unwrap().clone();
            best_score_for_len(
                accounts.len(),
                |idx| accounts[idx].enabled,
                |idx| qwen_account_selection_score(state, &accounts[idx]),
            )
        }
        TargetModel::DeepSeek => {
            let accounts = state.deepseek_accounts.lock().unwrap().clone();
            best_score_for_len(
                accounts.len(),
                |idx| accounts[idx].enabled,
                |idx| deepseek_account_selection_score(state, &accounts[idx]),
            )
        }
        TargetModel::Grok => {
            let accounts = state.grok_accounts.lock().unwrap().clone();
            best_score_for_len(
                accounts.len(),
                |idx| accounts[idx].enabled,
                |idx| grok_account_selection_score(state, &accounts[idx]),
            )
        }
        TargetModel::MiniMax => {
            let accounts = state.minimax_accounts.lock().unwrap().clone();
            best_score_for_len(
                accounts.len(),
                |idx| accounts[idx].enabled,
                |idx| minimax_account_selection_score(state, &accounts[idx]),
            )
        }
        TargetModel::Copilot => {
            let accounts = state.copilot_accounts.lock().unwrap().clone();
            best_score_for_len(
                accounts.len(),
                |idx| accounts[idx].enabled,
                |idx| copilot_account_selection_score(state, &accounts[idx]),
            )
        }
        TargetModel::Claude => {
            let accounts = state.claude_accounts.lock().unwrap().clone();
            best_score_for_len(
                accounts.len(),
                |idx| accounts[idx].enabled,
                |idx| claude_account_selection_score(state, &accounts[idx]),
            )
        }
        TargetModel::Glm => {
            let accounts = state.glm_accounts.lock().unwrap().clone();
            best_score_for_len(
                accounts.len(),
                |idx| accounts[idx].enabled,
                |idx| glm_account_selection_score(state, &accounts[idx]),
            )
        }
        TargetModel::Custom | TargetModel::CodexModels | TargetModel::UnifiedV1Models => {
            AccountSelectionScore {
                quota_pressure: Some(f64::INFINITY),
                active_requests: u64::MAX,
                recent_requests: u64::MAX,
            }
        }
    }
}

fn account_filter_matches(filter: &str, values: impl IntoIterator<Item = String>) -> bool {
    let filter = filter.trim();
    !filter.is_empty()
        && values
            .into_iter()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .any(|value| value.eq_ignore_ascii_case(filter))
}

fn codex_token_matches_account(token: &UpstreamToken, filter: &str) -> bool {
    account_filter_matches(
        filter,
        [
            codex_stats_key(token),
            token.account_id.clone().unwrap_or_default(),
            token.label.clone(),
            token.file_name.clone().unwrap_or_default(),
        ],
    )
}

fn antigravity_account_matches(
    account: &target::antigravity::accounts::AntigravityAccount,
    filter: &str,
) -> bool {
    account_filter_matches(
        filter,
        [
            antigravity_stats_key(account),
            account.email.clone(),
            account.label.clone(),
            account.project_id.clone().unwrap_or_default(),
            account.file_name.clone().unwrap_or_default(),
        ],
    )
}

fn gemini_account_matches(account: &target::gemini::accounts::GeminiAccount, filter: &str) -> bool {
    account_filter_matches(
        filter,
        [
            gemini_stats_key(account),
            account.email.clone(),
            account.label.clone(),
            account.project_id.clone().unwrap_or_default(),
            account.file_name.clone().unwrap_or_default(),
        ],
    )
}

fn qwen_account_matches(account: &target::qwen::accounts::QwenAccount, filter: &str) -> bool {
    account_filter_matches(
        filter,
        [
            qwen_stats_key(account),
            account.account_id.clone(),
            account.email.clone(),
            account.subject.clone().unwrap_or_default(),
            account.label.clone(),
            account.resource_url.clone().unwrap_or_default(),
            account.file_name.clone().unwrap_or_default(),
        ],
    )
}

fn deepseek_account_matches(
    account: &target::deepseek::accounts::DeepSeekAccount,
    filter: &str,
) -> bool {
    account_filter_matches(
        filter,
        [
            deepseek_stats_key(account),
            account.account_id.clone(),
            account.label.clone(),
            account.file_name.clone().unwrap_or_default(),
        ],
    )
}

fn grok_account_matches(account: &target::grok::accounts::GrokAccount, filter: &str) -> bool {
    account_filter_matches(
        filter,
        [
            grok_stats_key(account),
            account.label.clone(),
            account.name.clone().unwrap_or_default(),
            account.email.clone().unwrap_or_default(),
            account.user_id.clone().unwrap_or_default(),
            account.team_id.clone().unwrap_or_default(),
            account.file_name.clone().unwrap_or_default(),
        ],
    )
}

fn minimax_account_matches(
    account: &target::minimax::accounts::MiniMaxAccount,
    filter: &str,
) -> bool {
    account_filter_matches(
        filter,
        [
            minimax_stats_key(account),
            account.account_id.clone(),
            account.label.clone(),
            account.file_name.clone().unwrap_or_default(),
        ],
    )
}

fn copilot_account_matches(
    account: &target::copilot::accounts::CopilotAccount,
    filter: &str,
) -> bool {
    account_filter_matches(
        filter,
        [
            copilot_stats_key(account),
            account.account_id.clone(),
            account.login.clone(),
            account.label.clone(),
            account.file_name.clone().unwrap_or_default(),
        ],
    )
}

fn claude_account_matches(account: &target::claude::accounts::ClaudeAccount, filter: &str) -> bool {
    account_filter_matches(
        filter,
        [
            claude_stats_key(account),
            account.organization_uuid.clone(),
            account.account_id.clone(),
            account.label.clone(),
            account.email.clone().unwrap_or_default(),
            account.file_name.clone().unwrap_or_default(),
        ],
    )
}

fn glm_account_matches(account: &target::glm::accounts::GlmAccount, filter: &str) -> bool {
    account_filter_matches(
        filter,
        [
            glm_stats_key(account),
            account.account_id.clone(),
            account.label.clone(),
            account.account_type.clone(),
            account.file_name.clone().unwrap_or_default(),
        ],
    )
}

fn target_provider_access_key(target: TargetModel) -> Option<&'static str> {
    match target {
        TargetModel::Codex | TargetModel::CodexModels => Some("codex"),
        TargetModel::Antigravity => Some("agw"),
        TargetModel::Gemini => Some("gemini"),
        TargetModel::Qwen => Some("qwen"),
        TargetModel::DeepSeek => Some("deepseek"),
        TargetModel::Grok => Some("grok"),
        TargetModel::MiniMax => Some("minimax"),
        TargetModel::Copilot => Some("copilot"),
        TargetModel::Claude => Some("claude"),
        TargetModel::Glm => Some("glm"),
        TargetModel::Custom | TargetModel::UnifiedV1Models => None,
    }
}

fn api_key_rule_allows_account<F>(
    access: &api_keys::ApiKeyAccess,
    provider: &str,
    mut matches: F,
) -> bool
where
    F: FnMut(&str) -> bool,
{
    if access.all {
        return true;
    }
    access.provider_rule(provider).is_some_and(|rule| {
        rule.accounts.is_empty() || rule.accounts.iter().any(|account| matches(account))
    })
}

fn api_key_prompt_limit_response(
    source_api: SourceApi,
    access: &api_keys::ApiKeyAccess,
    provider: Option<&str>,
    account_limit: Option<u64>,
    estimated_tokens: u64,
) -> Option<Response> {
    let message =
        api_key_prompt_limit_violation(access, provider, account_limit, estimated_tokens)?;
    Some(if matches!(source_api, SourceApi::V1) {
        (
            StatusCode::TOO_MANY_REQUESTS,
            [(
                axum::http::header::CONTENT_TYPE.as_str(),
                "application/json",
            )],
            openai_error_body(&message, "rate_limit_error", Some("token_limit_exceeded")),
        )
            .into_response()
    } else {
        (StatusCode::TOO_MANY_REQUESTS, message).into_response()
    })
}

fn api_key_prompt_limit_violation(
    access: &api_keys::ApiKeyAccess,
    provider: Option<&str>,
    account_limit: Option<u64>,
    estimated_tokens: u64,
) -> Option<String> {
    if estimated_tokens == 0 {
        return None;
    }

    let mut limit = access.prompt_token_limit;
    if let Some(provider) = provider {
        if let Some(rule) = access.provider_rule(provider) {
            limit = min_optional_u64(limit, rule.prompt_token_limit);
        }
    }
    limit = min_optional_u64(limit, account_limit);

    let limit = limit?;
    if estimated_tokens <= limit {
        return None;
    }

    let scope = match (provider, account_limit) {
        (Some(provider), Some(_)) => format!("{} account", provider),
        (Some(provider), None) => format!("{} provider", provider),
        (None, _) => "API key".to_string(),
    };
    Some(format!(
        "{} prompt token limit exceeded: estimated {} input tokens, limit {}",
        scope, estimated_tokens, limit
    ))
}

fn api_key_account_prompt_limit_allows<F>(
    access: &api_keys::ApiKeyAccess,
    provider: &str,
    mut matches: F,
    estimated_tokens: Option<u64>,
) -> bool
where
    F: FnMut(&str) -> bool,
{
    let Some(estimated_tokens) = estimated_tokens else {
        return true;
    };
    if estimated_tokens == 0 {
        return true;
    }
    let account_limit = access
        .provider_rule(provider)
        .and_then(|rule| rule.account_prompt_token_limit(|account| matches(account)));
    api_key_prompt_limit_violation(access, Some(provider), account_limit, estimated_tokens)
        .is_none()
}

fn api_key_account_prompt_limit_response_for_target(
    source_api: SourceApi,
    state: &AppState,
    access: &api_keys::ApiKeyAccess,
    target: TargetModel,
    estimated_tokens: u64,
) -> Option<Response> {
    let provider = target_provider_access_key(target)?;
    let account_limit = match target {
        TargetModel::Codex => {
            let accounts = state.tokens.lock().unwrap();
            api_key_account_prompt_limit_blocked(
                &accounts,
                access,
                provider,
                estimated_tokens,
                |token, filter| codex_token_matches_account(token, filter),
            )
        }
        TargetModel::Antigravity => {
            let accounts = state.agw_accounts.lock().unwrap();
            api_key_account_prompt_limit_blocked(
                &accounts,
                access,
                provider,
                estimated_tokens,
                |account, filter| antigravity_account_matches(account, filter),
            )
        }
        TargetModel::Gemini => {
            let accounts = state.gemini_accounts.lock().unwrap();
            api_key_account_prompt_limit_blocked(
                &accounts,
                access,
                provider,
                estimated_tokens,
                |account, filter| gemini_account_matches(account, filter),
            )
        }
        TargetModel::Qwen => {
            let accounts = state.qwen_accounts.lock().unwrap();
            api_key_account_prompt_limit_blocked(
                &accounts,
                access,
                provider,
                estimated_tokens,
                |account, filter| qwen_account_matches(account, filter),
            )
        }
        TargetModel::DeepSeek => {
            let accounts = state.deepseek_accounts.lock().unwrap();
            api_key_account_prompt_limit_blocked(
                &accounts,
                access,
                provider,
                estimated_tokens,
                |account, filter| deepseek_account_matches(account, filter),
            )
        }
        TargetModel::Grok => {
            let accounts = state.grok_accounts.lock().unwrap();
            api_key_account_prompt_limit_blocked(
                &accounts,
                access,
                provider,
                estimated_tokens,
                |account, filter| grok_account_matches(account, filter),
            )
        }
        TargetModel::MiniMax => {
            let accounts = state.minimax_accounts.lock().unwrap();
            api_key_account_prompt_limit_blocked(
                &accounts,
                access,
                provider,
                estimated_tokens,
                |account, filter| minimax_account_matches(account, filter),
            )
        }
        TargetModel::Copilot => {
            let accounts = state.copilot_accounts.lock().unwrap();
            api_key_account_prompt_limit_blocked(
                &accounts,
                access,
                provider,
                estimated_tokens,
                |account, filter| copilot_account_matches(account, filter),
            )
        }
        TargetModel::Claude => {
            let accounts = state.claude_accounts.lock().unwrap();
            api_key_account_prompt_limit_blocked(
                &accounts,
                access,
                provider,
                estimated_tokens,
                |account, filter| claude_account_matches(account, filter),
            )
        }
        TargetModel::Glm => {
            let accounts = state.glm_accounts.lock().unwrap();
            api_key_account_prompt_limit_blocked(
                &accounts,
                access,
                provider,
                estimated_tokens,
                |account, filter| glm_account_matches(account, filter),
            )
        }
        TargetModel::CodexModels | TargetModel::Custom | TargetModel::UnifiedV1Models => None,
    }?;

    api_key_prompt_limit_response(
        source_api,
        access,
        Some(provider),
        Some(account_limit),
        estimated_tokens,
    )
}

fn api_key_account_prompt_limit_blocked<T, F>(
    accounts: &[T],
    access: &api_keys::ApiKeyAccess,
    provider: &str,
    estimated_tokens: u64,
    mut matches: F,
) -> Option<u64>
where
    F: FnMut(&T, &str) -> bool,
{
    if estimated_tokens == 0 {
        return None;
    }
    let rule = access.provider_rule(provider)?;
    if rule.account_limits.is_empty() {
        return None;
    }

    let mut saw_allowed_account = false;
    let mut strictest_blocked_limit = None;
    for account in accounts {
        if !api_key_rule_allows_account(access, provider, |filter| matches(account, filter)) {
            continue;
        }
        saw_allowed_account = true;
        let account_limit = rule.account_prompt_token_limit(|filter| matches(account, filter));
        if api_key_prompt_limit_violation(access, Some(provider), account_limit, estimated_tokens)
            .is_none()
        {
            return None;
        }
        strictest_blocked_limit = min_optional_u64(strictest_blocked_limit, account_limit);
    }

    if saw_allowed_account {
        strictest_blocked_limit
    } else {
        None
    }
}

fn min_optional_u64(left: Option<u64>, right: Option<u64>) -> Option<u64> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left.min(right)),
        (Some(left), None) => Some(left),
        (None, Some(right)) => Some(right),
        (None, None) => None,
    }
}

fn scoped_state_for_api_key_access(
    state: AppState,
    access: &api_keys::ApiKeyAccess,
    target: Option<TargetModel>,
) -> AppState {
    scoped_state_for_api_key_request(state, access, target, None)
}

fn scoped_state_for_api_key_request(
    state: AppState,
    access: &api_keys::ApiKeyAccess,
    target: Option<TargetModel>,
    estimated_prompt_tokens: Option<u64>,
) -> AppState {
    if access.all {
        if estimated_prompt_tokens.is_none() {
            return state;
        }
    }

    let mut scoped = state.clone();
    let all_providers = target.is_none();
    if all_providers || target == Some(TargetModel::Codex) {
        scoped.tokens = Arc::new(Mutex::new(
            state
                .tokens
                .lock()
                .unwrap()
                .iter()
                .filter(|token| {
                    api_key_rule_allows_account(access, "codex", |account| {
                        codex_token_matches_account(token, account)
                    }) && api_key_account_prompt_limit_allows(
                        access,
                        "codex",
                        |account| codex_token_matches_account(token, account),
                        estimated_prompt_tokens,
                    )
                })
                .cloned()
                .collect(),
        ));
    }
    if all_providers || target == Some(TargetModel::Antigravity) {
        scoped.agw_accounts = Arc::new(Mutex::new(
            state
                .agw_accounts
                .lock()
                .unwrap()
                .iter()
                .filter(|account| {
                    api_key_rule_allows_account(access, "agw", |filter| {
                        antigravity_account_matches(account, filter)
                    }) && api_key_account_prompt_limit_allows(
                        access,
                        "agw",
                        |filter| antigravity_account_matches(account, filter),
                        estimated_prompt_tokens,
                    )
                })
                .cloned()
                .collect(),
        ));
    }
    if all_providers || target == Some(TargetModel::Gemini) {
        scoped.gemini_accounts = Arc::new(Mutex::new(
            state
                .gemini_accounts
                .lock()
                .unwrap()
                .iter()
                .filter(|account| {
                    api_key_rule_allows_account(access, "gemini", |filter| {
                        gemini_account_matches(account, filter)
                    }) && api_key_account_prompt_limit_allows(
                        access,
                        "gemini",
                        |filter| gemini_account_matches(account, filter),
                        estimated_prompt_tokens,
                    )
                })
                .cloned()
                .collect(),
        ));
    }
    if all_providers || target == Some(TargetModel::Qwen) {
        scoped.qwen_accounts = Arc::new(Mutex::new(
            state
                .qwen_accounts
                .lock()
                .unwrap()
                .iter()
                .filter(|account| {
                    api_key_rule_allows_account(access, "qwen", |filter| {
                        qwen_account_matches(account, filter)
                    }) && api_key_account_prompt_limit_allows(
                        access,
                        "qwen",
                        |filter| qwen_account_matches(account, filter),
                        estimated_prompt_tokens,
                    )
                })
                .cloned()
                .collect(),
        ));
    }
    if all_providers || target == Some(TargetModel::DeepSeek) {
        scoped.deepseek_accounts = Arc::new(Mutex::new(
            state
                .deepseek_accounts
                .lock()
                .unwrap()
                .iter()
                .filter(|account| {
                    api_key_rule_allows_account(access, "deepseek", |filter| {
                        deepseek_account_matches(account, filter)
                    }) && api_key_account_prompt_limit_allows(
                        access,
                        "deepseek",
                        |filter| deepseek_account_matches(account, filter),
                        estimated_prompt_tokens,
                    )
                })
                .cloned()
                .collect(),
        ));
    }
    if all_providers || target == Some(TargetModel::Grok) {
        scoped.grok_accounts = Arc::new(Mutex::new(
            state
                .grok_accounts
                .lock()
                .unwrap()
                .iter()
                .filter(|account| {
                    api_key_rule_allows_account(access, "grok", |filter| {
                        grok_account_matches(account, filter)
                    }) && api_key_account_prompt_limit_allows(
                        access,
                        "grok",
                        |filter| grok_account_matches(account, filter),
                        estimated_prompt_tokens,
                    )
                })
                .cloned()
                .collect(),
        ));
    }
    if all_providers || target == Some(TargetModel::MiniMax) {
        scoped.minimax_accounts = Arc::new(Mutex::new(
            state
                .minimax_accounts
                .lock()
                .unwrap()
                .iter()
                .filter(|account| {
                    api_key_rule_allows_account(access, "minimax", |filter| {
                        minimax_account_matches(account, filter)
                    }) && api_key_account_prompt_limit_allows(
                        access,
                        "minimax",
                        |filter| minimax_account_matches(account, filter),
                        estimated_prompt_tokens,
                    )
                })
                .cloned()
                .collect(),
        ));
    }
    if all_providers || target == Some(TargetModel::Copilot) {
        scoped.copilot_accounts = Arc::new(Mutex::new(
            state
                .copilot_accounts
                .lock()
                .unwrap()
                .iter()
                .filter(|account| {
                    api_key_rule_allows_account(access, "copilot", |filter| {
                        copilot_account_matches(account, filter)
                    }) && api_key_account_prompt_limit_allows(
                        access,
                        "copilot",
                        |filter| copilot_account_matches(account, filter),
                        estimated_prompt_tokens,
                    )
                })
                .cloned()
                .collect(),
        ));
    }
    if all_providers || target == Some(TargetModel::Claude) {
        scoped.claude_accounts = Arc::new(Mutex::new(
            state
                .claude_accounts
                .lock()
                .unwrap()
                .iter()
                .filter(|account| {
                    api_key_rule_allows_account(access, "claude", |filter| {
                        claude_account_matches(account, filter)
                    }) && api_key_account_prompt_limit_allows(
                        access,
                        "claude",
                        |filter| claude_account_matches(account, filter),
                        estimated_prompt_tokens,
                    )
                })
                .cloned()
                .collect(),
        ));
    }
    if all_providers || target == Some(TargetModel::Glm) {
        scoped.glm_accounts = Arc::new(Mutex::new(
            state
                .glm_accounts
                .lock()
                .unwrap()
                .iter()
                .filter(|account| {
                    api_key_rule_allows_account(access, "glm", |filter| {
                        glm_account_matches(account, filter)
                    }) && api_key_account_prompt_limit_allows(
                        access,
                        "glm",
                        |filter| glm_account_matches(account, filter),
                        estimated_prompt_tokens,
                    )
                })
                .cloned()
                .collect(),
        ));
    }
    scoped
}

fn scoped_state_for_custom_target_account(
    state: AppState,
    target: TargetModel,
    candidate: &custom_models::CustomModelTarget,
) -> Result<AppState, String> {
    let Some(account_filter) = candidate
        .account
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return Ok(state);
    };
    let condition = candidate.account_condition;

    let mut scoped = state.clone();
    match target {
        TargetModel::Codex => {
            let tokens = state.tokens.lock().unwrap().clone();
            let filtered = tokens
                .into_iter()
                .filter(|token| {
                    token.enabled
                        && custom_account_condition_matches(
                            codex_token_matches_account(token, account_filter),
                            condition,
                        )
                })
                .collect::<Vec<_>>();
            if filtered.is_empty() {
                return Err(custom_account_condition_error(
                    "Codex",
                    account_filter,
                    condition,
                ));
            }
            scoped.tokens = Arc::new(Mutex::new(filtered));
            scoped.rr = Arc::new(Mutex::new(0));
        }
        TargetModel::Antigravity => {
            let accounts = state.agw_accounts.lock().unwrap().clone();
            let filtered = accounts
                .into_iter()
                .filter(|account| {
                    account.enabled
                        && custom_account_condition_matches(
                            antigravity_account_matches(account, account_filter),
                            condition,
                        )
                })
                .collect::<Vec<_>>();
            if filtered.is_empty() {
                return Err(custom_account_condition_error(
                    "Antigravity",
                    account_filter,
                    condition,
                ));
            }
            scoped.agw_accounts = Arc::new(Mutex::new(filtered));
            scoped.agw_rr = Arc::new(Mutex::new(0));
        }
        TargetModel::Gemini => {
            let accounts = state.gemini_accounts.lock().unwrap().clone();
            let filtered = accounts
                .into_iter()
                .filter(|account| {
                    account.enabled
                        && custom_account_condition_matches(
                            gemini_account_matches(account, account_filter),
                            condition,
                        )
                })
                .collect::<Vec<_>>();
            if filtered.is_empty() {
                return Err(custom_account_condition_error(
                    "Gemini",
                    account_filter,
                    condition,
                ));
            }
            scoped.gemini_accounts = Arc::new(Mutex::new(filtered));
            scoped.gemini_rr = Arc::new(Mutex::new(0));
        }
        TargetModel::Qwen => {
            let accounts = state.qwen_accounts.lock().unwrap().clone();
            let filtered = accounts
                .into_iter()
                .filter(|account| {
                    account.enabled
                        && custom_account_condition_matches(
                            qwen_account_matches(account, account_filter),
                            condition,
                        )
                })
                .collect::<Vec<_>>();
            if filtered.is_empty() {
                return Err(custom_account_condition_error(
                    "Qwen",
                    account_filter,
                    condition,
                ));
            }
            scoped.qwen_accounts = Arc::new(Mutex::new(filtered));
            scoped.qwen_rr = Arc::new(Mutex::new(0));
        }
        TargetModel::DeepSeek => {
            let accounts = state.deepseek_accounts.lock().unwrap().clone();
            let filtered = accounts
                .into_iter()
                .filter(|account| {
                    account.enabled
                        && custom_account_condition_matches(
                            deepseek_account_matches(account, account_filter),
                            condition,
                        )
                })
                .collect::<Vec<_>>();
            if filtered.is_empty() {
                return Err(custom_account_condition_error(
                    "DeepSeek",
                    account_filter,
                    condition,
                ));
            }
            scoped.deepseek_accounts = Arc::new(Mutex::new(filtered));
            scoped.deepseek_rr = Arc::new(Mutex::new(0));
        }
        TargetModel::Grok => {
            let accounts = state.grok_accounts.lock().unwrap().clone();
            let filtered = accounts
                .into_iter()
                .filter(|account| {
                    account.enabled
                        && custom_account_condition_matches(
                            grok_account_matches(account, account_filter),
                            condition,
                        )
                })
                .collect::<Vec<_>>();
            if filtered.is_empty() {
                return Err(custom_account_condition_error(
                    "Grok",
                    account_filter,
                    condition,
                ));
            }
            scoped.grok_accounts = Arc::new(Mutex::new(filtered));
            scoped.grok_rr = Arc::new(Mutex::new(0));
        }
        TargetModel::MiniMax => {
            let accounts = state.minimax_accounts.lock().unwrap().clone();
            let filtered = accounts
                .into_iter()
                .filter(|account| {
                    account.enabled
                        && custom_account_condition_matches(
                            minimax_account_matches(account, account_filter),
                            condition,
                        )
                })
                .collect::<Vec<_>>();
            if filtered.is_empty() {
                return Err(custom_account_condition_error(
                    "MiniMax",
                    account_filter,
                    condition,
                ));
            }
            scoped.minimax_accounts = Arc::new(Mutex::new(filtered));
            scoped.minimax_rr = Arc::new(Mutex::new(0));
        }
        TargetModel::Copilot => {
            let accounts = state.copilot_accounts.lock().unwrap().clone();
            let filtered = accounts
                .into_iter()
                .filter(|account| {
                    account.enabled
                        && custom_account_condition_matches(
                            copilot_account_matches(account, account_filter),
                            condition,
                        )
                })
                .collect::<Vec<_>>();
            if filtered.is_empty() {
                return Err(custom_account_condition_error(
                    "Copilot",
                    account_filter,
                    condition,
                ));
            }
            scoped.copilot_accounts = Arc::new(Mutex::new(filtered));
            scoped.copilot_rr = Arc::new(Mutex::new(0));
        }
        TargetModel::Claude => {
            let accounts = state.claude_accounts.lock().unwrap().clone();
            let filtered = accounts
                .into_iter()
                .filter(|account| {
                    account.enabled
                        && custom_account_condition_matches(
                            claude_account_matches(account, account_filter),
                            condition,
                        )
                })
                .collect::<Vec<_>>();
            if filtered.is_empty() {
                return Err(custom_account_condition_error(
                    "Claude",
                    account_filter,
                    condition,
                ));
            }
            scoped.claude_accounts = Arc::new(Mutex::new(filtered));
            scoped.claude_rr = Arc::new(Mutex::new(0));
        }
        TargetModel::Glm => {
            let accounts = state.glm_accounts.lock().unwrap().clone();
            let filtered = accounts
                .into_iter()
                .filter(|account| {
                    account.enabled
                        && custom_account_condition_matches(
                            glm_account_matches(account, account_filter),
                            condition,
                        )
                })
                .collect::<Vec<_>>();
            if filtered.is_empty() {
                return Err(custom_account_condition_error(
                    "GLM",
                    account_filter,
                    condition,
                ));
            }
            scoped.glm_accounts = Arc::new(Mutex::new(filtered));
            scoped.glm_rr = Arc::new(Mutex::new(0));
        }
        TargetModel::Custom | TargetModel::CodexModels | TargetModel::UnifiedV1Models => {}
    }
    Ok(scoped)
}

fn best_score_for_len<FEnabled, FScore>(
    len: usize,
    mut enabled: FEnabled,
    mut score_for: FScore,
) -> AccountSelectionScore
where
    FEnabled: FnMut(usize) -> bool,
    FScore: FnMut(usize) -> AccountSelectionScore,
{
    let mut best: Option<AccountSelectionScore> = None;
    for idx in 0..len {
        if !enabled(idx) {
            continue;
        }
        let score = score_for(idx);
        if best
            .as_ref()
            .map(|current| score.is_better_than(current))
            .unwrap_or(true)
        {
            best = Some(score);
        }
    }
    best.unwrap_or(AccountSelectionScore {
        quota_pressure: Some(f64::INFINITY),
        active_requests: u64::MAX,
        recent_requests: u64::MAX,
    })
}

fn rewrite_request_model(body: &Bytes, model: &str) -> Result<Bytes, String> {
    let mut value = serde_json::from_slice::<serde_json::Value>(body)
        .map_err(|_| "invalid request body".to_string())?;
    let object = value
        .as_object_mut()
        .ok_or_else(|| "request body must be a JSON object".to_string())?;
    object.insert(
        "model".to_string(),
        serde_json::Value::String(model.to_string()),
    );
    let body = serde_json::to_vec(&value)
        .map(Bytes::from)
        .map_err(|err| err.to_string())?;
    Ok(source::v1::provider::strip_provider_prefix_from_body(body))
}

fn should_custom_model_fallback(status: StatusCode) -> bool {
    status.as_u16() >= 400
}

pub(crate) fn should_retry_account_error(status: StatusCode, message: &str) -> bool {
    if matches!(
        status,
        StatusCode::UNAUTHORIZED
            | StatusCode::FORBIDDEN
            | StatusCode::REQUEST_TIMEOUT
            | StatusCode::CONFLICT
            | StatusCode::TOO_MANY_REQUESTS
            | StatusCode::BAD_GATEWAY
            | StatusCode::SERVICE_UNAVAILABLE
            | StatusCode::GATEWAY_TIMEOUT
    ) || status.is_server_error()
    {
        return true;
    }

    if status != StatusCode::BAD_REQUEST && status != StatusCode::PAYMENT_REQUIRED {
        return false;
    }

    let lower = message.to_ascii_lowercase();
    [
        "rate limit",
        "ratelimit",
        "too many requests",
        "quota",
        "resource exhausted",
        "insufficient_quota",
        "capacity",
        "overloaded",
        "temporarily unavailable",
        "try again later",
        "usage limit",
        "daily limit",
        "weekly limit",
        "monthly limit",
        "requests limit",
        "token limit exceeded",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

async fn dispatch_custom_target(
    state: AppState,
    headers: HeaderMap,
    upstream_path: &str,
    target: TargetModel,
    body: Bytes,
    response_mode: ResponseMode,
) -> axum::response::Response {
    match target {
        TargetModel::Codex => {
            dispatch_codex_custom_target(state, headers, upstream_path, body, response_mode).await
        }
        TargetModel::Antigravity => match upstream_path {
            "responses/compact" => target::antigravity::api::compact(State(state), headers, body)
                .await
                .into_response(),
            _ => target::antigravity::api::responses(State(state), headers, body)
                .await
                .into_response(),
        },
        TargetModel::Gemini => target::gemini::api::responses(State(state), headers, body)
            .await
            .into_response(),
        TargetModel::Qwen => target::qwen::api::responses(State(state), headers, body)
            .await
            .into_response(),
        TargetModel::DeepSeek => target::deepseek::api::responses(State(state), headers, body)
            .await
            .into_response(),
        TargetModel::Grok => target::grok::api::responses(State(state), headers, body)
            .await
            .into_response(),
        TargetModel::MiniMax => {
            target::minimax::responses_native::responses(State(state), headers, body)
                .await
                .into_response()
        }
        TargetModel::Copilot => target::copilot::api::responses(State(state), headers, body)
            .await
            .into_response(),
        TargetModel::Claude => target::claude::api::responses(State(state), headers, body)
            .await
            .into_response(),
        TargetModel::Glm => match upstream_path {
            "chat/completions" => target::glm::api::chat_completions(State(state), headers, body)
                .await
                .into_response(),
            "anthropic/v1/messages" => {
                target::glm::anthropic::messages(State(state), headers, body)
                    .await
                    .into_response()
            }
            _ => target::glm::api::responses(State(state), headers, body)
                .await
                .into_response(),
        },
        TargetModel::Custom | TargetModel::CodexModels | TargetModel::UnifiedV1Models => (
            StatusCode::BAD_REQUEST,
            [("Content-Type", "application/json")],
            openai_error_body("unsupported custom target", "invalid_request_error", None),
        )
            .into_response(),
    }
}

async fn dispatch_codex_custom_target(
    state: AppState,
    headers: HeaderMap,
    upstream_path: &str,
    body: Bytes,
    response_mode: ResponseMode,
) -> axum::response::Response {
    let upstream =
        target::codex::gateway::build_upstream_url(&state.cfg.upstream_base, upstream_path, None);
    let token_candidates = candidate_tokens_with_reservation(&state, true);
    if token_candidates.is_empty() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            [("Content-Type", "application/json")],
            openai_error_body("No upstream credentials configured", "server_error", None),
        )
            .into_response();
    }
    let request_value: Option<serde_json::Value> = serde_json::from_slice(&body).ok();
    let model = request_value.as_ref().and_then(model_from_request_value);
    let prompt = request_value
        .as_ref()
        .map(prompt_metrics_from_request_value)
        .unwrap_or_default();
    let mut last_error: Option<(StatusCode, String)> = None;

    for (attempt_idx, (_token_idx, token)) in token_candidates.iter().enumerate() {
        let context = codex_usage_context(
            token,
            model.clone(),
            upstream_path.to_string(),
            prompt.clone(),
        );
        record_codex_request(&state, &context);

        let session_id = Uuid::new_v4().to_string();
        let request_body = target::codex::gateway::build_request_body(
            &Method::POST,
            upstream_path,
            &headers,
            body.clone(),
            &session_id,
        );
        let mut req = state.client.request(Method::POST, upstream.clone());
        for (key, value) in headers.iter() {
            if should_drop_incoming_header(key.as_str()) {
                continue;
            }
            req = req.header(key, value);
        }
        req = req.header("Authorization", format!("Bearer {}", token.token));
        req = target::codex::gateway::apply_default_headers(
            req,
            &headers,
            token.account_id.as_deref(),
            &session_id,
        );

        let resp = match req.body(request_body).send().await {
            Ok(resp) => resp,
            Err(err) => {
                let message = format!("Upstream error: {}", err);
                record_codex_error(&state, &context, &message);
                last_error = Some((StatusCode::BAD_GATEWAY, message));
                if attempt_idx + 1 < token_candidates.len() {
                    continue;
                }
                break;
            }
        };
        let status = resp.status();
        let mut out_headers = HeaderMap::new();
        for (key, value) in resp.headers().iter() {
            let name = key.as_str().to_ascii_lowercase();
            if !is_hop_header(&name) && name != "content-encoding" && name != "content-length" {
                out_headers.insert(key.clone(), value.clone());
            }
        }
        let body_bytes = match resp.bytes().await {
            Ok(body) => body,
            Err(err) => {
                let message = format!("Upstream error: {}", err);
                record_codex_error(&state, &context, &message);
                last_error = Some((StatusCode::BAD_GATEWAY, message));
                if attempt_idx + 1 < token_candidates.len() {
                    continue;
                }
                break;
            }
        };
        if status.is_success() {
            let is_sse = out_headers
                .get(axum::http::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .map(|value| value.contains("text/event-stream"))
                .unwrap_or(false);
            if is_sse {
                if let Some(message) = sse_error_message(&body_bytes) {
                    let status = codex_error_status_from_message(&message, StatusCode::BAD_GATEWAY);
                    let affects_account_health = codex_error_affects_account_health(&message);
                    record_codex_error_with_health(
                        &state,
                        &context,
                        &message,
                        affects_account_health,
                    );
                    if affects_account_health
                        && attempt_idx + 1 < token_candidates.len()
                        && should_retry_account_error(StatusCode::TOO_MANY_REQUESTS, &message)
                    {
                        last_error = Some((StatusCode::TOO_MANY_REQUESTS, message));
                        continue;
                    }
                    out_headers.insert(
                        axum::http::header::CONTENT_TYPE,
                        HeaderValue::from_static("application/json"),
                    );
                    return (
                        status,
                        out_headers,
                        openai_error_body(
                            &message,
                            status_to_error_type(status),
                            status_to_error_code(status),
                        ),
                    )
                        .into_response();
                }
                if let Some(metrics) = usage_metrics_from_sse_response_body(&body_bytes) {
                    record_usage_success(&state, &context, &metrics);
                }
                if matches!(response_mode, ResponseMode::SseToJson) {
                    out_headers.insert(
                        axum::http::header::CONTENT_TYPE,
                        HeaderValue::from_static("application/json"),
                    );
                    return (status, out_headers, sse_to_response_json(&body_bytes))
                        .into_response();
                }
            } else {
                let value = serde_json::from_slice::<serde_json::Value>(&body_bytes).ok();
                let metrics = value
                    .as_ref()
                    .map(usage_metrics_from_response_value)
                    .unwrap_or_default();
                record_usage_success(&state, &context, &metrics);
            }
            return (status, out_headers, body_bytes).into_response();
        } else {
            let retry_after = out_headers
                .get(axum::http::header::RETRY_AFTER)
                .and_then(|value| value.to_str().ok());
            let message = upstream_failure_message(status, retry_after, &body_bytes);
            record_codex_error(&state, &context, &message);
            if attempt_idx + 1 < token_candidates.len()
                && should_retry_account_error(status, &message)
            {
                last_error = Some((status, message));
                continue;
            }
            return (status, out_headers, body_bytes).into_response();
        }
    }

    let (status, message) = last_error.unwrap_or_else(|| {
        (
            StatusCode::BAD_GATEWAY,
            "All Codex accounts failed".to_string(),
        )
    });
    let status = codex_error_status_from_message(&message, status);
    (
        status,
        [("Content-Type", "application/json")],
        openai_error_body(
            &format!("All Codex accounts failed; last error: {}", message),
            status_to_error_type(status),
            status_to_error_code(status),
        ),
    )
        .into_response()
}

fn upstream_failure_message(status: StatusCode, retry_after: Option<&str>, body: &[u8]) -> String {
    let retry_hint = retry_after
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| format!("; retry-after={}", value))
        .unwrap_or_default();
    let body = String::from_utf8_lossy(body);
    let body = body.trim();
    if body.is_empty() {
        format!("upstream status {}{}", status, retry_hint)
    } else {
        format!("upstream status {}{}: {}", status, retry_hint, body)
    }
}

fn codex_error_status_from_message(message: &str, fallback: StatusCode) -> StatusCode {
    let lower = message.to_ascii_lowercase();
    for status in [400, 401, 403, 408, 409, 429, 500, 502, 503, 504] {
        if contains_http_status(&lower, status) {
            return StatusCode::from_u16(status).unwrap_or(fallback);
        }
    }

    let is_quota = [
        "rate limit",
        "ratelimit",
        "too many requests",
        "quota",
        "resource exhausted",
        "insufficient_quota",
        "capacity",
        "usage limit",
        "daily limit",
        "weekly limit",
        "monthly limit",
        "requests limit",
        "token limit exceeded",
    ]
    .iter()
    .any(|needle| lower.contains(needle));
    if is_quota {
        return StatusCode::TOO_MANY_REQUESTS;
    }

    if lower.contains("invalid request history")
        || lower.contains("unsupported parameter")
        || lower.contains("invalid request")
        || lower.contains("bad request")
    {
        return StatusCode::BAD_REQUEST;
    }
    if lower.contains("timeout") || lower.contains("timed out") {
        return StatusCode::GATEWAY_TIMEOUT;
    }
    if lower.contains("no upstream credentials") || lower.contains("no upstream tokens") {
        return StatusCode::SERVICE_UNAVAILABLE;
    }

    fallback
}

fn codex_error_response(
    source_api: SourceApi,
    status: StatusCode,
    message: impl Into<String>,
) -> Response {
    let message = message.into();
    if matches!(source_api, SourceApi::V1) {
        (
            status,
            [(
                axum::http::header::CONTENT_TYPE.as_str(),
                "application/json",
            )],
            openai_error_body(
                &message,
                status_to_error_type(status),
                status_to_error_code(status),
            ),
        )
            .into_response()
    } else {
        (status, message).into_response()
    }
}

async fn codex_models_response(state: AppState, headers: HeaderMap) -> axum::response::Response {
    let body_bytes = cached_raw_codex_models_body(&state, &headers)
        .await
        .unwrap_or_else(|| Bytes::from_static(br#"{"models":[]}"#));
    let body_bytes = augment_codex_models_json(&body_bytes, &state);
    (
        StatusCode::OK,
        [("Content-Type", "application/json")],
        body_bytes,
    )
        .into_response()
}

async fn cached_raw_codex_models_body(state: &AppState, headers: &HeaderMap) -> Option<Bytes> {
    let cache = CODEX_MODEL_CACHE.get_or_init(|| tokio::sync::Mutex::new(None));
    let mut cache = cache.lock().await;
    if let Some((fetched_at, body)) = cache.as_ref() {
        if model_cache_is_fresh(*fetched_at) {
            return Some(body.clone());
        }
    }
    let stale = cache.as_ref().map(|(_, body)| body.clone());
    if let Some(body) = fetch_raw_codex_models_body(state, headers).await {
        *cache = Some((std::time::Instant::now(), body.clone()));
        return Some(body);
    }
    stale
}

async fn fetch_raw_codex_models_body(state: &AppState, headers: &HeaderMap) -> Option<Bytes> {
    for token in enabled_codex_model_tokens(state) {
        if let Some(body) = fetch_raw_codex_models_body_with_token(state, headers, &token).await {
            return Some(body);
        }
    }
    None
}

async fn fetch_raw_codex_models_body_with_token(
    state: &AppState,
    headers: &HeaderMap,
    token: &UpstreamToken,
) -> Option<Bytes> {
    let session_id = Uuid::new_v4().to_string();
    let mut req = state.client.request(
        Method::GET,
        target::codex::gateway::build_upstream_url(
            &state.cfg.upstream_base,
            "models",
            Some("client_version=1.0.0"),
        ),
    );
    for (key, value) in headers.iter() {
        if should_drop_incoming_header(key.as_str()) {
            continue;
        }
        req = req.header(key, value);
    }
    req = req.header("Authorization", format!("Bearer {}", token.token));
    req = target::codex::gateway::apply_default_headers(
        req,
        headers,
        token.account_id.as_deref(),
        &session_id,
    );

    let resp = req.send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    resp.bytes().await.ok()
}

fn enabled_codex_model_tokens(state: &AppState) -> Vec<UpstreamToken> {
    state
        .tokens
        .lock()
        .unwrap()
        .iter()
        .filter(|token| token.enabled)
        .cloned()
        .collect()
}

fn model_cache_is_fresh(fetched_at: std::time::Instant) -> bool {
    fetched_at.elapsed() < Duration::from_secs(MODEL_CACHE_TTL_SECONDS)
}

async fn unified_v1_models_response(
    state: AppState,
    headers: HeaderMap,
    upstream_path: &str,
    access: &api_keys::ApiKeyAccess,
) -> axum::response::Response {
    let mut models = collect_unified_v1_models(&state, &headers).await;
    models.retain(|model| model_entry_allowed_for_api_key(model, &state, access));
    if models.is_empty() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            [("Content-Type", "application/json")],
            openai_error_body("No upstream credentials configured", "server_error", None),
        )
            .into_response();
    }

    models.sort_by(|left, right| {
        left.get("id")
            .and_then(|value| value.as_str())
            .cmp(&right.get("id").and_then(|value| value.as_str()))
    });

    if upstream_path == "models" {
        let body = serde_json::to_vec(&serde_json::json!({
            "object": "list",
            "data": models,
            "models": models
        }))
        .unwrap_or_default();
        return (StatusCode::OK, [("Content-Type", "application/json")], body).into_response();
    }

    let Some(model_id) = upstream_path.strip_prefix("models/") else {
        return (
            StatusCode::NOT_FOUND,
            [("Content-Type", "application/json")],
            openai_error_body("v1 endpoint not found", "invalid_request_error", None),
        )
            .into_response();
    };

    let model = models
        .into_iter()
        .find(|entry| model_entry_matches_id(entry, model_id));

    match model {
        Some(model) => (
            StatusCode::OK,
            [("Content-Type", "application/json")],
            serde_json::to_vec(&model).unwrap_or_default(),
        )
            .into_response(),
        None => (
            StatusCode::NOT_FOUND,
            [("Content-Type", "application/json")],
            openai_error_body(
                &format!("The model '{}' does not exist", model_id),
                "invalid_request_error",
                Some("model_not_found"),
            ),
        )
            .into_response(),
    }
}

fn model_entry_allowed_for_api_key(
    model: &serde_json::Value,
    state: &AppState,
    access: &api_keys::ApiKeyAccess,
) -> bool {
    if access.all {
        return true;
    }
    let prefix = model
        .get("provider_prefix")
        .and_then(|value| value.as_str())
        .or_else(|| {
            model
                .get("id")
                .and_then(|value| value.as_str())
                .and_then(|id| id.split_once(':').map(|(prefix, _)| prefix))
        })
        .unwrap_or_default();
    if prefix.eq_ignore_ascii_case("ctm") {
        let Some(alias) = model
            .get("upstream_model")
            .and_then(|value| value.as_str())
            .or_else(|| {
                model
                    .get("id")
                    .and_then(|value| value.as_str())
                    .and_then(|id| id.split_once(':').map(|(_, alias)| alias))
            })
        else {
            return false;
        };
        return find_custom_model(state, alias).is_some_and(|custom_model| {
            custom_model.routes.iter().any(|group| {
                group.targets.iter().any(|target| {
                    target.enabled
                        && target_provider_access_key(source::v1::provider::target_from_model(
                            &target.model,
                        ))
                        .is_some_and(|provider| access.allows_provider(provider))
                })
            })
        });
    }
    normalize_model_catalog_provider(prefix)
        .and_then(model_catalog_prefix_to_access_provider)
        .is_some_and(|provider| access.allows_provider(provider))
}

fn model_catalog_prefix_to_access_provider(prefix: &str) -> Option<&'static str> {
    match prefix {
        "cod" => Some("codex"),
        "agw" => Some("agw"),
        "gem" => Some("gemini"),
        "qwn" => Some("qwen"),
        "dsk" => Some("deepseek"),
        "grk" => Some("grok"),
        "min" => Some("minimax"),
        "cop" => Some("copilot"),
        "cld" => Some("claude"),
        "glm" => Some("glm"),
        _ => None,
    }
}

fn model_entry_matches_id(entry: &serde_json::Value, model_id: &str) -> bool {
    if entry.get("id").and_then(|value| value.as_str()) == Some(model_id) {
        return true;
    }
    if model_id.contains(':') {
        return false;
    }

    entry.get("upstream_model").and_then(|value| value.as_str()) == Some(model_id)
        && entry
            .get("provider_prefix")
            .and_then(|value| value.as_str())
            == Some(preferred_model_provider_prefix(model_id))
}

fn preferred_model_provider_prefix(model_id: &str) -> &'static str {
    match source::v1::provider::target_from_model(model_id) {
        TargetModel::Antigravity => "agw",
        TargetModel::Gemini => "gem",
        TargetModel::Qwen => "qwn",
        TargetModel::DeepSeek => "dsk",
        TargetModel::Grok => "grk",
        TargetModel::MiniMax => "min",
        TargetModel::Copilot => "cop",
        TargetModel::Claude => "cld",
        TargetModel::Glm => "glm",
        TargetModel::Custom => "ctm",
        TargetModel::Codex | TargetModel::CodexModels | TargetModel::UnifiedV1Models => "cod",
    }
}

fn normalize_model_catalog_provider(provider: &str) -> Option<&'static str> {
    match provider.trim().to_ascii_lowercase().as_str() {
        "cod" | "codex" => Some("cod"),
        "agw" | "antigravity" | "anti-gravity" => Some("agw"),
        "gem" | "gemini" => Some("gem"),
        "qwn" | "qwen" => Some("qwn"),
        "dsk" | "deepseek" | "deep-seek" => Some("dsk"),
        "grk" | "grok" | "xai" | "x-ai" => Some("grk"),
        "min" | "minimax" | "mini-max" => Some("min"),
        "cop" | "copilot" | "github-copilot" => Some("cop"),
        "cld" | "claude" | "anthropic" => Some("cld"),
        "glm" | "zai" | "z-ai" => Some("glm"),
        _ => None,
    }
}

fn is_safe_credential_file_name(file_name: &str) -> bool {
    let path = std::path::Path::new(file_name);
    !file_name.is_empty()
        && path.components().count() == 1
        && matches!(
            path.components().next(),
            Some(std::path::Component::Normal(_))
        )
}

async fn collect_unified_v1_models(
    state: &AppState,
    headers: &HeaderMap,
) -> Vec<serde_json::Value> {
    let (fetched_at, models, refresh_in_flight) = {
        let cache = unified_model_cache().lock().await;
        (
            cache.fetched_at,
            cache.models.clone(),
            cache.refresh_in_flight,
        )
    };

    if let Some(fetched_at) = fetched_at {
        if model_cache_is_fresh(fetched_at) {
            return models;
        }
        if !refresh_in_flight {
            start_unified_model_cache_refresh(state, headers, None).await;
        }
        return models;
    }

    let outcome = refresh_unified_model_cache_now(state, headers, None).await;
    if outcome.started || !outcome.models.is_empty() {
        return outcome.models;
    }

    wait_for_unified_model_cache(MODEL_LIST_TIMEOUT + Duration::from_millis(750)).await
}

struct UnifiedModelRefreshOutcome {
    models: Vec<serde_json::Value>,
    started: bool,
}

enum UnifiedModelRefreshStart {
    Started(HashMap<String, Vec<serde_json::Value>>),
    Busy(Vec<serde_json::Value>),
}

fn unified_model_cache() -> &'static tokio::sync::Mutex<UnifiedModelCatalog> {
    UNIFIED_MODEL_CACHE.get_or_init(|| tokio::sync::Mutex::new(UnifiedModelCatalog::default()))
}

async fn begin_unified_model_cache_refresh() -> UnifiedModelRefreshStart {
    let mut cache = unified_model_cache().lock().await;
    if cache.refresh_in_flight {
        return UnifiedModelRefreshStart::Busy(cache.models.clone());
    }
    cache.refresh_in_flight = true;
    UnifiedModelRefreshStart::Started(cache.provider_models.clone())
}

async fn start_unified_model_cache_refresh(
    state: &AppState,
    headers: &HeaderMap,
    provider: Option<&'static str>,
) -> bool {
    let UnifiedModelRefreshStart::Started(stale_provider_models) =
        begin_unified_model_cache_refresh().await
    else {
        return false;
    };

    let state = state.clone();
    let headers = headers.clone();
    tokio::spawn(async move {
        run_unified_model_cache_refresh(&state, &headers, provider, stale_provider_models).await;
    });
    true
}

async fn refresh_unified_model_cache_now(
    state: &AppState,
    headers: &HeaderMap,
    provider: Option<&'static str>,
) -> UnifiedModelRefreshOutcome {
    match begin_unified_model_cache_refresh().await {
        UnifiedModelRefreshStart::Started(stale_provider_models) => UnifiedModelRefreshOutcome {
            models: run_unified_model_cache_refresh(
                state,
                headers,
                provider,
                stale_provider_models,
            )
            .await,
            started: true,
        },
        UnifiedModelRefreshStart::Busy(models) => UnifiedModelRefreshOutcome {
            models,
            started: false,
        },
    }
}

async fn run_unified_model_cache_refresh(
    state: &AppState,
    headers: &HeaderMap,
    provider: Option<&'static str>,
    stale_provider_models: HashMap<String, Vec<serde_json::Value>>,
) -> Vec<serde_json::Value> {
    let updates =
        fetch_unified_model_provider_updates(state, headers, &stale_provider_models, provider)
            .await;
    let mut provider_models = stale_provider_models;
    for (prefix, models) in updates {
        provider_models.insert(prefix, models);
    }

    let models = merge_unified_model_catalog(state, &provider_models);
    let mut cache = unified_model_cache().lock().await;
    cache.fetched_at = Some(std::time::Instant::now());
    cache.models = models.clone();
    cache.provider_models = provider_models;
    cache.refresh_in_flight = false;
    models
}

async fn fetch_unified_model_provider_updates(
    state: &AppState,
    headers: &HeaderMap,
    stale_provider_models: &HashMap<String, Vec<serde_json::Value>>,
    provider: Option<&'static str>,
) -> Vec<(String, Vec<serde_json::Value>)> {
    let prefixes = provider
        .map(|prefix| vec![prefix])
        .unwrap_or_else(|| MODEL_PROVIDER_PREFIXES.to_vec());
    futures_util::future::join_all(prefixes.into_iter().map(|prefix| {
        refresh_provider_models_by_prefix(state, headers, stale_provider_models, prefix)
    }))
    .await
}

async fn refresh_provider_models_by_prefix(
    state: &AppState,
    headers: &HeaderMap,
    stale_provider_models: &HashMap<String, Vec<serde_json::Value>>,
    prefix: &'static str,
) -> (String, Vec<serde_json::Value>) {
    let models = match prefix {
        "cod" => {
            fetch_provider_models_with_timeout(
                prefix,
                stale_provider_models,
                has_enabled_codex_account(state),
                async {
                    fetch_codex_v1_models_result(state, headers)
                        .await
                        .map(|models| provider_prefixed_models(models, "cod"))
                },
            )
            .await
        }
        "agw" => {
            fetch_provider_models_with_timeout(
                prefix,
                stale_provider_models,
                has_enabled_antigravity_account(state),
                async {
                    fetch_openai_models_from_response_result(
                        target::antigravity::api::models(State(state.clone()), headers.clone())
                            .await
                            .into_response(),
                    )
                    .await
                    .map(|models| provider_prefixed_models(models, "agw"))
                },
            )
            .await
        }
        "gem" => {
            fetch_provider_models_with_timeout(
                prefix,
                stale_provider_models,
                has_enabled_gemini_account(state),
                async {
                    fetch_openai_models_from_response_result(
                        target::gemini::api::models(State(state.clone()), headers.clone())
                            .await
                            .into_response(),
                    )
                    .await
                    .map(|models| provider_prefixed_models(models, "gem"))
                },
            )
            .await
        }
        "qwn" => {
            fetch_provider_models_with_timeout(
                prefix,
                stale_provider_models,
                has_enabled_qwen_account(state),
                async {
                    fetch_openai_models_from_response_result(
                        target::qwen::api::models(State(state.clone()), headers.clone())
                            .await
                            .into_response(),
                    )
                    .await
                    .map(|models| provider_prefixed_models(models, "qwn"))
                },
            )
            .await
        }
        "dsk" => {
            fetch_provider_models_with_timeout(
                prefix,
                stale_provider_models,
                has_enabled_deepseek_account(state),
                async {
                    fetch_openai_models_from_response_result(
                        target::deepseek::api::models(State(state.clone()), headers.clone())
                            .await
                            .into_response(),
                    )
                    .await
                    .map(|models| provider_prefixed_models(models, "dsk"))
                },
            )
            .await
        }
        "grk" => {
            fetch_provider_models_with_timeout(
                prefix,
                stale_provider_models,
                has_enabled_grok_account(state),
                async {
                    fetch_openai_models_from_response_result(
                        target::grok::api::models(State(state.clone()), headers.clone())
                            .await
                            .into_response(),
                    )
                    .await
                    .map(|models| provider_prefixed_models(models, "grk"))
                },
            )
            .await
        }
        "min" => {
            fetch_provider_models_with_timeout(
                prefix,
                stale_provider_models,
                has_enabled_minimax_account(state),
                async {
                    fetch_openai_models_from_response_result(
                        target::minimax::api::models(State(state.clone()), headers.clone())
                            .await
                            .into_response(),
                    )
                    .await
                    .map(|models| provider_prefixed_models(models, "min"))
                },
            )
            .await
        }
        "cop" => {
            fetch_provider_models_with_timeout(
                prefix,
                stale_provider_models,
                has_enabled_copilot_account(state),
                async {
                    fetch_openai_models_from_response_result(
                        target::copilot::api::models(State(state.clone()), headers.clone())
                            .await
                            .into_response(),
                    )
                    .await
                    .map(|models| provider_prefixed_models(models, "cop"))
                },
            )
            .await
        }
        "cld" => {
            fetch_provider_models_with_timeout(
                prefix,
                stale_provider_models,
                has_enabled_claude_account(state),
                async {
                    fetch_openai_models_from_response_result(
                        target::claude::api::models(State(state.clone()), headers.clone())
                            .await
                            .into_response(),
                    )
                    .await
                    .map(|models| provider_prefixed_models(models, "cld"))
                },
            )
            .await
        }
        "glm" => {
            fetch_provider_models_with_timeout(
                prefix,
                stale_provider_models,
                has_enabled_glm_account(state),
                async {
                    fetch_openai_models_from_response_result(
                        target::glm::api::models(State(state.clone()), headers.clone())
                            .await
                            .into_response(),
                    )
                    .await
                    .map(|models| provider_prefixed_models(models, "glm"))
                },
            )
            .await
        }
        _ => Vec::new(),
    };
    (prefix.to_string(), models)
}

async fn fetch_provider_models_with_timeout<F>(
    prefix: &str,
    stale_provider_models: &HashMap<String, Vec<serde_json::Value>>,
    enabled: bool,
    future: F,
) -> Vec<serde_json::Value>
where
    F: Future<Output = Option<Vec<serde_json::Value>>>,
{
    if !enabled {
        return Vec::new();
    }

    match tokio::time::timeout(MODEL_LIST_TIMEOUT, future).await {
        Ok(Some(models)) => models,
        Ok(None) | Err(_) => stale_provider_models
            .get(prefix)
            .cloned()
            .unwrap_or_default(),
    }
}

fn merge_unified_model_catalog(
    state: &AppState,
    provider_models: &HashMap<String, Vec<serde_json::Value>>,
) -> Vec<serde_json::Value> {
    let mut models = Vec::new();
    let mut seen = HashSet::new();
    for prefix in MODEL_PROVIDER_PREFIXES {
        append_unique_models(
            &mut models,
            &mut seen,
            provider_models.get(prefix).cloned().unwrap_or_default(),
        );
    }
    append_unique_models(&mut models, &mut seen, custom_model_openai_entries(state));
    models
}

async fn wait_for_unified_model_cache(timeout: Duration) -> Vec<serde_json::Value> {
    let started_at = std::time::Instant::now();
    loop {
        {
            let cache = unified_model_cache().lock().await;
            if !cache.models.is_empty() || !cache.refresh_in_flight {
                return cache.models.clone();
            }
        }
        if started_at.elapsed() >= timeout {
            return Vec::new();
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn invalidate_unified_model_cache() {
    if let Some(cache) = UNIFIED_MODEL_CACHE.get() {
        *cache.lock().await = UnifiedModelCatalog::default();
    }
}

async fn invalidate_codex_model_cache() {
    if let Some(cache) = CODEX_MODEL_CACHE.get() {
        *cache.lock().await = None;
    }
}

async fn invalidate_model_caches() {
    invalidate_unified_model_cache().await;
    invalidate_codex_model_cache().await;
}

fn custom_model_openai_entries(state: &AppState) -> Vec<serde_json::Value> {
    state
        .custom_models
        .lock()
        .unwrap()
        .iter()
        .filter(|model| model.enabled)
        .map(|model| {
            serde_json::json!({
                "id": custom_models::public_model_id(&model.alias),
                "object": "model",
                "created": 0,
                "owned_by": "custom",
                "display_name": model.display_name.clone().unwrap_or_else(|| model.alias.clone()),
                "provider_prefix": "ctm",
                "upstream_model": model.alias,
                "routes": model.routes.clone(),
                "route_group_count": custom_models::route_group_count(model),
                "target_count": custom_models::target_count(model)
            })
        })
        .collect()
}

fn provider_prefixed_models(
    incoming: Vec<serde_json::Value>,
    provider_prefix: &str,
) -> Vec<serde_json::Value> {
    incoming
        .into_iter()
        .filter_map(|model| provider_prefixed_model(model, provider_prefix))
        .collect()
}

fn provider_prefixed_model(
    mut model: serde_json::Value,
    provider_prefix: &str,
) -> Option<serde_json::Value> {
    let id = model
        .get("id")
        .and_then(|value| value.as_str())
        .map(|value| value.to_string())?;

    let (public_id, upstream_model, public_prefix) =
        if let Some((existing_prefix, upstream_model)) = split_provider_prefixed_model_id(&id) {
            let upstream_model = upstream_model.to_string();
            let public_prefix = existing_prefix.to_ascii_lowercase();
            (id, upstream_model, public_prefix)
        } else {
            (
                format!("{}:{}", provider_prefix, id),
                id,
                provider_prefix.to_string(),
            )
        };

    let object = model.as_object_mut()?;
    object.insert("id".to_string(), serde_json::Value::String(public_id));
    object.insert(
        "upstream_model".to_string(),
        serde_json::Value::String(upstream_model),
    );
    object.insert(
        "provider_prefix".to_string(),
        serde_json::Value::String(public_prefix),
    );

    Some(model)
}

fn split_provider_prefixed_model_id(model: &str) -> Option<(&str, &str)> {
    let (prefix, upstream_model) = model.split_once(':')?;
    if !is_supported_provider_prefix(prefix) {
        return None;
    }
    if upstream_model
        .chars()
        .next()
        .is_none_or(|ch| ch.is_whitespace())
    {
        return None;
    }
    let upstream_model = upstream_model.trim_end();
    if upstream_model.is_empty() {
        return None;
    }
    Some((prefix, upstream_model))
}

fn is_supported_provider_prefix(prefix: &str) -> bool {
    matches!(
        prefix.to_ascii_lowercase().as_str(),
        "agw" | "gem" | "qwn" | "dsk" | "grk" | "min" | "cop" | "cld" | "glm" | "cod" | "ctm"
    )
}

#[cfg(test)]
mod unified_model_catalog_tests {
    use super::{
        is_safe_credential_file_name, model_entry_matches_id, normalize_model_catalog_provider,
        provider_prefixed_model, quota_refresh_interval_for_snapshot, read_sse_prelude,
        split_provider_prefixed_model_id, sse_error_message, ReqwestByteStream,
        EXHAUSTED_QUOTA_REFRESH_SECONDS, MODEL_CACHE_TTL_SECONDS, QUOTA_REFRESH_SECONDS,
    };
    use std::time::Duration;

    #[test]
    fn unified_model_cache_ttl_is_ten_minutes() {
        assert_eq!(MODEL_CACHE_TTL_SECONDS, 10 * 60);
    }

    #[test]
    fn normalizes_dashboard_and_public_provider_names() {
        assert_eq!(normalize_model_catalog_provider("codex"), Some("cod"));
        assert_eq!(normalize_model_catalog_provider("gemini"), Some("gem"));
        assert_eq!(normalize_model_catalog_provider("qwen"), Some("qwn"));
        assert_eq!(normalize_model_catalog_provider("deepseek"), Some("dsk"));
        assert_eq!(normalize_model_catalog_provider("grok"), Some("grk"));
        assert_eq!(normalize_model_catalog_provider("minimax"), Some("min"));
        assert_eq!(normalize_model_catalog_provider("copilot"), Some("cop"));
        assert_eq!(normalize_model_catalog_provider("claude"), Some("cld"));
        assert_eq!(normalize_model_catalog_provider("glm"), Some("glm"));
        assert_eq!(normalize_model_catalog_provider("unknown"), None);
    }

    #[test]
    fn model_refresh_accepts_only_credential_basenames() {
        assert!(is_safe_credential_file_name("codex-account.json"));
        assert!(!is_safe_credential_file_name(""));
        assert!(!is_safe_credential_file_name("../codex-account.json"));
        assert!(!is_safe_credential_file_name("nested/codex-account.json"));
    }

    #[test]
    fn quota_refresh_interval_is_hourly_for_exhausted_snapshots() {
        let exhausted = serde_json::json!({
            "label": "account",
            "code_generation": {
                "five_hour": { "used_percent": 100.0, "reset_label": "1h" }
            }
        });
        assert_eq!(
            quota_refresh_interval_for_snapshot(&exhausted),
            Duration::from_secs(EXHAUSTED_QUOTA_REFRESH_SECONDS)
        );
    }

    #[test]
    fn quota_refresh_interval_stays_fast_for_available_snapshots() {
        let available = serde_json::json!({
            "label": "account",
            "code_generation": {
                "five_hour": { "used_percent": 99.9, "reset_label": "1m" }
            }
        });
        assert_eq!(
            quota_refresh_interval_for_snapshot(&available),
            Duration::from_secs(QUOTA_REFRESH_SECONDS)
        );
    }

    #[test]
    fn provider_prefixed_model_adds_public_prefix_and_keeps_upstream_model() {
        let model = provider_prefixed_model(
            serde_json::json!({
                "id": "gemini-2.5-pro",
                "object": "model"
            }),
            "gem",
        )
        .unwrap();

        assert_eq!(model["id"], "gem:gemini-2.5-pro");
        assert_eq!(model["upstream_model"], "gemini-2.5-pro");
        assert_eq!(model["provider_prefix"], "gem");
    }

    #[test]
    fn provider_prefixed_model_does_not_double_prefix_existing_ids() {
        let model = provider_prefixed_model(
            serde_json::json!({
                "id": "cop:gpt-5.1",
                "object": "model"
            }),
            "cop",
        )
        .unwrap();

        assert_eq!(model["id"], "cop:gpt-5.1");
        assert_eq!(model["upstream_model"], "gpt-5.1");
        assert_eq!(model["provider_prefix"], "cop");
    }

    #[test]
    fn split_provider_prefixed_model_id_accepts_only_supported_prefixes() {
        assert_eq!(
            split_provider_prefixed_model_id("min:MiniMax-M3"),
            Some(("min", "MiniMax-M3"))
        );
        assert_eq!(
            split_provider_prefixed_model_id("glm:glm-5.2"),
            Some(("glm", "glm-5.2"))
        );
        assert_eq!(split_provider_prefixed_model_id("openai:gpt-5.4"), None);
        assert_eq!(
            split_provider_prefixed_model_id("gem: gemini-2.5-pro"),
            None
        );
    }

    #[test]
    fn raw_model_retrieve_fallback_uses_default_provider_route() {
        let antigravity = serde_json::json!({
            "id": "agw:gemini-2.5-pro",
            "provider_prefix": "agw",
            "upstream_model": "gemini-2.5-pro"
        });
        let gemini = serde_json::json!({
            "id": "gem:gemini-2.5-pro",
            "provider_prefix": "gem",
            "upstream_model": "gemini-2.5-pro"
        });

        assert!(!model_entry_matches_id(&antigravity, "gemini-2.5-pro"));
        assert!(model_entry_matches_id(&gemini, "gemini-2.5-pro"));
        assert!(model_entry_matches_id(&antigravity, "agw:gemini-2.5-pro"));
    }

    #[test]
    fn detects_quota_errors_inside_successful_sse() {
        let body = bytes::Bytes::from_static(
            br#"event: error
data: {"type":"error","error":{"message":"rate limit exceeded"}}

data: [DONE]

"#,
        );
        assert_eq!(
            sse_error_message(&body).as_deref(),
            Some("rate limit exceeded")
        );
    }

    #[tokio::test]
    async fn sse_prelude_rejects_quota_error_before_output() {
        let mut stream: ReqwestByteStream = Box::pin(futures_util::stream::iter(vec![
            Ok::<_, reqwest::Error>(bytes::Bytes::from_static(
                b"data: {\"type\":\"response.created\"}\n\n",
            )),
            Ok::<_, reqwest::Error>(bytes::Bytes::from_static(
                b"data: {\"type\":\"error\",\"error\":{\"message\":\"quota exceeded\"}}\n\n",
            )),
        ]));

        let error = read_sse_prelude(&mut stream, Duration::from_secs(1))
            .await
            .unwrap_err();
        assert_eq!(error, "quota exceeded");
    }

    #[tokio::test]
    async fn sse_prelude_returns_metadata_and_first_output_together() {
        let mut stream: ReqwestByteStream = Box::pin(futures_util::stream::iter(vec![Ok::<
            _,
            reqwest::Error,
        >(
            bytes::Bytes::from_static(
                b"data: {\"type\":\"response.created\"}\n\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"ok\"}\n\n",
            ),
        )]));

        let prefix = read_sse_prelude(&mut stream, Duration::from_secs(1))
            .await
            .unwrap();
        let text = String::from_utf8(prefix.to_vec()).unwrap();
        assert!(text.contains("response.created"));
        assert!(text.contains("response.output_text.delta"));
    }

    #[tokio::test]
    async fn sse_prelude_times_out_when_upstream_never_emits() {
        let mut stream: ReqwestByteStream = Box::pin(futures_util::stream::pending());
        let error = read_sse_prelude(&mut stream, Duration::from_millis(10))
            .await
            .unwrap_err();
        assert!(error.contains("first-event timeout"));
    }
}

fn has_enabled_codex_account(state: &AppState) -> bool {
    state
        .tokens
        .lock()
        .unwrap()
        .iter()
        .any(|account| account.enabled)
}

fn has_enabled_antigravity_account(state: &AppState) -> bool {
    state
        .agw_accounts
        .lock()
        .unwrap()
        .iter()
        .any(|account| account.enabled)
}

fn has_enabled_gemini_account(state: &AppState) -> bool {
    state
        .gemini_accounts
        .lock()
        .unwrap()
        .iter()
        .any(|account| account.enabled)
}

fn has_enabled_qwen_account(state: &AppState) -> bool {
    state
        .qwen_accounts
        .lock()
        .unwrap()
        .iter()
        .any(|account| account.enabled)
}

fn has_enabled_deepseek_account(state: &AppState) -> bool {
    state
        .deepseek_accounts
        .lock()
        .unwrap()
        .iter()
        .any(|account| account.enabled)
}

fn has_enabled_grok_account(state: &AppState) -> bool {
    state
        .grok_accounts
        .lock()
        .unwrap()
        .iter()
        .any(|account| account.enabled)
}

fn has_enabled_minimax_account(state: &AppState) -> bool {
    state
        .minimax_accounts
        .lock()
        .unwrap()
        .iter()
        .any(|account| account.enabled)
}

fn has_enabled_copilot_account(state: &AppState) -> bool {
    state
        .copilot_accounts
        .lock()
        .unwrap()
        .iter()
        .any(|account| account.enabled)
}

fn has_enabled_claude_account(state: &AppState) -> bool {
    state
        .claude_accounts
        .lock()
        .unwrap()
        .iter()
        .any(|account| account.enabled)
}

fn has_enabled_glm_account(state: &AppState) -> bool {
    state
        .glm_accounts
        .lock()
        .unwrap()
        .iter()
        .any(|account| account.enabled)
}

fn append_unique_models(
    output: &mut Vec<serde_json::Value>,
    seen: &mut HashSet<String>,
    incoming: Vec<serde_json::Value>,
) {
    for model in incoming {
        let Some(id) = model
            .get("id")
            .and_then(|value| value.as_str())
            .map(|value| value.to_string())
        else {
            continue;
        };
        if seen.insert(id) {
            output.push(model);
        }
    }
}

async fn fetch_codex_v1_models_result(
    state: &AppState,
    headers: &HeaderMap,
) -> Option<Vec<serde_json::Value>> {
    let Some(body) = cached_raw_codex_models_body(state, headers).await else {
        return None;
    };
    let converted = match models_list_to_openai_json(&body) {
        Ok(body) => body,
        Err(_) => return None,
    };

    Some(model_entries_from_openai_list_bytes(&converted))
}

async fn fetch_openai_models_from_response_result(
    response: axum::response::Response,
) -> Option<Vec<serde_json::Value>> {
    if !response.status().is_success() {
        return None;
    }
    let body = match axum::body::to_bytes(response.into_body(), usize::MAX).await {
        Ok(body) => body,
        Err(_) => return None,
    };
    try_model_entries_from_openai_list_bytes(&body)
}

fn model_entries_from_openai_list_bytes(body: &[u8]) -> Vec<serde_json::Value> {
    try_model_entries_from_openai_list_bytes(body).unwrap_or_default()
}

fn try_model_entries_from_openai_list_bytes(body: &[u8]) -> Option<Vec<serde_json::Value>> {
    let value: serde_json::Value = match serde_json::from_slice(body) {
        Ok(value) => value,
        Err(_) => return None,
    };
    Some(
        value
            .get("data")
            .and_then(|value| value.as_array())
            .or_else(|| value.get("models").and_then(|value| value.as_array()))
            .cloned()
            .unwrap_or_default(),
    )
}

struct CompatSseState<S> {
    upstream: S,
    buffer: BytesMut,
    output_items: Vec<serde_json::Value>,
    queued: VecDeque<Bytes>,
}

struct CodexSseUsageTracker {
    state: AppState,
    context: UsageContext,
    buffer: BytesMut,
    recorded: bool,
}

impl CodexSseUsageTracker {
    fn new(state: AppState, context: UsageContext) -> Self {
        Self {
            state,
            context,
            buffer: BytesMut::new(),
            recorded: false,
        }
    }

    fn push(&mut self, chunk: &Bytes) {
        if self.recorded {
            return;
        }

        self.buffer.extend_from_slice(chunk);
        while let Some((event_end, delimiter_len)) = find_sse_event_boundary(&self.buffer) {
            let raw_event = self.buffer.split_to(event_end + delimiter_len);
            let event = raw_event[..event_end].to_vec();
            self.handle_event(&event);
            if self.recorded {
                break;
            }
        }
    }

    fn handle_event(&mut self, raw_event: &[u8]) {
        let text = String::from_utf8_lossy(raw_event);
        let mut data_lines = Vec::new();
        for line in text.lines() {
            let line = line.trim_end_matches('\r');
            if let Some(value) = line.strip_prefix("data:") {
                data_lines.push(value.trim_start().to_string());
            }
        }
        if data_lines.is_empty() {
            return;
        }

        let data_text = data_lines.join("\n");
        if data_text == "[DONE]" {
            record_usage_success(&self.state, &self.context, &UsageMetrics::default());
            self.recorded = true;
            return;
        }
        let value: serde_json::Value = match serde_json::from_str(&data_text) {
            Ok(value) => value,
            Err(_) => return,
        };
        if let Some(message) = sse_error_from_value(&value) {
            self.fail(&message);
            return;
        }
        if value.get("type").and_then(|v| v.as_str()) != Some("response.completed") {
            return;
        }
        let Some(response) = value.get("response") else {
            return;
        };
        let metrics = usage_metrics_from_response_value(response);
        record_usage_success(&self.state, &self.context, &metrics);
        self.recorded = true;
    }

    fn fail(&mut self, message: &str) {
        if self.recorded {
            return;
        }
        record_codex_error(&self.state, &self.context, message);
        self.recorded = true;
    }
}

impl Drop for CodexSseUsageTracker {
    fn drop(&mut self) {
        if !self.recorded {
            router_request_abandoned(&self.state, self.context.provider_name, &self.context.key);
        }
    }
}

fn compat_v1_sse_stream<S>(
    stream: S,
) -> impl futures_util::Stream<Item = Result<Bytes, std::io::Error>>
where
    S: futures_util::Stream<Item = Result<Bytes, std::io::Error>> + Unpin,
{
    stream::try_unfold(
        CompatSseState {
            upstream: stream,
            buffer: BytesMut::new(),
            output_items: Vec::new(),
            queued: VecDeque::new(),
        },
        |mut state| async move {
            loop {
                if let Some(chunk) = state.queued.pop_front() {
                    return Ok(Some((chunk, state)));
                }

                match state.upstream.next().await {
                    Some(Ok(chunk)) => queue_compat_sse_bytes(&mut state, &chunk),
                    Some(Err(err)) => return Err(err),
                    None => {
                        flush_compat_sse_buffer(&mut state);
                        if let Some(chunk) = state.queued.pop_front() {
                            return Ok(Some((chunk, state)));
                        }
                        return Ok(None);
                    }
                }
            }
        },
    )
}

fn queue_compat_sse_bytes<S>(state: &mut CompatSseState<S>, chunk: &Bytes) {
    state.buffer.extend_from_slice(chunk);

    while let Some((event_end, delimiter_len)) = find_sse_event_boundary(&state.buffer) {
        let raw_event = state.buffer.split_to(event_end + delimiter_len);
        let event = raw_event[..event_end].to_vec();
        push_compat_sse_event(state, &event, delimiter_len == 4);
    }
}

fn flush_compat_sse_buffer<S>(state: &mut CompatSseState<S>) {
    if state.buffer.is_empty() {
        return;
    }
    let remaining = state.buffer.split().to_vec();
    push_compat_sse_event(state, &remaining, false);
}

fn push_compat_sse_event<S>(state: &mut CompatSseState<S>, raw_event: &[u8], use_crlf: bool) {
    let event = rewrite_v1_sse_event(raw_event, &mut state.output_items, use_crlf);
    state.queued.push_back(Bytes::from(event));
}

fn find_sse_event_boundary(buffer: &[u8]) -> Option<(usize, usize)> {
    buffer
        .windows(4)
        .position(|window| {
            window
                == b"

"
        })
        .map(|idx| (idx, 4))
        .or_else(|| {
            buffer
                .windows(2)
                .position(|window| {
                    window
                        == b"

"
                })
                .map(|idx| (idx, 2))
        })
}

fn rewrite_v1_sse_event(
    raw_event: &[u8],
    output_items: &mut Vec<serde_json::Value>,
    use_crlf: bool,
) -> Vec<u8> {
    let text = String::from_utf8_lossy(raw_event);
    let mut event_name: Option<String> = None;
    let mut data_lines = Vec::new();

    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        if let Some(value) = line.strip_prefix("event:") {
            event_name = Some(value.trim().to_string());
        } else if let Some(value) = line.strip_prefix("data:") {
            data_lines.push(value.trim_start().to_string());
        }
    }

    if data_lines.is_empty() {
        return append_sse_delimiter(raw_event.to_vec(), use_crlf);
    }

    let data_text = data_lines.join(
        "
",
    );
    if data_text == "[DONE]" {
        return render_sse_event(event_name.as_deref(), "[DONE]", use_crlf);
    }

    let mut value: serde_json::Value = match serde_json::from_str(&data_text) {
        Ok(value) => value,
        Err(_) => return append_sse_delimiter(raw_event.to_vec(), use_crlf),
    };

    if let Some(kind) = value.get("type").and_then(|kind| kind.as_str()) {
        match kind {
            "response.output_item.done" => {
                if let Some(item) = value.get("item").cloned() {
                    let output_index = value
                        .get("output_index")
                        .and_then(|index| index.as_u64())
                        .unwrap_or(output_items.len() as u64)
                        as usize;
                    if output_items.len() <= output_index {
                        output_items.resize(output_index + 1, serde_json::Value::Null);
                    }
                    output_items[output_index] = item;
                }
            }
            "response.completed" => {
                if let Some(response) = value
                    .get_mut("response")
                    .and_then(|response| response.as_object_mut())
                {
                    let should_fill_output = response
                        .get("output")
                        .and_then(|output| output.as_array())
                        .map(|output| output.is_empty())
                        .unwrap_or(true);
                    if should_fill_output && output_items.iter().any(|item| !item.is_null()) {
                        response.insert(
                            "output".to_string(),
                            serde_json::Value::Array(
                                output_items
                                    .iter()
                                    .filter(|item| !item.is_null())
                                    .cloned()
                                    .collect(),
                            ),
                        );
                    }
                }
            }
            _ => {}
        }
    }

    let event_name = event_name.or_else(|| {
        value
            .get("type")
            .and_then(|kind| kind.as_str())
            .map(|kind| kind.to_string())
    });
    let data = serde_json::to_string(&value).unwrap_or(data_text);
    render_sse_event(event_name.as_deref(), &data, use_crlf)
}

fn append_sse_delimiter(mut raw_event: Vec<u8>, use_crlf: bool) -> Vec<u8> {
    if use_crlf {
        raw_event.extend_from_slice(
            b"

",
        );
    } else {
        raw_event.extend_from_slice(
            b"

",
        );
    }
    raw_event
}

fn render_sse_event(event_name: Option<&str>, data: &str, use_crlf: bool) -> Vec<u8> {
    let delimiter = if use_crlf {
        "
"
    } else {
        "
"
    };
    let mut out = String::new();
    if let Some(event_name) = event_name {
        out.push_str("event: ");
        out.push_str(event_name);
        out.push_str(delimiter);
    }
    out.push_str("data: ");
    out.push_str(data);
    out.push_str(delimiter);
    out.push_str(delimiter);
    out.into_bytes()
}

fn build_usage_stats(
    tokens: &[UpstreamToken],
    agw_accounts: &[target::antigravity::accounts::AntigravityAccount],
    gemini_accounts: &[target::gemini::accounts::GeminiAccount],
    qwen_accounts: &[target::qwen::accounts::QwenAccount],
    deepseek_accounts: &[target::deepseek::accounts::DeepSeekAccount],
    grok_accounts: &[target::grok::accounts::GrokAccount],
    minimax_accounts: &[target::minimax::accounts::MiniMaxAccount],
    copilot_accounts: &[target::copilot::accounts::CopilotAccount],
    claude_accounts: &[target::claude::accounts::ClaudeAccount],
    glm_accounts: &[target::glm::accounts::GlmAccount],
    persisted_stats: &StatsStore,
) -> UsageStats {
    let codex_accounts = tokens
        .iter()
        .map(|token| {
            let key = codex_stats_key(token);
            let stored = persisted_stats
                .account_usage(Provider::Codex, &key)
                .cloned()
                .unwrap_or_default();
            AccountUsage {
                key,
                label: token.label.clone(),
                account_id: token.account_id.clone().unwrap_or_default(),
                requests: stored.requests,
                errors: stored.errors,
                prompt_total: stored.prompt_total,
                prompt_error_total: stored.prompt_error_total,
                input_tokens: stored.input_tokens,
                output_tokens: stored.output_tokens,
                total_tokens: stored.total_tokens,
                cache_tokens: stored.cache_tokens,
                reasoning_tokens: stored.reasoning_tokens,
                first_seen_at: stored.first_seen_at,
                last_seen_at: stored.last_seen_at,
                last_success_at: stored.last_success_at,
                last_error_at: stored.last_error_at,
                last_error_message: stored.last_error_message,
            }
        })
        .collect();

    let agw_accounts = agw_accounts
        .iter()
        .map(|account| {
            let key = antigravity_stats_key(account);
            let stored = persisted_stats
                .account_usage(Provider::Antigravity, &key)
                .cloned()
                .unwrap_or_default();
            AccountUsage {
                key,
                label: account.label.clone(),
                account_id: account.email.clone(),
                requests: stored.requests,
                errors: stored.errors,
                prompt_total: stored.prompt_total,
                prompt_error_total: stored.prompt_error_total,
                input_tokens: stored.input_tokens,
                output_tokens: stored.output_tokens,
                total_tokens: stored.total_tokens,
                cache_tokens: stored.cache_tokens,
                reasoning_tokens: stored.reasoning_tokens,
                first_seen_at: stored.first_seen_at,
                last_seen_at: stored.last_seen_at,
                last_success_at: stored.last_success_at,
                last_error_at: stored.last_error_at,
                last_error_message: stored.last_error_message,
            }
        })
        .collect();

    let qwen_accounts = qwen_accounts
        .iter()
        .map(|account| {
            let key = qwen_stats_key(account);
            let stored = persisted_stats
                .account_usage(Provider::Qwen, &key)
                .cloned()
                .unwrap_or_default();
            AccountUsage {
                key,
                label: account.label.clone(),
                account_id: account.email.clone(),
                requests: stored.requests,
                errors: stored.errors,
                prompt_total: stored.prompt_total,
                prompt_error_total: stored.prompt_error_total,
                input_tokens: stored.input_tokens,
                output_tokens: stored.output_tokens,
                total_tokens: stored.total_tokens,
                cache_tokens: stored.cache_tokens,
                reasoning_tokens: stored.reasoning_tokens,
                first_seen_at: stored.first_seen_at,
                last_seen_at: stored.last_seen_at,
                last_success_at: stored.last_success_at,
                last_error_at: stored.last_error_at,
                last_error_message: stored.last_error_message,
            }
        })
        .collect();

    let gemini_accounts = gemini_accounts
        .iter()
        .map(|account| {
            let key = gemini_stats_key(account);
            let stored = persisted_stats
                .account_usage(Provider::Gemini, &key)
                .cloned()
                .unwrap_or_default();
            AccountUsage {
                key,
                label: account.label.clone(),
                account_id: account.email.clone(),
                requests: stored.requests,
                errors: stored.errors,
                prompt_total: stored.prompt_total,
                prompt_error_total: stored.prompt_error_total,
                input_tokens: stored.input_tokens,
                output_tokens: stored.output_tokens,
                total_tokens: stored.total_tokens,
                cache_tokens: stored.cache_tokens,
                reasoning_tokens: stored.reasoning_tokens,
                first_seen_at: stored.first_seen_at,
                last_seen_at: stored.last_seen_at,
                last_success_at: stored.last_success_at,
                last_error_at: stored.last_error_at,
                last_error_message: stored.last_error_message,
            }
        })
        .collect();

    let deepseek_accounts = deepseek_accounts
        .iter()
        .map(|account| {
            let key = deepseek_stats_key(account);
            let stored = persisted_stats
                .account_usage(Provider::DeepSeek, &key)
                .cloned()
                .unwrap_or_default();
            AccountUsage {
                key,
                label: account.label.clone(),
                account_id: account.account_id.clone(),
                requests: stored.requests,
                errors: stored.errors,
                prompt_total: stored.prompt_total,
                prompt_error_total: stored.prompt_error_total,
                input_tokens: stored.input_tokens,
                output_tokens: stored.output_tokens,
                total_tokens: stored.total_tokens,
                cache_tokens: stored.cache_tokens,
                reasoning_tokens: stored.reasoning_tokens,
                first_seen_at: stored.first_seen_at,
                last_seen_at: stored.last_seen_at,
                last_success_at: stored.last_success_at,
                last_error_at: stored.last_error_at,
                last_error_message: stored.last_error_message,
            }
        })
        .collect();

    let grok_accounts = grok_accounts
        .iter()
        .map(|account| {
            let key = grok_stats_key(account);
            let stored = persisted_stats
                .account_usage(Provider::Grok, &key)
                .cloned()
                .unwrap_or_default();
            AccountUsage {
                key,
                label: account.label.clone(),
                account_id: account
                    .user_id
                    .clone()
                    .or_else(|| account.email.clone())
                    .unwrap_or_default(),
                requests: stored.requests,
                errors: stored.errors,
                prompt_total: stored.prompt_total,
                prompt_error_total: stored.prompt_error_total,
                input_tokens: stored.input_tokens,
                output_tokens: stored.output_tokens,
                total_tokens: stored.total_tokens,
                cache_tokens: stored.cache_tokens,
                reasoning_tokens: stored.reasoning_tokens,
                first_seen_at: stored.first_seen_at,
                last_seen_at: stored.last_seen_at,
                last_success_at: stored.last_success_at,
                last_error_at: stored.last_error_at,
                last_error_message: stored.last_error_message,
            }
        })
        .collect();

    let minimax_accounts = minimax_accounts
        .iter()
        .map(|account| {
            let key = minimax_stats_key(account);
            let stored = persisted_stats
                .account_usage(Provider::MiniMax, &key)
                .cloned()
                .unwrap_or_default();
            AccountUsage {
                key,
                label: account.label.clone(),
                account_id: account.account_id.clone(),
                requests: stored.requests,
                errors: stored.errors,
                prompt_total: stored.prompt_total,
                prompt_error_total: stored.prompt_error_total,
                input_tokens: stored.input_tokens,
                output_tokens: stored.output_tokens,
                total_tokens: stored.total_tokens,
                cache_tokens: stored.cache_tokens,
                reasoning_tokens: stored.reasoning_tokens,
                first_seen_at: stored.first_seen_at,
                last_seen_at: stored.last_seen_at,
                last_success_at: stored.last_success_at,
                last_error_at: stored.last_error_at,
                last_error_message: stored.last_error_message,
            }
        })
        .collect();

    let copilot_accounts = copilot_accounts
        .iter()
        .map(|account| {
            let key = copilot_stats_key(account);
            let stored = persisted_stats
                .account_usage(Provider::Copilot, &key)
                .cloned()
                .unwrap_or_default();
            AccountUsage {
                key,
                label: account.label.clone(),
                account_id: account.account_id.clone(),
                requests: stored.requests,
                errors: stored.errors,
                prompt_total: stored.prompt_total,
                prompt_error_total: stored.prompt_error_total,
                input_tokens: stored.input_tokens,
                output_tokens: stored.output_tokens,
                total_tokens: stored.total_tokens,
                cache_tokens: stored.cache_tokens,
                reasoning_tokens: stored.reasoning_tokens,
                first_seen_at: stored.first_seen_at,
                last_seen_at: stored.last_seen_at,
                last_success_at: stored.last_success_at,
                last_error_at: stored.last_error_at,
                last_error_message: stored.last_error_message,
            }
        })
        .collect();

    let claude_accounts = claude_accounts
        .iter()
        .map(|account| {
            let key = claude_stats_key(account);
            let stored = persisted_stats
                .account_usage(Provider::Claude, &key)
                .cloned()
                .unwrap_or_default();
            AccountUsage {
                key,
                label: account.label.clone(),
                account_id: account.account_id.clone(),
                requests: stored.requests,
                errors: stored.errors,
                prompt_total: stored.prompt_total,
                prompt_error_total: stored.prompt_error_total,
                input_tokens: stored.input_tokens,
                output_tokens: stored.output_tokens,
                total_tokens: stored.total_tokens,
                cache_tokens: stored.cache_tokens,
                reasoning_tokens: stored.reasoning_tokens,
                first_seen_at: stored.first_seen_at,
                last_seen_at: stored.last_seen_at,
                last_success_at: stored.last_success_at,
                last_error_at: stored.last_error_at,
                last_error_message: stored.last_error_message,
            }
        })
        .collect();

    let glm_accounts = glm_accounts
        .iter()
        .map(|account| {
            let key = glm_stats_key(account);
            let stored = persisted_stats
                .account_usage(Provider::Glm, &key)
                .cloned()
                .unwrap_or_default();
            AccountUsage {
                key,
                label: account.label.clone(),
                account_id: account.account_id.clone(),
                requests: stored.requests,
                errors: stored.errors,
                prompt_total: stored.prompt_total,
                prompt_error_total: stored.prompt_error_total,
                input_tokens: stored.input_tokens,
                output_tokens: stored.output_tokens,
                total_tokens: stored.total_tokens,
                cache_tokens: stored.cache_tokens,
                reasoning_tokens: stored.reasoning_tokens,
                first_seen_at: stored.first_seen_at,
                last_seen_at: stored.last_seen_at,
                last_success_at: stored.last_success_at,
                last_error_at: stored.last_error_at,
                last_error_message: stored.last_error_message,
            }
        })
        .collect();

    UsageStats {
        codex_accounts,
        agw_accounts,
        gemini_accounts,
        qwen_accounts,
        deepseek_accounts,
        grok_accounts,
        minimax_accounts,
        copilot_accounts,
        claude_accounts,
        glm_accounts,
        total_requests: persisted_stats.total_requests,
        total_errors: persisted_stats.total_errors,
        total_prompt_total: persisted_stats.total_prompt_total,
        total_prompt_error_total: persisted_stats.total_prompt_error_total,
        total_input_tokens: persisted_stats.total_input_tokens,
        total_output_tokens: persisted_stats.total_output_tokens,
        total_tokens_used: persisted_stats.total_tokens_used,
        total_cache_tokens: persisted_stats.total_cache_tokens,
        total_reasoning_tokens: persisted_stats.total_reasoning_tokens,
        first_recorded_at: persisted_stats.first_recorded_at.clone(),
        last_recorded_at: persisted_stats.last_recorded_at.clone(),
    }
}

pub(crate) fn sync_usage_stats(state: &AppState) {
    let tokens = state.tokens.lock().unwrap().clone();
    let agw_accounts = state.agw_accounts.lock().unwrap().clone();
    let gemini_accounts = state.gemini_accounts.lock().unwrap().clone();
    let qwen_accounts = state.qwen_accounts.lock().unwrap().clone();
    let deepseek_accounts = state.deepseek_accounts.lock().unwrap().clone();
    let grok_accounts = state.grok_accounts.lock().unwrap().clone();
    let minimax_accounts = state.minimax_accounts.lock().unwrap().clone();
    let copilot_accounts = state.copilot_accounts.lock().unwrap().clone();
    let claude_accounts = state.claude_accounts.lock().unwrap().clone();
    let glm_accounts = state.glm_accounts.lock().unwrap().clone();
    let persisted_stats = state.persisted_stats.lock().unwrap().clone();
    let mut stats = state.stats.lock().unwrap();
    *stats = build_usage_stats(
        &tokens,
        &agw_accounts,
        &gemini_accounts,
        &qwen_accounts,
        &deepseek_accounts,
        &grok_accounts,
        &minimax_accounts,
        &copilot_accounts,
        &claude_accounts,
        &glm_accounts,
        &persisted_stats,
    );
}

pub(crate) fn codex_stats_key(token: &UpstreamToken) -> String {
    if let Some(account_id) = token.account_id.as_ref().filter(|s| !s.trim().is_empty()) {
        return format!("codex:account_id:{}", account_id);
    }
    if let Some(file_name) = token.file_name.as_ref().filter(|s| !s.trim().is_empty()) {
        return format!("codex:file:{}", file_name);
    }
    format!("codex:label:{}", token.label)
}

pub(crate) fn antigravity_stats_key(
    account: &target::antigravity::accounts::AntigravityAccount,
) -> String {
    if !account.email.trim().is_empty() {
        return format!("agw:email:{}", account.email);
    }
    if let Some(file_name) = account.file_name.as_ref().filter(|s| !s.trim().is_empty()) {
        return format!("agw:file:{}", file_name);
    }
    if let Some(project_id) = account.project_id.as_ref().filter(|s| !s.trim().is_empty()) {
        return format!("agw:project:{}", project_id);
    }
    format!("agw:label:{}", account.label)
}

pub(crate) fn qwen_stats_key(account: &target::qwen::accounts::QwenAccount) -> String {
    if let Some(subject) = account.subject.as_ref().filter(|s| !s.trim().is_empty()) {
        return format!("qwen:subject:{}", subject);
    }
    if !account.account_id.trim().is_empty() {
        return format!("qwen:subject:{}", account.account_id);
    }
    if !account.email.trim().is_empty() {
        return format!("qwen:email:{}", account.email);
    }
    if let Some(file_name) = account.file_name.as_ref().filter(|s| !s.trim().is_empty()) {
        return format!("qwen:file:{}", file_name);
    }
    if let Some(resource_url) = account
        .resource_url
        .as_ref()
        .filter(|s| !s.trim().is_empty())
    {
        return format!("qwen:resource:{}", resource_url);
    }
    format!("qwen:label:{}", account.label)
}

pub(crate) fn gemini_stats_key(account: &target::gemini::accounts::GeminiAccount) -> String {
    if !account.email.trim().is_empty() {
        if let Some(project_id) = account.project_id.as_ref().filter(|s| !s.trim().is_empty()) {
            return format!("gemini:email:{}|project:{}", account.email, project_id);
        }
        return format!("gemini:email:{}", account.email);
    }
    if let Some(file_name) = account.file_name.as_ref().filter(|s| !s.trim().is_empty()) {
        return format!("gemini:file:{}", file_name);
    }
    format!("gemini:label:{}", account.label)
}

pub(crate) fn deepseek_stats_key(account: &target::deepseek::accounts::DeepSeekAccount) -> String {
    if !account.account_id.trim().is_empty() {
        return format!("deepseek:account_id:{}", account.account_id);
    }
    if let Some(file_name) = account.file_name.as_ref().filter(|s| !s.trim().is_empty()) {
        return format!("deepseek:file:{}", file_name);
    }
    format!("deepseek:label:{}", account.label)
}

pub(crate) fn grok_stats_key(account: &target::grok::accounts::GrokAccount) -> String {
    if let Some(user_id) = account.user_id.as_ref().filter(|s| !s.trim().is_empty()) {
        return format!("grok:user_id:{}", user_id);
    }
    if let Some(email) = account.email.as_ref().filter(|s| !s.trim().is_empty()) {
        return format!("grok:email:{}", email);
    }
    if let Some(file_name) = account.file_name.as_ref().filter(|s| !s.trim().is_empty()) {
        return format!("grok:file:{}", file_name);
    }
    format!("grok:label:{}", account.label)
}

fn qwen_fallback_stats_keys(account: &target::qwen::accounts::QwenAccount) -> Vec<String> {
    let mut keys = Vec::new();
    if !account.email.trim().is_empty() {
        keys.push(format!("qwen:email:{}", account.email));
    }
    if let Some(file_name) = account.file_name.as_ref().filter(|s| !s.trim().is_empty()) {
        keys.push(format!("qwen:file:{}", file_name));
    }
    if let Some(resource_url) = account
        .resource_url
        .as_ref()
        .filter(|s| !s.trim().is_empty())
    {
        keys.push(format!("qwen:resource:{}", resource_url));
    }
    if !account.label.trim().is_empty() {
        keys.push(format!("qwen:label:{}", account.label));
    }
    keys
}

fn grok_fallback_stats_keys(account: &target::grok::accounts::GrokAccount) -> Vec<String> {
    let mut keys = Vec::new();
    if let Some(email) = account.email.as_ref().filter(|s| !s.trim().is_empty()) {
        keys.push(format!("grok:email:{}", email));
    }
    if let Some(file_name) = account.file_name.as_ref().filter(|s| !s.trim().is_empty()) {
        keys.push(format!("grok:file:{}", file_name));
    }
    if !account.label.trim().is_empty() {
        keys.push(format!("grok:label:{}", account.label));
    }
    keys
}

fn migrate_qwen_usage_keys(state: &AppState) {
    let accounts = state.qwen_accounts.lock().unwrap().clone();
    let mut changed = false;
    {
        let mut persisted = state.persisted_stats.lock().unwrap();
        for account in &accounts {
            let stable_key = qwen_stats_key(account);
            if persisted.qwen.contains_key(&stable_key) {
                continue;
            }

            for fallback_key in qwen_fallback_stats_keys(account) {
                if fallback_key == stable_key {
                    continue;
                }
                let Some(old_usage) = persisted.qwen.remove(&fallback_key) else {
                    continue;
                };
                let entry = persisted.qwen.entry(stable_key.clone()).or_default();
                merge_usage(entry, old_usage);
                changed = true;
                break;
            }
        }
    }
    if changed {
        persist_stats_store(state);
    }
}

pub(crate) fn migrate_grok_usage_keys(state: &AppState) {
    let accounts = state.grok_accounts.lock().unwrap().clone();
    let mut changed = false;
    {
        let mut persisted = state.persisted_stats.lock().unwrap();
        for account in &accounts {
            let stable_key = grok_stats_key(account);
            if persisted.grok.contains_key(&stable_key) {
                continue;
            }

            for fallback_key in grok_fallback_stats_keys(account) {
                if fallback_key == stable_key {
                    continue;
                }
                let Some(old_usage) = persisted.grok.remove(&fallback_key) else {
                    continue;
                };
                let entry = persisted.grok.entry(stable_key.clone()).or_default();
                merge_usage(entry, old_usage);
                changed = true;
                break;
            }
        }
    }
    if changed {
        persist_stats_store(state);
    }
}

fn merge_usage(
    target: &mut stats_store::StoredAccountUsage,
    source: stats_store::StoredAccountUsage,
) {
    target.label = source.label;
    target.account_id = source.account_id;
    target.requests += source.requests;
    target.errors += source.errors;
    target.prompt_total += source.prompt_total;
    target.prompt_error_total += source.prompt_error_total;
    target.input_tokens += source.input_tokens;
    target.output_tokens += source.output_tokens;
    target.total_tokens += source.total_tokens;
    target.cache_tokens += source.cache_tokens;
    target.reasoning_tokens += source.reasoning_tokens;
    target.first_seen_at = earliest_timestamp(target.first_seen_at.take(), source.first_seen_at);
    target.last_seen_at = latest_timestamp(target.last_seen_at.take(), source.last_seen_at);
    target.last_success_at =
        latest_timestamp(target.last_success_at.take(), source.last_success_at);
    let (last_error_at, last_error_message) = merge_latest_error_details(
        target.last_error_at.take(),
        target.last_error_message.take(),
        source.last_error_at,
        source.last_error_message,
    );
    target.last_error_at = last_error_at;
    target.last_error_message = last_error_message;
}

pub(crate) fn minimax_stats_key(account: &target::minimax::accounts::MiniMaxAccount) -> String {
    if !account.account_id.trim().is_empty() {
        return format!("minimax:account_id:{}", account.account_id);
    }
    if let Some(file_name) = account.file_name.as_ref().filter(|s| !s.is_empty()) {
        return format!("minimax:file:{}", file_name);
    }
    format!("minimax:label:{}", account.label)
}

pub(crate) fn copilot_stats_key(account: &target::copilot::accounts::CopilotAccount) -> String {
    if !account.account_id.trim().is_empty() {
        return format!("copilot:account_id:{}", account.account_id);
    }
    if !account.login.trim().is_empty() {
        return format!("copilot:login:{}", account.login);
    }
    if let Some(file_name) = account.file_name.as_ref().filter(|s| !s.trim().is_empty()) {
        return format!("copilot:file:{}", file_name);
    }
    format!("copilot:label:{}", account.label)
}

pub(crate) fn claude_stats_key(account: &target::claude::accounts::ClaudeAccount) -> String {
    if !account.organization_uuid.trim().is_empty() {
        return format!("claude:organization:{}", account.organization_uuid);
    }
    if !account.account_id.trim().is_empty() {
        return format!("claude:account_id:{}", account.account_id);
    }
    if let Some(email) = account
        .email
        .as_ref()
        .filter(|value| !value.trim().is_empty())
    {
        return format!("claude:email:{}", email);
    }
    if let Some(file_name) = account
        .file_name
        .as_ref()
        .filter(|value| !value.trim().is_empty())
    {
        return format!("claude:file:{}", file_name);
    }
    format!("claude:label:{}", account.label)
}

pub(crate) fn glm_stats_key(account: &target::glm::accounts::GlmAccount) -> String {
    if !account.account_id.trim().is_empty() {
        return format!("glm:account_id:{}", account.account_id);
    }
    if let Some(file_name) = account
        .file_name
        .as_ref()
        .filter(|value| !value.trim().is_empty())
    {
        return format!("glm:file:{}", file_name);
    }
    format!("glm:label:{}", account.label)
}

const ACCOUNT_SELECTION_QUOTA_EPSILON: f64 = 0.01;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct AccountSelectionScore {
    quota_pressure: Option<f64>,
    active_requests: u64,
    recent_requests: u64,
}

impl AccountSelectionScore {
    pub(crate) fn is_better_than(&self, other: &Self) -> bool {
        if self.is_quota_exhausted() != other.is_quota_exhausted() {
            return !self.is_quota_exhausted();
        }

        match (self.quota_pressure, other.quota_pressure) {
            (Some(candidate), Some(current)) => {
                let diff = candidate - current;
                if diff.abs() > ACCOUNT_SELECTION_QUOTA_EPSILON {
                    return candidate < current;
                }
            }
            (Some(candidate), None) if candidate.is_infinite() && candidate.is_sign_positive() => {
                return false;
            }
            (None, Some(current)) if current.is_infinite() && current.is_sign_positive() => {
                return true;
            }
            _ => {}
        }

        if self.active_requests != other.active_requests {
            return self.active_requests < other.active_requests;
        }

        if self.recent_requests != other.recent_requests {
            return self.recent_requests < other.recent_requests;
        }

        false
    }

    fn is_quota_exhausted(&self) -> bool {
        self.quota_pressure
            .map(|value| value.is_infinite() && value.is_sign_positive())
            .unwrap_or(false)
    }
}

pub(crate) fn select_ordered_account_indices_with_priority<FEnabled, FPriority, FScore>(
    len: usize,
    start_idx: usize,
    mut is_enabled: FEnabled,
    mut is_priority: FPriority,
    mut score_for: FScore,
) -> Vec<usize>
where
    FEnabled: FnMut(usize) -> bool,
    FPriority: FnMut(usize) -> bool,
    FScore: FnMut(usize) -> AccountSelectionScore,
{
    if len == 0 {
        return Vec::new();
    }

    let mut candidates = Vec::new();
    for offset in 0..len {
        let candidate_idx = (start_idx + offset) % len;
        if !is_enabled(candidate_idx) {
            continue;
        }
        candidates.push((
            candidate_idx,
            offset,
            is_priority(candidate_idx),
            score_for(candidate_idx),
        ));
    }

    let has_usable_priority = candidates
        .iter()
        .any(|(_, _, priority, score)| *priority && !score.is_quota_exhausted());
    let mut selected = candidates
        .into_iter()
        .filter(|(_, _, priority, score)| {
            if has_usable_priority {
                *priority && !score.is_quota_exhausted()
            } else {
                !*priority
            }
        })
        .collect::<Vec<_>>();

    if selected.is_empty() {
        return Vec::new();
    }

    selected.sort_by(
        |(_, left_offset, _, left_score), (_, right_offset, _, right_score)| {
            if left_score.is_better_than(right_score) {
                Ordering::Less
            } else if right_score.is_better_than(left_score) {
                Ordering::Greater
            } else {
                left_offset.cmp(right_offset)
            }
        },
    );

    selected.into_iter().map(|(idx, _, _, _)| idx).collect()
}

fn quota_cache_key(file_name: Option<&str>, label: &str) -> String {
    file_name
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| label.to_string())
}

fn push_quota_pressure(current: &mut Option<f64>, value: Option<f64>) {
    let Some(value) = value else {
        return;
    };
    if value.is_nan() {
        return;
    }
    *current = Some(current.map(|existing| existing.max(value)).unwrap_or(value));
}

fn usage_backed_selection_score(
    state: &AppState,
    provider: Provider,
    key: String,
    quota_pressure: Option<f64>,
) -> AccountSelectionScore {
    let provider_name = provider_name(provider);
    let (active_requests, recent_requests) = router_account_load(state, provider_name, &key);
    AccountSelectionScore {
        quota_pressure: quota_pressure.filter(|value| !value.is_nan()),
        active_requests: active_requests as u64,
        recent_requests: recent_requests as u64,
    }
}

fn provider_name(provider: Provider) -> &'static str {
    match provider {
        Provider::Codex => "codex",
        Provider::Antigravity => "antigravity",
        Provider::Gemini => "gemini",
        Provider::Qwen => "qwen",
        Provider::DeepSeek => "deepseek",
        Provider::Grok => "grok",
        Provider::MiniMax => "minimax",
        Provider::Copilot => "copilot",
        Provider::Claude => "claude",
        Provider::Glm => "glm",
    }
}

pub(crate) fn account_router_key(provider: &str, account_key: &str) -> String {
    format!("{}|{}", provider, account_key)
}

pub(crate) fn router_account_eligible(state: &AppState, provider: &str, account_key: &str) -> bool {
    let key = account_router_key(provider, account_key);
    let now = std::time::Instant::now();
    let mut router = state.account_router.lock().unwrap();
    let runtime = router.entry(key).or_default();
    matches!(
        account_runtime_status(true, runtime, now),
        AccountRuntimeStatus::Healthy
    )
}

pub(crate) fn account_runtime_status(
    enabled: bool,
    runtime: &AccountRuntimeState,
    now: std::time::Instant,
) -> AccountRuntimeStatus {
    if !enabled {
        return AccountRuntimeStatus::Disabled;
    }
    match runtime.cooldown_until {
        Some(until) if until > now => AccountRuntimeStatus::Cooldown,
        Some(_) if runtime.probing => AccountRuntimeStatus::Probing,
        _ => AccountRuntimeStatus::Healthy,
    }
}

pub(crate) fn account_refresh_lock(
    state: &AppState,
    provider: &str,
    account_key: &str,
) -> Arc<tokio::sync::Mutex<()>> {
    state
        .account_refresh_locks
        .lock()
        .unwrap()
        .entry(account_router_key(provider, account_key))
        .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone()
}

fn router_account_load(state: &AppState, provider: &str, account_key: &str) -> (usize, usize) {
    let mut router = state.account_router.lock().unwrap();
    let runtime = router
        .entry(account_router_key(provider, account_key))
        .or_default();
    let cutoff = std::time::Instant::now() - Duration::from_secs(5 * 60);
    runtime.recent_requests.retain(|started| *started >= cutoff);
    (runtime.active_requests, runtime.recent_requests.len())
}

fn router_request_started(state: &AppState, provider: &str, account_key: &str) {
    let key = account_router_key(provider, account_key);
    let mut router = state.account_router.lock().unwrap();
    let runtime = router.entry(key).or_default();
    if runtime.reserved_requests > 0 {
        runtime.reserved_requests -= 1;
        return;
    }
    runtime.active_requests = runtime.active_requests.saturating_add(1);
    let now = std::time::Instant::now();
    runtime.probing = runtime.cooldown_until.is_some_and(|until| until <= now);
    let cutoff = now - Duration::from_secs(5 * 60);
    runtime.recent_requests.retain(|started| *started >= cutoff);
    runtime.recent_requests.push_back(now);
}

/// Reserve the first account while its provider round-robin lock is still
/// held. This closes the gap between scoring and request bookkeeping: a
/// concurrent prompt cannot select the same apparently-idle account before
/// the first prompt has recorded its request.
pub(crate) fn router_reserve_account(state: &AppState, provider: &str, account_key: &str) {
    let key = account_router_key(provider, account_key);
    let mut router = state.account_router.lock().unwrap();
    let runtime = router.entry(key).or_default();
    runtime.active_requests = runtime.active_requests.saturating_add(1);
    runtime.reserved_requests = runtime.reserved_requests.saturating_add(1);
    let now = std::time::Instant::now();
    runtime.probing = runtime.cooldown_until.is_some_and(|until| until <= now);
    let cutoff = now - Duration::from_secs(5 * 60);
    runtime.recent_requests.retain(|started| *started >= cutoff);
    runtime.recent_requests.push_back(now);
}

pub(crate) fn router_request_abandoned(state: &AppState, provider: &str, account_key: &str) {
    let key = account_router_key(provider, account_key);
    let mut router = state.account_router.lock().unwrap();
    let runtime = router.entry(key).or_default();
    runtime.active_requests = runtime.active_requests.saturating_sub(1);
    runtime.probing = false;
}

pub(crate) fn router_reservation_cancelled(state: &AppState, provider: &str, account_key: &str) {
    let key = account_router_key(provider, account_key);
    let mut router = state.account_router.lock().unwrap();
    let runtime = router.entry(key).or_default();
    if runtime.reserved_requests > 0 {
        runtime.reserved_requests -= 1;
        runtime.active_requests = runtime.active_requests.saturating_sub(1);
        runtime.probing = false;
    }
}

fn router_request_finished_neutral(state: &AppState, provider: &str, account_key: &str) {
    let key = account_router_key(provider, account_key);
    let now = std::time::Instant::now();
    let mut router = state.account_router.lock().unwrap();
    let runtime = router.entry(key).or_default();
    runtime.active_requests = runtime.active_requests.saturating_sub(1);
    if runtime.cooldown_until.is_some_and(|until| until <= now) {
        runtime.consecutive_failures = 0;
        runtime.cooldown_until = None;
    }
    runtime.probing = false;
}

fn router_request_finished(
    state: &AppState,
    provider: &str,
    account_key: &str,
    success: bool,
    message: Option<&str>,
) {
    let key = account_router_key(provider, account_key);
    let mut router = state.account_router.lock().unwrap();
    let runtime = router.entry(key).or_default();
    runtime.active_requests = runtime.active_requests.saturating_sub(1);
    if success {
        runtime.consecutive_failures = 0;
        runtime.cooldown_until = None;
        runtime.probing = false;
        return;
    }

    apply_router_failure(
        runtime,
        message.unwrap_or_default(),
        std::time::Instant::now(),
    );
}

fn apply_router_failure(runtime: &mut AccountRuntimeState, message: &str, now: std::time::Instant) {
    let message = message.to_ascii_lowercase();
    // An upstream per-model denial ("this model requires paid credits", "model not
    // available on your plan") is NOT a quota/billing hit on the account. It only
    // affects the specific model and must not poison the rest of the provider's
    // account pool. Anthropic surfaces these via `error_code="credits_required"`
    // or a model-level `disabled_reason`, both of which we detect here. Without
    // this carve-out, one model access denial blacklists every other model for up
    // to 30 minutes even when each was working a moment ago.
    let is_model_access_denied = message.contains("\"error_code\":\"credits_required\"")
        || message.contains("\"error_code\":\"model_access_denied\"")
        || message.contains("\"error_code\":\"plan_doesnt_include_model\"")
        || message.contains("disabled_reason\":\"org_level_disabled\"")
        || message.contains("requires usage credits");
    if is_model_access_denied {
        runtime.consecutive_failures = 0;
        runtime.cooldown_until = None;
        runtime.probing = false;
        return;
    }
    let is_quota = contains_http_status(&message, 429)
        || message.contains("rate limit")
        || message.contains("ratelimit")
        || message.contains("quota")
        || message.contains("resource exhausted")
        || message.contains("usage limit");
    let deterministic_bad_request = message.contains("invalid request history")
        || (contains_http_status(&message, 400) && !is_quota);
    if deterministic_bad_request {
        runtime.consecutive_failures = 0;
        runtime.cooldown_until = None;
        runtime.probing = false;
        return;
    }
    runtime.consecutive_failures = runtime.consecutive_failures.saturating_add(1);
    let is_timeout = message.contains("timeout")
        || message.contains("timed out")
        || message.contains("failed to reach");
    let retry_after_seconds = parse_retry_after_seconds(&message);
    let delay_seconds = if is_quota {
        retry_after_seconds.unwrap_or(30 * 60)
    } else if is_timeout {
        5
    } else {
        2u64.saturating_pow(runtime.consecutive_failures.min(6))
    };
    runtime.cooldown_until = Some(now + Duration::from_secs(delay_seconds.clamp(1, 30 * 60)));
    runtime.probing = false;
}

fn codex_error_affects_account_health(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    let message = message.trim();
    if message.is_empty()
        || message == "upstream returned an sse error"
        || message.contains("invalid request history")
    {
        return false;
    }

    let is_quota_or_capacity = [
        "rate limit",
        "ratelimit",
        "too many requests",
        "quota",
        "resource exhausted",
        "insufficient_quota",
        "capacity",
        "overloaded",
        "temporarily unavailable",
        "try again later",
        "usage limit",
        "daily limit",
        "weekly limit",
        "monthly limit",
        "requests limit",
        "token limit exceeded",
    ]
    .iter()
    .any(|needle| message.contains(needle));
    if contains_http_status(message, 400) && !is_quota_or_capacity {
        return false;
    }

    [401, 403, 408, 409, 429, 500, 502, 503, 504]
        .iter()
        .any(|status| contains_http_status(message, *status))
        || is_quota_or_capacity
        || [
            "unauthorized",
            "forbidden",
            "timeout",
            "timed out",
            "failed to reach",
            "upstream send failed",
            "stream read failed",
            "body read failed",
            "error body read failed",
            "error decoding response body",
            "ended before producing",
            "oversized sse prelude",
        ]
        .iter()
        .any(|needle| message.contains(needle))
}

pub(crate) fn router_account_runtime_json(
    state: &AppState,
    provider: &str,
    account_key: &str,
    enabled: bool,
) -> serde_json::Value {
    let key = account_router_key(provider, account_key);
    let now = std::time::Instant::now();
    let mut router = state.account_router.lock().unwrap();
    let runtime = router.entry(key).or_default();
    let status = account_runtime_status(enabled, runtime, now);
    let cooldown_remaining_seconds = match runtime.cooldown_until {
        Some(until) if until > now => Some((until - now).as_secs().max(1)),
        _ => None,
    };
    serde_json::json!({
        "status": account_runtime_status_label(status),
        "cooldown_remaining_seconds": cooldown_remaining_seconds,
        "active_requests": runtime.active_requests,
        "consecutive_failures": runtime.consecutive_failures,
    })
}

fn account_runtime_status_label(status: AccountRuntimeStatus) -> &'static str {
    match status {
        AccountRuntimeStatus::Healthy => "healthy",
        AccountRuntimeStatus::Cooldown => "cooldown",
        AccountRuntimeStatus::Probing => "probing",
        AccountRuntimeStatus::Disabled => "disabled",
    }
}

fn clear_router_account_runtime(state: &AppState, provider: &str, account_key: &str) {
    let key = account_router_key(provider, account_key);
    let mut router = state.account_router.lock().unwrap();
    let runtime = router.entry(key).or_default();
    runtime.consecutive_failures = 0;
    runtime.cooldown_until = None;
    runtime.probing = false;
}

pub(crate) fn router_clear_credential_file_cooldown(state: &AppState, file_name: &str) {
    let file_name = file_name.trim();
    if file_name.is_empty() {
        return;
    }

    for token in state.tokens.lock().unwrap().iter() {
        if token.file_name.as_deref() == Some(file_name) {
            clear_router_account_runtime(state, "codex", &codex_stats_key(token));
        }
    }
    for account in state.agw_accounts.lock().unwrap().iter() {
        if account.file_name.as_deref() == Some(file_name) {
            clear_router_account_runtime(state, "antigravity", &antigravity_stats_key(account));
        }
    }
    for account in state.gemini_accounts.lock().unwrap().iter() {
        if account.file_name.as_deref() == Some(file_name) {
            clear_router_account_runtime(state, "gemini", &gemini_stats_key(account));
        }
    }
    for account in state.qwen_accounts.lock().unwrap().iter() {
        if account.file_name.as_deref() == Some(file_name) {
            clear_router_account_runtime(state, "qwen", &qwen_stats_key(account));
        }
    }
    for account in state.deepseek_accounts.lock().unwrap().iter() {
        if account.file_name.as_deref() == Some(file_name) {
            clear_router_account_runtime(state, "deepseek", &deepseek_stats_key(account));
        }
    }
    for account in state.minimax_accounts.lock().unwrap().iter() {
        if account.file_name.as_deref() == Some(file_name) {
            clear_router_account_runtime(state, "minimax", &minimax_stats_key(account));
        }
    }
    for account in state.grok_accounts.lock().unwrap().iter() {
        if account.file_name.as_deref() == Some(file_name) {
            clear_router_account_runtime(state, "grok", &grok_stats_key(account));
        }
    }
    for account in state.copilot_accounts.lock().unwrap().iter() {
        if account.file_name.as_deref() == Some(file_name) {
            clear_router_account_runtime(state, "copilot", &copilot_stats_key(account));
        }
    }
    for account in state.claude_accounts.lock().unwrap().iter() {
        if account.file_name.as_deref() == Some(file_name) {
            clear_router_account_runtime(state, "claude", &claude_stats_key(account));
        }
    }
    for account in state.glm_accounts.lock().unwrap().iter() {
        if account.file_name.as_deref() == Some(file_name) {
            clear_router_account_runtime(state, "glm", &glm_stats_key(account));
        }
    }
}

fn contains_http_status(message: &str, status: u16) -> bool {
    let status = status.to_string();
    message
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .any(|part| part == status)
}

fn parse_retry_after_seconds(message: &str) -> Option<u64> {
    const MARKERS: [&str; 4] = [
        "retry-after=",
        "retry-after:",
        "retry_after=",
        "\"retry-after\":",
    ];
    MARKERS.iter().find_map(|marker| {
        let value = message.split(marker).nth(1)?.trim_start();
        let digits = value
            .trim_start_matches(['\'', '"'])
            .chars()
            .take_while(|ch| ch.is_ascii_digit())
            .collect::<String>();
        digits.parse::<u64>().ok()
    })
}

pub(crate) fn codex_token_selection_score(
    state: &AppState,
    token_idx: usize,
    token: &UpstreamToken,
) -> AccountSelectionScore {
    let quota_pressure = {
        let cache = state.quota_cache.lock().unwrap();
        cache
            .get(token_idx)
            .and_then(|entry| entry.as_ref())
            .and_then(|entry| entry.error.is_none().then(|| &entry.summary))
            .and_then(codex_quota_pressure)
    };
    usage_backed_selection_score(
        state,
        Provider::Codex,
        codex_stats_key(token),
        quota_pressure,
    )
}

pub(crate) fn antigravity_account_selection_score(
    state: &AppState,
    account: &target::antigravity::accounts::AntigravityAccount,
) -> AccountSelectionScore {
    let key = quota_cache_key(account.file_name.as_deref(), &account.label);
    let quota_pressure = {
        let cache = state.agw_quota_cache.lock().unwrap();
        cache
            .get(&key)
            .and_then(|entry| entry.error.is_none().then(|| &entry.summary))
            .and_then(|summary| google_quota_pressure(&summary.groups, &summary.models))
    };
    usage_backed_selection_score(
        state,
        Provider::Antigravity,
        antigravity_stats_key(account),
        quota_pressure,
    )
}

pub(crate) fn gemini_account_selection_score(
    state: &AppState,
    account: &target::gemini::accounts::GeminiAccount,
) -> AccountSelectionScore {
    let key = quota_cache_key(account.file_name.as_deref(), &account.label);
    let quota_pressure = {
        let cache = state.gemini_quota_cache.lock().unwrap();
        cache
            .get(&key)
            .and_then(|entry| entry.error.is_none().then(|| &entry.summary))
            .and_then(|summary| gemini_quota_pressure(&summary.groups, &summary.models))
    };
    usage_backed_selection_score(
        state,
        Provider::Gemini,
        gemini_stats_key(account),
        quota_pressure,
    )
}

pub(crate) fn qwen_account_selection_score(
    state: &AppState,
    account: &target::qwen::accounts::QwenAccount,
) -> AccountSelectionScore {
    let key = quota_cache_key(account.file_name.as_deref(), &account.label);
    let quota_pressure = {
        let cache = state.qwen_quota_cache.lock().unwrap();
        cache
            .get(&key)
            .and_then(|entry| entry.error.is_none().then(|| &entry.summary))
            .and_then(|summary| qwen_quota_pressure(&summary.limits))
    };
    usage_backed_selection_score(
        state,
        Provider::Qwen,
        qwen_stats_key(account),
        quota_pressure,
    )
}

pub(crate) fn deepseek_account_selection_score(
    state: &AppState,
    account: &target::deepseek::accounts::DeepSeekAccount,
) -> AccountSelectionScore {
    let key = quota_cache_key(account.file_name.as_deref(), &account.label);
    let quota_pressure = {
        let cache = state.deepseek_quota_cache.lock().unwrap();
        cache
            .get(&key)
            .and_then(|entry| entry.error.is_none().then(|| &entry.summary))
            .and_then(deepseek_balance_pressure)
    };
    usage_backed_selection_score(
        state,
        Provider::DeepSeek,
        deepseek_stats_key(account),
        quota_pressure,
    )
}

pub(crate) fn minimax_account_selection_score(
    state: &AppState,
    account: &target::minimax::accounts::MiniMaxAccount,
) -> AccountSelectionScore {
    let key = quota_cache_key(account.file_name.as_deref(), &account.label);
    let quota_pressure = {
        let cache = state.minimax_quota_cache.lock().unwrap();
        cache
            .get(&key)
            .and_then(|entry| entry.error.is_none().then(|| &entry.summary))
            .and_then(minimax_quota_pressure)
    };
    usage_backed_selection_score(
        state,
        Provider::MiniMax,
        minimax_stats_key(account),
        quota_pressure,
    )
}

pub(crate) fn grok_account_selection_score(
    state: &AppState,
    account: &target::grok::accounts::GrokAccount,
) -> AccountSelectionScore {
    usage_backed_selection_score(
        state,
        Provider::Grok,
        grok_stats_key(account),
        grok_rate_limit_pressure(&account.rate_limits),
    )
}

pub(crate) fn copilot_account_selection_score(
    state: &AppState,
    account: &target::copilot::accounts::CopilotAccount,
) -> AccountSelectionScore {
    usage_backed_selection_score(state, Provider::Copilot, copilot_stats_key(account), None)
}

pub(crate) fn claude_account_selection_score(
    state: &AppState,
    account: &target::claude::accounts::ClaudeAccount,
) -> AccountSelectionScore {
    usage_backed_selection_score(state, Provider::Claude, claude_stats_key(account), None)
}

pub(crate) fn glm_account_selection_score(
    state: &AppState,
    account: &target::glm::accounts::GlmAccount,
) -> AccountSelectionScore {
    usage_backed_selection_score(state, Provider::Glm, glm_stats_key(account), None)
}

fn codex_quota_pressure(summary: &target::codex::quota::QuotaSummary) -> Option<f64> {
    let mut pressure = None;
    push_quota_pressure(
        &mut pressure,
        summary
            .code_generation
            .five_hour
            .as_ref()
            .and_then(|bucket| bucket.used_percent),
    );
    push_quota_pressure(
        &mut pressure,
        summary
            .code_generation
            .weekly
            .as_ref()
            .and_then(|bucket| bucket.used_percent),
    );
    push_quota_pressure(
        &mut pressure,
        summary
            .code_review
            .five_hour
            .as_ref()
            .and_then(|bucket| bucket.used_percent),
    );
    push_quota_pressure(
        &mut pressure,
        summary
            .code_review
            .weekly
            .as_ref()
            .and_then(|bucket| bucket.used_percent),
    );
    for limit in &summary.additional_rate_limits {
        push_quota_pressure(
            &mut pressure,
            limit
                .five_hour
                .as_ref()
                .and_then(|bucket| bucket.used_percent),
        );
        push_quota_pressure(
            &mut pressure,
            limit.weekly.as_ref().and_then(|bucket| bucket.used_percent),
        );
    }
    pressure
}

fn google_quota_pressure(
    groups: &[target::antigravity::quota::QuotaGroupSummary],
    models: &[target::antigravity::quota::ModelQuotaSummary],
) -> Option<f64> {
    let mut pressure = None;
    for group in groups {
        push_quota_pressure(
            &mut pressure,
            group
                .five_hour
                .as_ref()
                .and_then(|bucket| bucket.used_percent),
        );
        push_quota_pressure(
            &mut pressure,
            group.weekly.as_ref().and_then(|bucket| bucket.used_percent),
        );
    }
    for model in models {
        push_quota_pressure(
            &mut pressure,
            model
                .current
                .as_ref()
                .and_then(|bucket| bucket.used_percent),
        );
        push_quota_pressure(
            &mut pressure,
            model
                .five_hour
                .as_ref()
                .and_then(|bucket| bucket.used_percent),
        );
        push_quota_pressure(
            &mut pressure,
            model.weekly.as_ref().and_then(|bucket| bucket.used_percent),
        );
    }
    pressure
}

fn gemini_quota_pressure(
    groups: &[target::gemini::quota::QuotaGroupSummary],
    models: &[target::gemini::quota::ModelQuotaSummary],
) -> Option<f64> {
    let mut pressure = None;
    for group in groups {
        push_quota_pressure(
            &mut pressure,
            group
                .five_hour
                .as_ref()
                .and_then(|bucket| bucket.used_percent),
        );
        push_quota_pressure(
            &mut pressure,
            group.weekly.as_ref().and_then(|bucket| bucket.used_percent),
        );
    }
    for model in models {
        push_quota_pressure(
            &mut pressure,
            model
                .current
                .as_ref()
                .and_then(|bucket| bucket.used_percent),
        );
        push_quota_pressure(
            &mut pressure,
            model
                .five_hour
                .as_ref()
                .and_then(|bucket| bucket.used_percent),
        );
        push_quota_pressure(
            &mut pressure,
            model.weekly.as_ref().and_then(|bucket| bucket.used_percent),
        );
    }
    pressure
}

fn qwen_quota_pressure(limits: &[target::qwen::quota::RateLimitSummary]) -> Option<f64> {
    let mut pressure = None;
    for limit in limits {
        push_quota_pressure(&mut pressure, limit.used_percent);
    }
    pressure
}

fn deepseek_balance_pressure(summary: &target::deepseek::quota::QuotaSummary) -> Option<f64> {
    if !summary.is_available {
        return Some(f64::INFINITY);
    }
    if !summary.has_balance {
        return None;
    }
    let total_balance: f64 = summary
        .balances
        .iter()
        .filter_map(|balance| parse_balance_amount(&balance.total_balance))
        .sum();
    Some(-total_balance)
}

fn parse_balance_amount(value: &str) -> Option<f64> {
    let normalized = value.trim().replace(',', "");
    if normalized.is_empty() {
        return None;
    }
    normalized
        .parse::<f64>()
        .ok()
        .filter(|value| !value.is_nan())
}

fn minimax_quota_pressure(summary: &target::minimax::quota::QuotaSummary) -> Option<f64> {
    if !summary.is_available {
        return Some(f64::INFINITY);
    }
    let mut pressure = None;
    push_quota_pressure(
        &mut pressure,
        summary
            .current_window
            .as_ref()
            .and_then(|bucket| bucket.used_percent),
    );
    push_quota_pressure(
        &mut pressure,
        summary
            .weekly
            .as_ref()
            .and_then(|bucket| bucket.used_percent),
    );
    for model in &summary.models {
        push_quota_pressure(
            &mut pressure,
            model
                .current_window
                .as_ref()
                .and_then(|bucket| bucket.used_percent),
        );
        push_quota_pressure(
            &mut pressure,
            model.weekly.as_ref().and_then(|bucket| bucket.used_percent),
        );
    }
    pressure
}

fn grok_rate_limit_pressure(rate_limits: &[target::grok::auth::GrokRateLimitInfo]) -> Option<f64> {
    let mut pressure = None;
    for limit in rate_limits {
        push_quota_pressure(&mut pressure, limit.used_percent);
    }
    pressure
}

pub(crate) fn minimax_usage_context(
    account: &target::minimax::accounts::MiniMaxAccount,
    model: Option<String>,
    request_path: impl Into<String>,
    prompt: PromptMetrics,
) -> UsageContext {
    UsageContext {
        provider: Provider::MiniMax,
        provider_name: "minimax",
        key: minimax_stats_key(account),
        label: account.label.clone(),
        account_id: account.account_id.clone(),
        credential_file: account.file_name.clone(),
        model,
        request_path: request_path.into(),
        prompt,
    }
}

pub(crate) fn codex_usage_context(
    token: &UpstreamToken,
    model: Option<String>,
    request_path: impl Into<String>,
    prompt: PromptMetrics,
) -> UsageContext {
    UsageContext {
        provider: Provider::Codex,
        provider_name: "codex",
        key: codex_stats_key(token),
        label: token.label.clone(),
        account_id: token.account_id.clone().unwrap_or_default(),
        credential_file: token.file_name.clone(),
        model,
        request_path: request_path.into(),
        prompt,
    }
}

pub(crate) fn antigravity_usage_context(
    account: &target::antigravity::accounts::AntigravityAccount,
    model: Option<String>,
    request_path: impl Into<String>,
    prompt: PromptMetrics,
) -> UsageContext {
    UsageContext {
        provider: Provider::Antigravity,
        provider_name: "antigravity",
        key: antigravity_stats_key(account),
        label: account.label.clone(),
        account_id: account.email.clone(),
        credential_file: account.file_name.clone(),
        model,
        request_path: request_path.into(),
        prompt,
    }
}

pub(crate) fn qwen_usage_context(
    account: &target::qwen::accounts::QwenAccount,
    model: Option<String>,
    request_path: impl Into<String>,
    prompt: PromptMetrics,
) -> UsageContext {
    UsageContext {
        provider: Provider::Qwen,
        provider_name: "qwen",
        key: qwen_stats_key(account),
        label: account.label.clone(),
        account_id: if !account.account_id.trim().is_empty() {
            account.account_id.clone()
        } else {
            account
                .subject
                .clone()
                .unwrap_or_else(|| account.email.clone())
        },
        credential_file: account.file_name.clone(),
        model,
        request_path: request_path.into(),
        prompt,
    }
}

pub(crate) fn gemini_usage_context(
    account: &target::gemini::accounts::GeminiAccount,
    model: Option<String>,
    request_path: impl Into<String>,
    prompt: PromptMetrics,
) -> UsageContext {
    UsageContext {
        provider: Provider::Gemini,
        provider_name: "gemini",
        key: gemini_stats_key(account),
        label: account.label.clone(),
        account_id: account.email.clone(),
        credential_file: account.file_name.clone(),
        model,
        request_path: request_path.into(),
        prompt,
    }
}

pub(crate) fn deepseek_usage_context(
    account: &target::deepseek::accounts::DeepSeekAccount,
    model: Option<String>,
    request_path: impl Into<String>,
    prompt: PromptMetrics,
) -> UsageContext {
    UsageContext {
        provider: Provider::DeepSeek,
        provider_name: "deepseek",
        key: deepseek_stats_key(account),
        label: account.label.clone(),
        account_id: account.account_id.clone(),
        credential_file: account.file_name.clone(),
        model,
        request_path: request_path.into(),
        prompt,
    }
}

pub(crate) fn grok_usage_context(
    account: &target::grok::accounts::GrokAccount,
    model: Option<String>,
    request_path: impl Into<String>,
    prompt: PromptMetrics,
) -> UsageContext {
    UsageContext {
        provider: Provider::Grok,
        provider_name: "grok",
        key: grok_stats_key(account),
        label: account.label.clone(),
        account_id: account
            .user_id
            .clone()
            .or_else(|| account.email.clone())
            .unwrap_or_default(),
        credential_file: account.file_name.clone(),
        model,
        request_path: request_path.into(),
        prompt,
    }
}

pub(crate) fn copilot_usage_context(
    account: &target::copilot::accounts::CopilotAccount,
    model: Option<String>,
    request_path: impl Into<String>,
    prompt: PromptMetrics,
) -> UsageContext {
    UsageContext {
        provider: Provider::Copilot,
        provider_name: "copilot",
        key: copilot_stats_key(account),
        label: account.label.clone(),
        account_id: account.account_id.clone(),
        credential_file: account.file_name.clone(),
        model,
        request_path: request_path.into(),
        prompt,
    }
}

pub(crate) fn claude_usage_context(
    account: &target::claude::accounts::ClaudeAccount,
    model: Option<String>,
    request_path: impl Into<String>,
    prompt: PromptMetrics,
) -> UsageContext {
    UsageContext {
        provider: Provider::Claude,
        provider_name: "claude",
        key: claude_stats_key(account),
        label: account.label.clone(),
        account_id: account.account_id.clone(),
        credential_file: account.file_name.clone(),
        model,
        request_path: request_path.into(),
        prompt,
    }
}

pub(crate) fn glm_usage_context(
    account: &target::glm::accounts::GlmAccount,
    model: Option<String>,
    request_path: impl Into<String>,
    prompt: PromptMetrics,
) -> UsageContext {
    UsageContext {
        provider: Provider::Glm,
        provider_name: "glm",
        key: glm_stats_key(account),
        label: account.label.clone(),
        account_id: account.account_id.clone(),
        credential_file: account.file_name.clone(),
        model,
        request_path: request_path.into(),
        prompt,
    }
}

fn start_persistence_worker(
    cfg: Arc<Config>,
    persisted_stats: Arc<Mutex<stats_store::StatsStore>>,
    receiver: mpsc::Receiver<PersistenceEvent>,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("gateway-persistence".to_string())
        .spawn(move || {
            let mut stats_dirty = false;
            let mut pending_api_keys = None;
            let mut pending_history = Vec::new();
            let flush_interval = Duration::from_secs(2);
            let mut next_flush = std::time::Instant::now() + flush_interval;
            let mut last_history_maintenance = std::time::Instant::now()
                .checked_sub(Duration::from_secs(3600))
                .unwrap_or_else(std::time::Instant::now);

            loop {
                let wait = next_flush.saturating_duration_since(std::time::Instant::now());
                match receiver.recv_timeout(wait) {
                    Ok(PersistenceEvent::StatsDirty) => stats_dirty = true,
                    Ok(PersistenceEvent::History(entry)) => pending_history.push(entry),
                    Ok(PersistenceEvent::ApiKeys(snapshot)) => pending_api_keys = Some(snapshot),
                    Ok(PersistenceEvent::Shutdown(flushed)) => {
                        flush_persistence(
                            &cfg,
                            &persisted_stats,
                            &mut stats_dirty,
                            &mut pending_api_keys,
                            &mut pending_history,
                        );
                        let _ = flushed.send(());
                        break;
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        flush_persistence(
                            &cfg,
                            &persisted_stats,
                            &mut stats_dirty,
                            &mut pending_api_keys,
                            &mut pending_history,
                        );
                        break;
                    }
                }

                let now = std::time::Instant::now();
                if now >= next_flush || pending_history.len() >= 512 {
                    flush_persistence(
                        &cfg,
                        &persisted_stats,
                        &mut stats_dirty,
                        &mut pending_api_keys,
                        &mut pending_history,
                    );
                    next_flush = now + flush_interval;
                }
                if last_history_maintenance.elapsed() >= Duration::from_secs(3600) {
                    if let Err(err) = usage_store::prune(
                        &cfg,
                        cfg.history_retention_days,
                        cfg.history_max_entries,
                    ) {
                        error!("failed to prune usage history: {}", err);
                    }
                    last_history_maintenance = std::time::Instant::now();
                }
            }
        })
        .expect("failed to start persistence worker")
}

fn flush_persistence(
    cfg: &Config,
    persisted_stats: &Arc<Mutex<stats_store::StatsStore>>,
    stats_dirty: &mut bool,
    pending_api_keys: &mut Option<api_keys::ApiKeyStore>,
    pending_history: &mut Vec<usage_store::UsageHistoryEntry>,
) {
    if *stats_dirty {
        let snapshot = persisted_stats.lock().unwrap().clone();
        if let Err(err) = stats_store::save(cfg, &snapshot) {
            error!("failed to persist usage stats: {}", err);
        }
        *stats_dirty = false;
    }
    if let Some(snapshot) = pending_api_keys.take() {
        if let Err(err) = api_keys::save(cfg, &snapshot) {
            error!("failed to persist API key store: {}", err);
        }
    }
    if !pending_history.is_empty() {
        let entries = std::mem::take(pending_history);
        if let Err(err) = usage_store::append_batch(cfg, &entries) {
            error!("failed to append usage history: {}", err);
        }
    }
}

fn append_usage_history(state: &AppState, entry: usage_store::UsageHistoryEntry) {
    if state
        .persistence_tx
        .send(PersistenceEvent::History(entry))
        .is_err()
    {
        error!("persistence worker is unavailable; usage history was dropped");
    }
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}

fn earliest_timestamp(current: Option<String>, incoming: Option<String>) -> Option<String> {
    match (current, incoming) {
        (Some(current), Some(incoming)) => Some(std::cmp::min(current, incoming)),
        (Some(current), None) => Some(current),
        (None, Some(incoming)) => Some(incoming),
        (None, None) => None,
    }
}

fn latest_timestamp(current: Option<String>, incoming: Option<String>) -> Option<String> {
    match (current, incoming) {
        (Some(current), Some(incoming)) => Some(std::cmp::max(current, incoming)),
        (Some(current), None) => Some(current),
        (None, Some(incoming)) => Some(incoming),
        (None, None) => None,
    }
}

fn merge_latest_error_details(
    current_at: Option<String>,
    current_message: Option<String>,
    incoming_at: Option<String>,
    incoming_message: Option<String>,
) -> (Option<String>, Option<String>) {
    match (current_at, incoming_at) {
        (Some(current_at), Some(incoming_at)) if current_at > incoming_at => {
            (Some(current_at), current_message)
        }
        (Some(current_at), Some(incoming_at)) if incoming_at > current_at => {
            (Some(incoming_at), incoming_message)
        }
        (Some(current_at), Some(_)) => (Some(current_at), incoming_message.or(current_message)),
        (Some(current_at), None) => (Some(current_at), current_message),
        (None, Some(incoming_at)) => (Some(incoming_at), incoming_message),
        (None, None) => (None, current_message.or(incoming_message)),
    }
}

fn provider_from_history_name(name: &str) -> Option<Provider> {
    if name.eq_ignore_ascii_case("codex") {
        Some(Provider::Codex)
    } else if name.eq_ignore_ascii_case("antigravity") {
        Some(Provider::Antigravity)
    } else if name.eq_ignore_ascii_case("gemini") {
        Some(Provider::Gemini)
    } else if name.eq_ignore_ascii_case("qwen") {
        Some(Provider::Qwen)
    } else if name.eq_ignore_ascii_case("deepseek") {
        Some(Provider::DeepSeek)
    } else if name.eq_ignore_ascii_case("grok") {
        Some(Provider::Grok)
    } else if name.eq_ignore_ascii_case("minimax") {
        Some(Provider::MiniMax)
    } else if name.eq_ignore_ascii_case("copilot") {
        Some(Provider::Copilot)
    } else if name.eq_ignore_ascii_case("claude") {
        Some(Provider::Claude)
    } else if name.eq_ignore_ascii_case("glm") {
        Some(Provider::Glm)
    } else {
        None
    }
}

fn backfill_last_error_messages_from_history(state: &AppState) {
    let latest_messages = match stats_store::load_latest_error_messages(&state.cfg) {
        Ok(messages) => messages,
        Err(err) => {
            error!(
                "failed to load latest error messages from usage history: {}",
                err
            );
            return;
        }
    };
    if latest_messages.is_empty() {
        return;
    }

    let mut changed = false;
    {
        let mut persisted = state.persisted_stats.lock().unwrap();
        for ((provider_name, account_key), latest) in latest_messages {
            let Some(provider) = provider_from_history_name(&provider_name) else {
                continue;
            };
            let entry = persisted.account_usage_mut(provider, account_key);
            if entry.last_error_at.as_deref() != Some(latest.recorded_at.as_str()) {
                continue;
            }
            if entry
                .last_error_message
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .is_some()
            {
                continue;
            }
            entry.last_error_message = Some(latest.error_message);
            changed = true;
        }
    }
    if changed {
        persist_stats_store(state);
    }
}

fn persist_stats_store(state: &AppState) {
    let snapshot = state.persisted_stats.lock().unwrap().clone();
    if let Err(err) = stats_store::save(&state.cfg, &snapshot) {
        error!("failed to persist usage stats: {}", err);
    }
}

pub(crate) fn check_api_key(state: &AppState, headers: &HeaderMap) -> bool {
    authenticate_api_key(state, headers).is_some()
}

fn authenticate_api_key(state: &AppState, headers: &HeaderMap) -> Option<AuthenticatedApiKey> {
    if headers
        .get("x-internal-proxy-key")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.trim() == state.internal_proxy_secret.as_str())
    {
        return Some(AuthenticatedApiKey {
            id: None,
            access: api_keys::ApiKeyAccess::default(),
        });
    }
    let Some(token) = extract_api_key(headers) else {
        return None;
    };
    let now = now_rfc3339();
    let lookup_hash = api_keys::token_lookup_hash(token);
    let cached = state
        .api_key_cache
        .read()
        .unwrap()
        .get(&lookup_hash)
        .cloned();
    let authenticated = if let Some(ApiKeyCacheEntry::Verified(authenticated)) = cached {
        // Administrative mutations invalidate the whole cache before they
        // become visible. A cache hit therefore needs no API-key store lock.
        Some(authenticated)
    } else if let Some(ApiKeyCacheEntry::Rejected(rejected_at)) = cached {
        if rejected_at.elapsed() < Duration::from_secs(5) {
            return None;
        }
        state.api_key_cache.write().unwrap().remove(&lookup_hash);
        verify_api_key_cache_miss(state, token, &lookup_hash)
    } else {
        verify_api_key_cache_miss(state, token, &lookup_hash)
    };
    let Some(authenticated) = authenticated else {
        return None;
    };

    // Last-used metadata is throttled to one update per minute and flushed
    // by the persistence worker, so it does not add a disk write per request.
    let should_touch = authenticated.id.as_ref().is_some_and(|key_id| {
        let mut touched = state.api_key_last_used.lock().unwrap();
        let bucket = now.get(..16).unwrap_or(&now).to_string();
        if touched.get(key_id) == Some(&bucket) {
            false
        } else {
            touched.insert(key_id.clone(), bucket);
            true
        }
    });
    if should_touch {
        let snapshot = {
            let mut store = state.api_keys.lock().unwrap();
            if api_keys::touch_last_used(
                &mut store,
                authenticated.id.as_deref().unwrap_or_default(),
                &now,
            ) {
                Some(store.clone())
            } else {
                None
            }
        };
        if let Some(snapshot) = snapshot {
            let _ = state
                .persistence_tx
                .send(PersistenceEvent::ApiKeys(snapshot));
        }
    }
    Some(authenticated)
}

fn verify_api_key_cache_miss(
    state: &AppState,
    token: &str,
    lookup_hash: &str,
) -> Option<AuthenticatedApiKey> {
    // Clone only the lookup candidates while holding the short-lived mutex.
    // Argon2 verification runs after the mutex is released.
    let candidates = {
        let store = state.api_keys.lock().unwrap();
        api_keys::verification_candidates(&store, token)
    };
    let verified = candidates
        .iter()
        .find(|record| api_keys::verify_record(record, token))
        .map(|record| AuthenticatedApiKey {
            id: Some(record.id.clone()),
            access: record.access.clone(),
        });
    let cache_entry = verified
        .as_ref()
        .map(|authenticated| ApiKeyCacheEntry::Verified(authenticated.clone()))
        .unwrap_or_else(|| ApiKeyCacheEntry::Rejected(std::time::Instant::now()));
    state
        .api_key_cache
        .write()
        .unwrap()
        .insert(lookup_hash.to_string(), cache_entry);
    verified
}

fn extract_api_key(headers: &HeaderMap) -> Option<&str> {
    let auth = headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    if let Some(token) = auth.strip_prefix("Bearer ") {
        return Some(token.trim());
    }
    headers
        .get("x-api-key")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn require_admin_session_json(state: &AppState, headers: &HeaderMap) -> Option<Response> {
    if !admin_auth::is_enabled(&state.cfg.admin_auth) {
        return None;
    }
    let mut sessions = state.admin_sessions.lock().unwrap();
    if admin_auth::validate_session(headers, &mut sessions) {
        return None;
    }
    Some(
        (
            StatusCode::UNAUTHORIZED,
            [("Content-Type", "application/json")],
            serde_json::to_vec(&serde_json::json!({
                "ok": false,
                "message": "admin login required"
            }))
            .unwrap_or_default(),
        )
            .into_response(),
    )
}

fn require_admin_session_text(state: &AppState, headers: &HeaderMap) -> Option<Response> {
    if !admin_auth::is_enabled(&state.cfg.admin_auth) {
        return None;
    }
    let mut sessions = state.admin_sessions.lock().unwrap();
    if admin_auth::validate_session(headers, &mut sessions) {
        return None;
    }
    Some(
        (
            StatusCode::UNAUTHORIZED,
            [("Content-Type", "text/plain; charset=utf-8")],
            "admin login required".to_string(),
        )
            .into_response(),
    )
}

fn candidate_tokens_with_reservation(
    state: &AppState,
    reserve_first: bool,
) -> Vec<(usize, UpstreamToken)> {
    let mut idx = state.rr.lock().unwrap();
    let tokens = state.tokens.lock().unwrap().clone();
    if tokens.is_empty() {
        return Vec::new();
    }
    let len = tokens.len();
    let priority_accounts = routing_priority_accounts_for_provider(state, "codex");
    let picked_indices = select_ordered_account_indices_with_priority(
        len,
        *idx,
        |candidate_idx| {
            tokens[candidate_idx].enabled
                && router_account_eligible(state, "codex", &codex_stats_key(&tokens[candidate_idx]))
        },
        |candidate_idx| priority_accounts.contains(&codex_stats_key(&tokens[candidate_idx])),
        |candidate_idx| codex_token_selection_score(state, candidate_idx, &tokens[candidate_idx]),
    );
    if let Some(picked_idx) = picked_indices.first() {
        *idx = (picked_idx + 1) % len;
        if reserve_first {
            router_reserve_account(state, "codex", &codex_stats_key(&tokens[*picked_idx]));
        }
    }
    picked_indices
        .into_iter()
        .map(|candidate_idx| (candidate_idx, tokens[candidate_idx].clone()))
        .collect()
}

fn stats_accounts_mut(stats: &mut UsageStats, provider: Provider) -> &mut Vec<AccountUsage> {
    match provider {
        Provider::Codex => &mut stats.codex_accounts,
        Provider::Antigravity => &mut stats.agw_accounts,
        Provider::Gemini => &mut stats.gemini_accounts,
        Provider::Qwen => &mut stats.qwen_accounts,
        Provider::DeepSeek => &mut stats.deepseek_accounts,
        Provider::Grok => &mut stats.grok_accounts,
        Provider::MiniMax => &mut stats.minimax_accounts,
        Provider::Copilot => &mut stats.copilot_accounts,
        Provider::Claude => &mut stats.claude_accounts,
        Provider::Glm => &mut stats.glm_accounts,
    }
}

fn apply_counter_delta_to_stats(
    stats: &mut UsageStats,
    provider: Provider,
    key: &str,
    label: &str,
    account_id: &str,
    delta: &CounterDelta,
) {
    if delta.request_delta > 0 {
        stats.total_requests += delta.request_delta;
    }
    if delta.error_delta > 0 {
        stats.total_errors += delta.error_delta;
    }
    stats.total_prompt_total += delta.prompt_total_delta;
    stats.total_prompt_error_total += delta.prompt_error_total_delta;
    stats.total_input_tokens += delta.input_tokens_delta;
    stats.total_output_tokens += delta.output_tokens_delta;
    stats.total_tokens_used += delta.total_tokens_delta;
    stats.total_cache_tokens += delta.cache_tokens_delta;
    stats.total_reasoning_tokens += delta.reasoning_tokens_delta;
    if let Some(observed_at) = delta.observed_at.clone() {
        if stats.first_recorded_at.is_none() {
            stats.first_recorded_at = Some(observed_at.clone());
        }
        stats.last_recorded_at = Some(observed_at);
    }

    let accounts = stats_accounts_mut(stats, provider);
    let entry_index = accounts.iter().position(|entry| entry.key == key);
    let entry_index = entry_index.unwrap_or_else(|| {
        accounts.push(AccountUsage {
            key: key.to_string(),
            ..Default::default()
        });
        accounts.len() - 1
    });
    let entry = &mut accounts[entry_index];
    entry.label = label.to_string();
    entry.account_id = account_id.to_string();
    entry.requests += delta.request_delta;
    entry.errors += delta.error_delta;
    entry.prompt_total += delta.prompt_total_delta;
    entry.prompt_error_total += delta.prompt_error_total_delta;
    entry.input_tokens += delta.input_tokens_delta;
    entry.output_tokens += delta.output_tokens_delta;
    entry.total_tokens += delta.total_tokens_delta;
    entry.cache_tokens += delta.cache_tokens_delta;
    entry.reasoning_tokens += delta.reasoning_tokens_delta;
    if let Some(observed_at) = delta.observed_at.clone() {
        if entry.first_seen_at.is_none() {
            entry.first_seen_at = Some(observed_at.clone());
        }
        entry.last_seen_at = Some(observed_at);
    }
    if let Some(success_at) = delta.success_at.clone() {
        entry.last_success_at = Some(success_at);
    }
    if let Some(error_at) = delta.error_at.clone() {
        entry.last_error_at = Some(error_at);
    }
    if let Some(error_message) = delta.error_message.clone() {
        entry.last_error_message = Some(error_message);
    }
}

fn update_account_counters(
    state: &AppState,
    provider: Provider,
    key: String,
    label: String,
    account_id: String,
    delta: CounterDelta,
) {
    let stats_delta = delta.clone();
    {
        let mut persisted = state.persisted_stats.lock().unwrap();
        if delta.request_delta > 0 {
            persisted.total_requests += delta.request_delta;
        }
        if delta.error_delta > 0 {
            persisted.total_errors += delta.error_delta;
        }
        persisted.total_prompt_total += delta.prompt_total_delta;
        persisted.total_prompt_error_total += delta.prompt_error_total_delta;
        persisted.total_input_tokens += delta.input_tokens_delta;
        persisted.total_output_tokens += delta.output_tokens_delta;
        persisted.total_tokens_used += delta.total_tokens_delta;
        persisted.total_cache_tokens += delta.cache_tokens_delta;
        persisted.total_reasoning_tokens += delta.reasoning_tokens_delta;
        if let Some(observed_at) = delta.observed_at.clone() {
            if persisted.first_recorded_at.is_none() {
                persisted.first_recorded_at = Some(observed_at.clone());
            }
            persisted.last_recorded_at = Some(observed_at);
        }
        let entry = persisted.account_usage_mut(provider, key.clone());
        entry.label = label.clone();
        entry.account_id = account_id.clone();
        entry.requests += delta.request_delta;
        entry.errors += delta.error_delta;
        entry.prompt_total += delta.prompt_total_delta;
        entry.prompt_error_total += delta.prompt_error_total_delta;
        entry.input_tokens += delta.input_tokens_delta;
        entry.output_tokens += delta.output_tokens_delta;
        entry.total_tokens += delta.total_tokens_delta;
        entry.cache_tokens += delta.cache_tokens_delta;
        entry.reasoning_tokens += delta.reasoning_tokens_delta;
        if let Some(observed_at) = delta.observed_at {
            if entry.first_seen_at.is_none() {
                entry.first_seen_at = Some(observed_at.clone());
            }
            entry.last_seen_at = Some(observed_at);
        }
        if let Some(success_at) = delta.success_at {
            entry.last_success_at = Some(success_at);
        }
        if let Some(error_at) = delta.error_at {
            entry.last_error_at = Some(error_at);
        }
        if let Some(error_message) = delta.error_message {
            entry.last_error_message = Some(error_message);
        }
    }
    {
        let mut stats = state.stats.lock().unwrap();
        apply_counter_delta_to_stats(
            &mut stats,
            provider,
            &key,
            &label,
            &account_id,
            &stats_delta,
        );
    }
    let _ = state.persistence_tx.send(PersistenceEvent::StatsDirty);
}

fn record_request_started(state: &AppState, context: &UsageContext) {
    router_request_started(state, context.provider_name, &context.key);
    update_account_counters(
        state,
        context.provider,
        context.key.clone(),
        context.label.clone(),
        context.account_id.clone(),
        CounterDelta {
            request_delta: 1,
            prompt_total_delta: if context.prompt.is_prompt { 1 } else { 0 },
            observed_at: Some(now_rfc3339()),
            ..Default::default()
        },
    );
}

fn record_request_error(state: &AppState, context: &UsageContext, message: impl Into<String>) {
    record_request_error_with_health(state, context, message, true);
}

fn record_request_error_with_health(
    state: &AppState,
    context: &UsageContext,
    message: impl Into<String>,
    affects_account_health: bool,
) {
    let observed_at = now_rfc3339();
    let message = message.into();
    if affects_account_health {
        router_request_finished(
            state,
            context.provider_name,
            &context.key,
            false,
            Some(&message),
        );
    } else {
        router_request_finished_neutral(state, context.provider_name, &context.key);
    }
    update_account_counters(
        state,
        context.provider,
        context.key.clone(),
        context.label.clone(),
        context.account_id.clone(),
        CounterDelta {
            error_delta: 1,
            prompt_error_total_delta: if context.prompt.is_prompt { 1 } else { 0 },
            observed_at: Some(observed_at.clone()),
            error_at: Some(observed_at.clone()),
            error_message: Some(message.clone()),
            ..Default::default()
        },
    );
    notifications::notify_error(state, context, &message, &observed_at);
    if context.prompt.is_prompt {
        append_usage_history(
            state,
            usage_store::UsageHistoryEntry {
                recorded_at: observed_at,
                provider: context.provider_name.to_string(),
                account_key: context.key.clone(),
                account_label: context.label.clone(),
                account_id: context.account_id.clone(),
                credential_file: context.credential_file.clone(),
                model: context.model.clone(),
                request_path: context.request_path.clone(),
                api_key_id: state.request_api_key_id.clone(),
                success: false,
                error: true,
                request_total: 1,
                prompt_total: 1,
                prompt_error_total: 1,
                input_tokens: 0,
                output_tokens: 0,
                total_tokens: 0,
                cache_tokens: 0,
                reasoning_tokens: 0,
                input_chars: context.prompt.input_chars,
                prompt_items: context.prompt.prompt_items,
                error_message: Some(message),
                raw_usage: None,
            },
        );
    }
}

fn record_usage_success(state: &AppState, context: &UsageContext, metrics: &UsageMetrics) {
    let observed_at = now_rfc3339();
    router_request_finished(state, context.provider_name, &context.key, true, None);
    update_account_counters(
        state,
        context.provider,
        context.key.clone(),
        context.label.clone(),
        context.account_id.clone(),
        CounterDelta {
            input_tokens_delta: metrics.input_tokens,
            output_tokens_delta: metrics.output_tokens,
            total_tokens_delta: metrics.total_tokens,
            cache_tokens_delta: metrics.cache_tokens,
            reasoning_tokens_delta: metrics.reasoning_tokens,
            observed_at: Some(observed_at.clone()),
            success_at: Some(observed_at.clone()),
            ..Default::default()
        },
    );
    if context.prompt.is_prompt {
        append_usage_history(
            state,
            usage_store::UsageHistoryEntry {
                recorded_at: observed_at,
                provider: context.provider_name.to_string(),
                account_key: context.key.clone(),
                account_label: context.label.clone(),
                account_id: context.account_id.clone(),
                credential_file: context.credential_file.clone(),
                model: context.model.clone(),
                request_path: context.request_path.clone(),
                api_key_id: state.request_api_key_id.clone(),
                success: true,
                error: false,
                request_total: 1,
                prompt_total: 1,
                prompt_error_total: 0,
                input_tokens: metrics.input_tokens,
                output_tokens: metrics.output_tokens,
                total_tokens: metrics.total_tokens,
                cache_tokens: metrics.cache_tokens,
                reasoning_tokens: metrics.reasoning_tokens,
                input_chars: context.prompt.input_chars,
                prompt_items: context.prompt.prompt_items,
                error_message: None,
                raw_usage: metrics.raw_usage.clone(),
            },
        );
    }
}

fn record_codex_request(state: &AppState, context: &UsageContext) {
    record_request_started(state, context);
}

fn record_codex_error(state: &AppState, context: &UsageContext, message: impl Into<String>) {
    let message = message.into();
    let affects_account_health = codex_error_affects_account_health(&message);
    record_codex_error_with_health(state, context, message, affects_account_health);
}

fn record_codex_error_with_health(
    state: &AppState,
    context: &UsageContext,
    message: impl Into<String>,
    affects_account_health: bool,
) {
    record_request_error_with_health(state, context, message, affects_account_health);
}

pub(crate) fn record_antigravity_request(state: &AppState, context: &UsageContext) {
    record_request_started(state, context);
}

pub(crate) fn record_antigravity_error(
    state: &AppState,
    context: &UsageContext,
    message: impl Into<String>,
) {
    record_request_error(state, context, message);
}

pub(crate) fn record_antigravity_success(
    state: &AppState,
    context: &UsageContext,
    metrics: &UsageMetrics,
) {
    record_usage_success(state, context, metrics);
}

pub(crate) fn record_qwen_request(state: &AppState, context: &UsageContext) {
    record_request_started(state, context);
}

pub(crate) fn record_qwen_error(
    state: &AppState,
    context: &UsageContext,
    message: impl Into<String>,
) {
    record_request_error(state, context, message);
}

pub(crate) fn record_qwen_success(
    state: &AppState,
    context: &UsageContext,
    metrics: &UsageMetrics,
) {
    record_usage_success(state, context, metrics);
}

pub(crate) fn record_gemini_request(state: &AppState, context: &UsageContext) {
    record_request_started(state, context);
}

pub(crate) fn record_gemini_error(
    state: &AppState,
    context: &UsageContext,
    message: impl Into<String>,
) {
    record_request_error(state, context, message);
}

pub(crate) fn record_gemini_success(
    state: &AppState,
    context: &UsageContext,
    metrics: &UsageMetrics,
) {
    record_usage_success(state, context, metrics);
}

pub(crate) fn record_deepseek_request(state: &AppState, context: &UsageContext) {
    record_request_started(state, context);
}

pub(crate) fn record_deepseek_error(
    state: &AppState,
    context: &UsageContext,
    message: impl Into<String>,
) {
    record_request_error(state, context, message);
}

pub(crate) fn record_deepseek_success(
    state: &AppState,
    context: &UsageContext,
    metrics: &UsageMetrics,
) {
    record_usage_success(state, context, metrics);
}

pub(crate) fn record_grok_error(
    state: &AppState,
    context: &UsageContext,
    message: impl Into<String>,
) {
    record_request_error(state, context, message);
}

pub(crate) fn record_grok_success(
    state: &AppState,
    context: &UsageContext,
    metrics: &UsageMetrics,
) {
    record_usage_success(state, context, metrics);
}

pub(crate) fn record_minimax_request(state: &AppState, context: &UsageContext) {
    record_request_started(state, context);
}

pub(crate) fn record_minimax_error(
    state: &AppState,
    context: &UsageContext,
    message: impl Into<String>,
) {
    record_request_error(state, context, message);
}

pub(crate) fn record_minimax_success(
    state: &AppState,
    context: &UsageContext,
    metrics: &UsageMetrics,
) {
    record_usage_success(state, context, metrics);
}

pub(crate) fn record_copilot_request(state: &AppState, context: &UsageContext) {
    record_request_started(state, context);
}

pub(crate) fn record_copilot_error(
    state: &AppState,
    context: &UsageContext,
    message: impl Into<String>,
) {
    record_request_error(state, context, message);
}

pub(crate) fn record_copilot_success(
    state: &AppState,
    context: &UsageContext,
    metrics: &UsageMetrics,
) {
    record_usage_success(state, context, metrics);
}

pub(crate) fn record_claude_request(state: &AppState, context: &UsageContext) {
    record_request_started(state, context);
}

pub(crate) fn record_claude_error(
    state: &AppState,
    context: &UsageContext,
    message: impl Into<String>,
) {
    record_request_error(state, context, message);
}

pub(crate) fn record_claude_success(
    state: &AppState,
    context: &UsageContext,
    metrics: &UsageMetrics,
) {
    record_usage_success(state, context, metrics);
}

pub(crate) fn record_glm_request(state: &AppState, context: &UsageContext) {
    record_request_started(state, context);
}

pub(crate) fn record_glm_error(
    state: &AppState,
    context: &UsageContext,
    message: impl Into<String>,
) {
    record_request_error(state, context, message);
}

pub(crate) fn record_glm_success(state: &AppState, context: &UsageContext, metrics: &UsageMetrics) {
    record_usage_success(state, context, metrics);
}

fn is_hop_header(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
    )
}

fn augment_codex_models_json(body: &Bytes, state: &AppState) -> Bytes {
    let Ok(mut value) = serde_json::from_slice::<serde_json::Value>(body) else {
        return body.clone();
    };
    let Some(root) = value.as_object_mut() else {
        return body.clone();
    };
    if !root.get("models").is_some_and(|value| value.is_array()) {
        root.insert("models".to_string(), serde_json::json!([]));
    }
    let models = root
        .get_mut("models")
        .and_then(|value| value.as_array_mut())
        .expect("models was initialized as an array");

    append_prefixed_codex_model_aliases(models);

    let mut existing = models
        .iter()
        .filter_map(|model| model.get("slug").and_then(|value| value.as_str()))
        .map(|slug| slug.to_string())
        .collect::<HashSet<_>>();

    for model in codex_provider_model_metadata(state) {
        let Some(slug) = model.get("slug").and_then(|value| value.as_str()) else {
            continue;
        };
        if existing.insert(slug.to_string()) {
            models.push(model);
        }
    }

    serde_json::to_vec(&value)
        .map(Bytes::from)
        .unwrap_or_else(|_| body.clone())
}

fn append_prefixed_codex_model_aliases(models: &mut Vec<serde_json::Value>) {
    let mut existing = models
        .iter()
        .filter_map(|model| model.get("slug").and_then(|value| value.as_str()))
        .map(|slug| slug.to_string())
        .collect::<HashSet<_>>();

    let aliases = models
        .iter()
        .filter_map(|model| {
            let slug = model.get("slug").and_then(|value| value.as_str())?;
            if slug.contains(':') {
                return None;
            }

            let prefixed_slug = format!("cod:{}", slug);
            if !existing.insert(prefixed_slug.clone()) {
                return None;
            }

            let mut alias = model.clone();
            let object = alias.as_object_mut()?;
            object.insert(
                "slug".to_string(),
                serde_json::Value::String(prefixed_slug.clone()),
            );
            if object.contains_key("id") {
                object.insert("id".to_string(), serde_json::Value::String(prefixed_slug));
            }
            Some(alias)
        })
        .collect::<Vec<_>>();

    models.extend(aliases);
}

fn codex_provider_model_metadata(state: &AppState) -> Vec<serde_json::Value> {
    let mut models = Vec::new();
    if state
        .deepseek_accounts
        .lock()
        .unwrap()
        .iter()
        .any(|account| account.enabled)
    {
        models.push(codex_provider_model(
            "dsk:deepseek-v4-pro",
            "DeepSeek V4 Pro",
            "DeepSeek model routed through the configured DeepSeek account.",
            64_000,
            true,
            true,
        ));
        models.push(codex_provider_model(
            "dsk:deepseek-v4-flash",
            "DeepSeek V4 Flash",
            "Fast DeepSeek model routed through the configured DeepSeek account.",
            64_000,
            false,
            true,
        ));
    }
    if state
        .gemini_accounts
        .lock()
        .unwrap()
        .iter()
        .any(|account| account.enabled)
    {
        models.push(codex_provider_model(
            "gem:gemini-2.5-pro",
            "Gemini 2.5 Pro",
            "Gemini model routed through the configured Gemini account.",
            1_048_576,
            true,
            true,
        ));
        models.push(codex_provider_model(
            "gem:gemini-2.5-flash",
            "Gemini 2.5 Flash",
            "Fast Gemini model routed through the configured Gemini account.",
            1_048_576,
            true,
            true,
        ));
        models.push(codex_provider_model(
            "gem:gemini-3-pro",
            "Gemini 3 Pro",
            "Gemini model routed through the configured Gemini account.",
            1_048_576,
            true,
            true,
        ));
    }
    if state
        .grok_accounts
        .lock()
        .unwrap()
        .iter()
        .any(|account| account.enabled)
    {
        models.push(codex_provider_model(
            "grk:grok-4.3",
            "Grok 4.3",
            "Grok model routed through the configured xAI account.",
            256_000,
            true,
            true,
        ));
        models.push(codex_provider_model(
            "grk:grok-4.1",
            "Grok 4.1",
            "Grok model routed through the configured xAI account.",
            256_000,
            true,
            true,
        ));
        models.push(codex_provider_model(
            "grk:grok-3",
            "Grok 3",
            "Grok model routed through the configured xAI account.",
            131_072,
            false,
            false,
        ));
    }
    if state
        .minimax_accounts
        .lock()
        .unwrap()
        .iter()
        .any(|account| account.enabled)
    {
        models.push(codex_provider_model(
            "min:MiniMax-M3",
            "MiniMax M3",
            "MiniMax model routed through the configured MiniMax account.",
            512_000,
            true,
            true,
        ));
        models.push(codex_provider_model(
            "min:MiniMax-M2.7",
            "MiniMax M2.7",
            "MiniMax model routed through the configured MiniMax account.",
            512_000,
            false,
            false,
        ));
        models.push(codex_provider_model(
            "min:MiniMax-M2.7-highspeed",
            "MiniMax M2.7 Highspeed",
            "Fast MiniMax model routed through the configured MiniMax account.",
            512_000,
            true,
            false,
        ));
    }
    if state
        .copilot_accounts
        .lock()
        .unwrap()
        .iter()
        .any(|account| account.enabled)
    {
        models.push(codex_provider_model(
            "cop:gpt-5.1",
            "GitHub Copilot GPT-5.1",
            "GitHub Copilot model routed through the configured Copilot account.",
            200_000,
            true,
            true,
        ));
        models.push(codex_provider_model(
            "cop:gpt-5",
            "GitHub Copilot GPT-5",
            "GitHub Copilot model routed through the configured Copilot account.",
            200_000,
            true,
            true,
        ));
        models.push(codex_provider_model(
            "cop:claude-sonnet-4",
            "GitHub Copilot Claude Sonnet 4",
            "GitHub Copilot Claude model routed through the configured Copilot account.",
            200_000,
            true,
            true,
        ));
        models.push(codex_provider_model(
            "cop:claude-sonnet-4.5",
            "GitHub Copilot Claude Sonnet 4.5",
            "GitHub Copilot Claude model routed through the configured Copilot account.",
            200_000,
            true,
            true,
        ));
        models.push(codex_provider_model(
            "cop:claude-opus-4.6-1m",
            "GitHub Copilot Claude Opus 4.6 1M",
            "GitHub Copilot Claude model routed through the configured Copilot account.",
            1_000_000,
            true,
            true,
        ));
    }
    if state
        .glm_accounts
        .lock()
        .unwrap()
        .iter()
        .any(|account| account.enabled)
    {
        models.push(codex_provider_model(
            "glm:glm-5.2",
            "GLM 5.2",
            "GLM Coding Plan model routed through the configured Z.AI account.",
            256_000,
            true,
            true,
        ));
        models.push(codex_provider_model(
            "glm:glm-5.1",
            "GLM 5.1",
            "GLM Coding Plan model routed through the configured Z.AI account.",
            256_000,
            true,
            true,
        ));
        models.push(codex_provider_model(
            "glm:glm-4.6",
            "GLM 4.6",
            "GLM Coding Plan model routed through the configured Z.AI account.",
            128_000,
            true,
            true,
        ));
    }
    for model in state
        .custom_models
        .lock()
        .unwrap()
        .iter()
        .filter(|model| model.enabled)
    {
        let display_name = model
            .display_name
            .clone()
            .unwrap_or_else(|| format!("Custom {}", model.alias));
        let route_summary = custom_model_route_summary(model);
        let description = format!(
            "Custom model alias routed through {} across {} route step(s). Comma-separated targets in a step are load-balanced; later steps are fallbacks.",
            route_summary,
            custom_models::route_group_count(model)
        );
        models.push(codex_provider_model(
            &custom_models::public_model_id(&model.alias),
            &display_name,
            &description,
            128_000,
            true,
            true,
        ));
    }
    models
}

fn custom_model_route_summary(model: &custom_models::CustomModel) -> String {
    let summary = custom_models::route_summary(model);
    if summary.trim().is_empty() {
        "no enabled targets".to_string()
    } else {
        summary
    }
}

fn codex_provider_model(
    slug: &str,
    display_name: &str,
    description: &str,
    context_window: u64,
    supports_reasoning: bool,
    supports_images: bool,
) -> serde_json::Value {
    let reasoning_levels = if supports_reasoning {
        serde_json::json!([
            { "effort": "low", "description": "Fast responses with lighter reasoning" },
            { "effort": "medium", "description": "Balanced reasoning depth" },
            { "effort": "high", "description": "More reasoning depth" }
        ])
    } else {
        serde_json::json!([])
    };
    let input_modalities = if supports_images {
        serde_json::json!(["text", "image"])
    } else {
        serde_json::json!(["text"])
    };

    let mut model = serde_json::json!({
        "slug": slug,
        "priority": 1,
        "display_name": display_name,
        "description": description,
        "context_window": context_window,
        "max_context_window": context_window,
        "input_modalities": input_modalities,
        "supports_parallel_tool_calls": true,
        "supports_image_detail_original": false,
        "supports_search_tool": false,
        "support_verbosity": false,
        "default_verbosity": "low",
        "supported_in_api": true,
        "visibility": "list",
        "shell_type": "shell_command",
        "tool_mode": null,
        "apply_patch_tool_type": "freeform",
        "web_search_tool_type": "text_and_image",
        "default_reasoning_level": if supports_reasoning { "medium" } else { "none" },
        "supported_reasoning_levels": reasoning_levels,
        "default_reasoning_summary": "none",
        "reasoning_summary_format": "experimental",
        "supports_reasoning_summaries": false,
        "prefer_websockets": false,
        "use_responses_lite": false,
        "base_instructions": "You are a coding agent. Follow the user's instructions and use tools carefully."
    });

    let object = model
        .as_object_mut()
        .expect("codex provider model metadata is an object");
    object.insert(
        "auto_compact_token_limit".to_string(),
        serde_json::Value::Null,
    );
    object.insert(
        "minimal_client_version".to_string(),
        serde_json::Value::Null,
    );
    object.insert("comp_hash".to_string(), serde_json::Value::Null);
    object.insert("availability_nux".to_string(), serde_json::Value::Null);
    object.insert("upgrade".to_string(), serde_json::Value::Null);
    object.insert(
        "available_in_plans".to_string(),
        serde_json::json!(["free", "plus", "pro", "team", "enterprise"]),
    );
    object.insert("additional_speed_tiers".to_string(), serde_json::json!([]));
    object.insert("default_service_tier".to_string(), serde_json::Value::Null);
    object.insert("service_tiers".to_string(), serde_json::json!([]));
    object.insert(
        "experimental_supported_tools".to_string(),
        serde_json::json!([]),
    );
    object.insert("multi_agent_version".to_string(), serde_json::Value::Null);
    object.insert(
        "truncation_policy".to_string(),
        serde_json::json!({
            "mode": "tokens",
            "limit": context_window
        }),
    );
    object.insert(
        "auto_review_model_override".to_string(),
        serde_json::Value::Null,
    );
    object.insert(
        "model_messages".to_string(),
        serde_json::json!({
            "instructions_template": "You are a coding agent. Follow the user's instructions and use tools carefully.\n\n{{ personality }}",
            "instructions_variables": {}
        }),
    );

    model
}

#[cfg(test)]
mod codex_provider_model_tests {
    use super::{append_prefixed_codex_model_aliases, codex_provider_model};

    #[test]
    fn codex_models_include_prefixed_and_unprefixed_aliases() {
        let mut models = vec![
            serde_json::json!({
                "id": "gpt-5.4",
                "slug": "gpt-5.4",
                "display_name": "GPT-5.4"
            }),
            serde_json::json!({
                "slug": "cod:gpt-5.3",
                "display_name": "GPT-5.3"
            }),
        ];

        append_prefixed_codex_model_aliases(&mut models);

        let slugs = models
            .iter()
            .filter_map(|model| model.get("slug").and_then(|value| value.as_str()))
            .collect::<Vec<_>>();
        assert_eq!(slugs, vec!["gpt-5.4", "cod:gpt-5.3", "cod:gpt-5.4"]);

        let alias = models
            .iter()
            .find(|model| model["slug"] == "cod:gpt-5.4")
            .unwrap();
        assert_eq!(alias["id"], "cod:gpt-5.4");
        assert_eq!(alias["display_name"], "GPT-5.4");
    }

    #[test]
    fn provider_model_advertises_image_input_when_supported() {
        let model = codex_provider_model(
            "grok-4.3",
            "Grok 4.3",
            "Grok model routed through the configured xAI account.",
            256_000,
            true,
            true,
        );

        assert_eq!(
            model["input_modalities"],
            serde_json::json!(["text", "image"])
        );
    }

    #[test]
    fn provider_model_keeps_text_only_when_images_are_not_supported() {
        let model = codex_provider_model(
            "MiniMax-M2.7",
            "MiniMax M2.7",
            "MiniMax model routed through the configured MiniMax account.",
            512_000,
            false,
            false,
        );

        assert_eq!(model["input_modalities"], serde_json::json!(["text"]));
    }
}

#[cfg(test)]
mod account_selection_tests {
    use super::{
        account_runtime_status, api_key_account_prompt_limit_allows,
        api_key_account_prompt_limit_blocked, api_key_prompt_limit_violation,
        api_key_rule_allows_account, apply_router_failure, codex_error_affects_account_health,
        codex_error_status_from_message, codex_token_matches_account, parse_retry_after_seconds,
        select_ordered_account_indices_with_priority, AccountRuntimeState, AccountRuntimeStatus,
        AccountSelectionScore,
    };
    use crate::api_keys::{ApiKeyAccess, ApiKeyAccountLimit, ApiKeyProviderAccess};
    use crate::target::codex::tokens::UpstreamToken;
    use axum::http::StatusCode;
    use std::collections::VecDeque;
    use std::time::{Duration, Instant};

    #[test]
    fn quota_headroom_wins_before_request_history() {
        let scores = [
            AccountSelectionScore {
                quota_pressure: Some(81.0),
                active_requests: 1,
                recent_requests: 1,
            },
            AccountSelectionScore {
                quota_pressure: Some(22.0),
                active_requests: 10_000,
                recent_requests: 100,
            },
        ];

        let picked = select_ordered_account_indices_with_priority(
            2,
            0,
            |_| true,
            |_| false,
            |idx| scores[idx],
        )
        .into_iter()
        .next()
        .expect("picked account");
        assert_eq!(picked, 1);
    }

    #[test]
    fn exhausted_quota_accounts_are_ordered_after_available_accounts() {
        let scores = [
            AccountSelectionScore {
                quota_pressure: Some(f64::INFINITY),
                active_requests: 1,
                recent_requests: 1,
            },
            AccountSelectionScore {
                quota_pressure: Some(90.0),
                active_requests: 10_000,
                recent_requests: 100,
            },
            AccountSelectionScore {
                quota_pressure: Some(f64::INFINITY),
                active_requests: 2,
                recent_requests: 2,
            },
        ];

        let order = select_ordered_account_indices_with_priority(
            3,
            0,
            |_| true,
            |_| false,
            |idx| scores[idx],
        );
        assert_eq!(order, vec![1, 0, 2]);
    }

    #[test]
    fn equal_scores_keep_round_robin_tie_breaker() {
        let scores = [
            AccountSelectionScore {
                quota_pressure: Some(50.0),
                active_requests: 10,
                recent_requests: 1,
            },
            AccountSelectionScore {
                quota_pressure: Some(50.0),
                active_requests: 10,
                recent_requests: 1,
            },
        ];

        let picked = select_ordered_account_indices_with_priority(
            2,
            1,
            |_| true,
            |_| false,
            |idx| scores[idx],
        )
        .into_iter()
        .next()
        .expect("picked account");
        assert_eq!(picked, 1);
    }

    #[test]
    fn disabled_accounts_are_skipped_before_scoring() {
        let scores = [
            AccountSelectionScore {
                quota_pressure: Some(1.0),
                active_requests: 0,
                recent_requests: 0,
            },
            AccountSelectionScore {
                quota_pressure: Some(90.0),
                active_requests: 0,
                recent_requests: 0,
            },
        ];

        let picked = select_ordered_account_indices_with_priority(
            2,
            0,
            |idx| idx == 1,
            |_| false,
            |idx| scores[idx],
        )
        .into_iter()
        .next()
        .expect("picked account");
        assert_eq!(picked, 1);
    }

    #[test]
    fn active_requests_win_after_equal_quota_headroom() {
        let scores = [
            AccountSelectionScore {
                quota_pressure: Some(40.0),
                active_requests: 3,
                recent_requests: 0,
            },
            AccountSelectionScore {
                quota_pressure: Some(40.0),
                active_requests: 0,
                recent_requests: 9,
            },
        ];

        let picked = select_ordered_account_indices_with_priority(
            2,
            0,
            |_| true,
            |_| false,
            |idx| scores[idx],
        )
        .into_iter()
        .next()
        .expect("picked account");
        assert_eq!(picked, 1);
    }

    #[test]
    fn priority_accounts_are_selected_before_normal_accounts() {
        let scores = [
            AccountSelectionScore {
                quota_pressure: Some(90.0),
                active_requests: 10,
                recent_requests: 10,
            },
            AccountSelectionScore {
                quota_pressure: Some(10.0),
                active_requests: 0,
                recent_requests: 0,
            },
        ];

        let picked = select_ordered_account_indices_with_priority(
            2,
            0,
            |_| true,
            |idx| idx == 0,
            |idx| scores[idx],
        )
        .into_iter()
        .next()
        .expect("picked account");
        assert_eq!(picked, 0);
    }

    #[test]
    fn exhausted_priority_accounts_fall_back_to_normal_accounts() {
        let scores = [
            AccountSelectionScore {
                quota_pressure: Some(f64::INFINITY),
                active_requests: 0,
                recent_requests: 0,
            },
            AccountSelectionScore {
                quota_pressure: Some(10.0),
                active_requests: 0,
                recent_requests: 0,
            },
        ];

        let picked = select_ordered_account_indices_with_priority(
            2,
            0,
            |_| true,
            |idx| idx == 0,
            |idx| scores[idx],
        )
        .into_iter()
        .next()
        .expect("picked account");
        assert_eq!(picked, 1);
    }

    #[test]
    fn api_key_account_rule_matches_any_selected_account_alias() {
        let access = ApiKeyAccess {
            all: false,
            prompt_token_limit: None,
            providers: vec![ApiKeyProviderAccess {
                provider: "codex".to_string(),
                accounts: vec!["first.json".to_string(), "second.json".to_string()],
                prompt_token_limit: None,
                account_limits: Vec::new(),
            }],
        };

        let selected = UpstreamToken {
            token: "token-b".to_string(),
            account_id: Some("account-b".to_string()),
            label: "Second".to_string(),
            file_name: Some("second.json".to_string()),
            enabled: true,
            expired_at: None,
        };
        let excluded = UpstreamToken {
            token: "token-c".to_string(),
            account_id: Some("account-c".to_string()),
            label: "Third".to_string(),
            file_name: Some("third.json".to_string()),
            enabled: true,
            expired_at: None,
        };

        assert!(api_key_rule_allows_account(&access, "codex", |allowed| {
            codex_token_matches_account(&selected, allowed)
        }));
        assert!(!api_key_rule_allows_account(&access, "codex", |allowed| {
            codex_token_matches_account(&excluded, allowed)
        }));
        assert!(!api_key_rule_allows_account(&access, "gemini", |_| true));
    }

    #[test]
    fn api_key_prompt_limits_use_strictest_matching_scope() {
        let access = ApiKeyAccess {
            all: false,
            prompt_token_limit: Some(1000),
            providers: vec![ApiKeyProviderAccess {
                provider: "codex".to_string(),
                accounts: vec!["first.json".to_string(), "second.json".to_string()],
                prompt_token_limit: Some(800),
                account_limits: vec![
                    ApiKeyAccountLimit {
                        account: "first.json".to_string(),
                        prompt_token_limit: Some(300),
                    },
                    ApiKeyAccountLimit {
                        account: "second.json".to_string(),
                        prompt_token_limit: Some(900),
                    },
                ],
            }],
        }
        .normalized()
        .unwrap();

        assert!(api_key_prompt_limit_violation(&access, None, None, 1001).is_some());
        assert!(api_key_prompt_limit_violation(&access, Some("codex"), None, 801).is_some());
        assert!(api_key_account_prompt_limit_allows(
            &access,
            "codex",
            |account| account == "first.json",
            Some(300)
        ));
        assert!(!api_key_account_prompt_limit_allows(
            &access,
            "codex",
            |account| account == "first.json",
            Some(301)
        ));
        assert!(api_key_account_prompt_limit_allows(
            &access,
            "codex",
            |account| account == "second.json",
            Some(800)
        ));
        assert!(!api_key_account_prompt_limit_allows(
            &access,
            "codex",
            |account| account == "second.json",
            Some(801)
        ));
    }

    #[test]
    fn api_key_account_prompt_limit_blocked_requires_all_allowed_accounts_over_limit() {
        let access = ApiKeyAccess {
            all: false,
            prompt_token_limit: None,
            providers: vec![ApiKeyProviderAccess {
                provider: "codex".to_string(),
                accounts: Vec::new(),
                prompt_token_limit: None,
                account_limits: vec![
                    ApiKeyAccountLimit {
                        account: "first.json".to_string(),
                        prompt_token_limit: Some(300),
                    },
                    ApiKeyAccountLimit {
                        account: "second.json".to_string(),
                        prompt_token_limit: Some(250),
                    },
                ],
            }],
        }
        .normalized()
        .unwrap();
        let accounts = vec![
            UpstreamToken {
                token: "token-a".to_string(),
                account_id: Some("account-a".to_string()),
                label: "First".to_string(),
                file_name: Some("first.json".to_string()),
                enabled: true,
                expired_at: None,
            },
            UpstreamToken {
                token: "token-b".to_string(),
                account_id: Some("account-b".to_string()),
                label: "Second".to_string(),
                file_name: Some("second.json".to_string()),
                enabled: true,
                expired_at: None,
            },
        ];

        assert_eq!(
            api_key_account_prompt_limit_blocked(
                &accounts,
                &access,
                "codex",
                301,
                codex_token_matches_account,
            ),
            Some(250)
        );
        assert_eq!(
            api_key_account_prompt_limit_blocked(
                &accounts,
                &access,
                "codex",
                275,
                codex_token_matches_account,
            ),
            None
        );
    }

    #[test]
    fn model_cache_ttl_is_ten_minutes() {
        let now = Instant::now();
        assert!(super::model_cache_is_fresh(
            now - Duration::from_secs(10 * 60 - 1)
        ));
        assert!(!super::model_cache_is_fresh(
            now - Duration::from_secs(10 * 60 + 1)
        ));
    }

    #[test]
    fn cooldown_state_blocks_until_expiry_then_allows_one_probe() {
        let now = Instant::now();
        let runtime = AccountRuntimeState {
            cooldown_until: Some(now + Duration::from_secs(10)),
            ..Default::default()
        };
        assert_eq!(
            account_runtime_status(true, &runtime, now),
            AccountRuntimeStatus::Cooldown
        );

        let expired = AccountRuntimeState {
            cooldown_until: Some(now - Duration::from_secs(1)),
            ..Default::default()
        };
        assert_eq!(
            account_runtime_status(true, &expired, now),
            AccountRuntimeStatus::Healthy
        );

        let probing = AccountRuntimeState {
            cooldown_until: Some(now - Duration::from_secs(1)),
            probing: true,
            ..Default::default()
        };
        assert_eq!(
            account_runtime_status(true, &probing, now),
            AccountRuntimeStatus::Probing
        );
    }

    #[test]
    fn deterministic_invalid_history_does_not_create_cooldown() {
        let mut runtime = AccountRuntimeState {
            consecutive_failures: 4,
            cooldown_until: Some(Instant::now() + Duration::from_secs(60)),
            probing: true,
            recent_requests: VecDeque::new(),
            ..Default::default()
        };
        apply_router_failure(
            &mut runtime,
            "Invalid request history: function_call_output has no matching function_call",
            Instant::now(),
        );
        assert_eq!(runtime.consecutive_failures, 0);
        assert!(runtime.cooldown_until.is_none());
        assert!(!runtime.probing);
    }

    #[test]
    fn generic_codex_sse_error_does_not_affect_account_health() {
        assert!(!codex_error_affects_account_health(
            "upstream returned an SSE error"
        ));
        assert!(!codex_error_affects_account_health(
            "Invalid request history: function_call_output has no matching function_call"
        ));
        assert!(!codex_error_affects_account_health(
            "upstream status 400: unsupported parameter"
        ));
    }

    #[test]
    fn codex_auth_quota_and_upstream_errors_affect_account_health() {
        assert!(codex_error_affects_account_health("upstream status 401"));
        assert!(codex_error_affects_account_health(
            "upstream status 429 retry-after=30"
        ));
        assert!(codex_error_affects_account_health(
            "upstream stream read failed: timeout"
        ));
        assert!(codex_error_affects_account_health(
            "upstream status 503 temporarily unavailable"
        ));
    }

    #[test]
    fn codex_error_status_preserves_actionable_statuses() {
        assert_eq!(
            codex_error_status_from_message(
                "upstream status 429 retry-after=30: quota exceeded",
                StatusCode::BAD_GATEWAY
            ),
            StatusCode::TOO_MANY_REQUESTS
        );
        assert_eq!(
            codex_error_status_from_message(
                "Invalid request history: function_call_output has no matching function_call",
                StatusCode::BAD_GATEWAY
            ),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            codex_error_status_from_message(
                "upstream first-event timeout after 15 seconds",
                StatusCode::BAD_GATEWAY
            ),
            StatusCode::GATEWAY_TIMEOUT
        );
        assert_eq!(
            codex_error_status_from_message(
                "unsupported parameter: max_output_tokens",
                StatusCode::BAD_GATEWAY
            ),
            StatusCode::BAD_REQUEST
        );
    }

    #[test]
    fn quota_retry_after_is_bounded_and_parsed_from_common_formats() {
        assert_eq!(parse_retry_after_seconds("429 retry-after=45"), Some(45));
        assert_eq!(
            parse_retry_after_seconds("quota error {\"retry-after\": 90}"),
            Some(90)
        );

        let now = Instant::now();
        let mut runtime = AccountRuntimeState::default();
        apply_router_failure(&mut runtime, "status 429 retry-after=999999", now);
        let cooldown = runtime.cooldown_until.expect("quota cooldown");
        assert!(cooldown <= now + Duration::from_secs(30 * 60));
        assert!(cooldown > now);
    }

    #[test]
    fn claude_fable_credits_required_error_does_not_poison_router() {
        let now = Instant::now();
        let mut runtime = AccountRuntimeState::default();
        // The exact envelope Anthropic returned for the Fable 5 access denial:
        // HTTP 429 + rate_limit_error + error_code credits_required + a per-model
        // disabled_reason. Without the carve-out, this hits the 30-minute quota
        // cooldown and blacklists every other Claude model on the same account.
        let body = r#"Claude returned 429 Too Many Requests: {"type":"error","error":{"type":"rate_limit_error","message":"Usage credits are required for this model.","details":{"error_code":"credits_required","model_display_name":"Fable 5","disabled_reason":"org_level_disabled","model":"claude-fable-5"}},"request_id":"req_011CdEZoCbk2HhjhmoyPFvAN"}"#;
        apply_router_failure(&mut runtime, body, now);
        assert_eq!(runtime.consecutive_failures, 0);
        assert!(runtime.cooldown_until.is_none());
        assert!(!runtime.probing);
    }

    #[test]
    fn genuine_429_quota_still_triggers_provider_cooldown() {
        let now = Instant::now();
        let mut runtime = AccountRuntimeState::default();
        let body = r#"Claude returned 429: {"type":"error","error":{"type":"rate_limit_error","message":"You have exceeded the usage limit."}}"#;
        apply_router_failure(&mut runtime, body, now);
        let cooldown = runtime.cooldown_until.expect("quota cooldown must be set");
        assert!(cooldown > now + Duration::from_secs(60));
    }

    #[test]
    fn plan_doesnt_include_model_does_not_poison_router() {
        let now = Instant::now();
        let mut runtime = AccountRuntimeState::default();
        let body = r#"Claude returned 402 Payment Required: {"type":"error","error":{"type":"billing_error","details":{"error_code":"plan_doesnt_include_model","model":"claude-opus-4-9"}}}"#;
        apply_router_failure(&mut runtime, body, now);
        assert_eq!(runtime.consecutive_failures, 0);
        assert!(runtime.cooldown_until.is_none());
    }
}

pub(crate) fn should_drop_incoming_header(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    if is_hop_header(&lower)
        || lower == "authorization"
        || lower == "x-api-key"
        || lower == "x-internal-proxy-key"
        || lower == "accept-encoding"
        || lower == "host"
        || lower == "content-length"
        || lower == "version"
        || lower == "x-forwarded-for"
        || lower == "x-forwarded-host"
        || lower == "x-forwarded-proto"
        || lower == "x-real-ip"
        || lower == "true-client-ip"
    {
        return true;
    }

    // Never leak edge-provider headers upstream (for example Cloudflare).
    lower.starts_with("cf-")
}

fn select_config_path(cli_path: Option<PathBuf>, env_path: Option<PathBuf>) -> PathBuf {
    cli_path
        .or(env_path)
        .unwrap_or_else(|| PathBuf::from("config.json"))
}

fn absolute_path(path: &FsPath) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .expect("failed to determine current directory")
            .join(path)
    }
}

fn resolve_config_relative_paths(cfg: &mut Config, config_path: &FsPath) {
    let Some(auth_dir) = cfg.auth_dir.as_mut() else {
        return;
    };
    let configured_auth_dir = FsPath::new(auth_dir);
    if configured_auth_dir.is_absolute() {
        return;
    }

    let config_dir = config_path.parent().unwrap_or_else(|| FsPath::new("."));
    *auth_dir = config_dir
        .join(configured_auth_dir)
        .to_string_lossy()
        .into_owned();
}

fn load_config(config_path: &FsPath) -> (Config, PathBuf) {
    let config_path = absolute_path(config_path);
    let data = std::fs::read_to_string(&config_path)
        .unwrap_or_else(|err| panic!("failed to read config {}: {err}", config_path.display()));
    let mut cfg: Config = serde_json::from_str(&data)
        .unwrap_or_else(|err| panic!("invalid config {}: {err}", config_path.display()));
    resolve_config_relative_paths(&mut cfg, &config_path);
    admin_auth::apply_env_overrides(&mut cfg.admin_auth);
    (cfg, config_path)
}

pub(crate) fn generated_temp_download_dir() -> PathBuf {
    std::env::temp_dir().join("io-gateway-downloads")
}

#[cfg(test)]
mod runtime_path_tests {
    use super::{
        generated_temp_download_dir, resolve_config_relative_paths, select_config_path, Config,
        GatewayArgs,
    };
    use clap::Parser;
    use std::path::PathBuf;

    #[test]
    fn config_flag_is_parsed_and_has_priority_over_environment_path() {
        let args = GatewayArgs::try_parse_from(["io-gateway", "--config", "cli/config.json"])
            .expect("parse --config");
        assert_eq!(
            select_config_path(args.config, Some(PathBuf::from("env/config.json"))),
            PathBuf::from("cli/config.json")
        );
    }

    #[test]
    fn environment_config_path_is_used_before_legacy_default() {
        assert_eq!(
            select_config_path(None, Some(PathBuf::from("env/config.json"))),
            PathBuf::from("env/config.json")
        );
        assert_eq!(select_config_path(None, None), PathBuf::from("config.json"));
    }

    #[test]
    fn relative_auth_dir_is_resolved_from_config_parent() {
        let mut cfg: Config = serde_json::from_value(serde_json::json!({
            "listen": "127.0.0.1:8319",
            "upstream_base": "https://example.test",
            "proxy_api_key": "test-key",
            "tokens": [],
            "auth_dir": "auths"
        }))
        .expect("valid test config");
        let config_path = std::env::temp_dir()
            .join("io-gateway-runtime-path-tests")
            .join("config.json");

        resolve_config_relative_paths(&mut cfg, &config_path);

        assert_eq!(
            PathBuf::from(cfg.auth_dir.expect("auth dir")),
            config_path.parent().expect("config parent").join("auths")
        );
    }

    #[test]
    fn generated_download_dir_uses_the_system_temp_directory() {
        assert_eq!(
            generated_temp_download_dir(),
            std::env::temp_dir().join("io-gateway-downloads")
        );
    }
}

fn admin_session_path(cfg: &Config) -> PathBuf {
    cfg.auth_dir
        .as_deref()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("admin-sessions.json")
}

fn detect_source_api(raw_path: &str) -> SourceApi {
    let trimmed = raw_path.trim_start_matches('/');
    if trimmed == "codex" || trimmed.starts_with("codex/") {
        SourceApi::Codex
    } else if trimmed == "claude" || trimmed.starts_with("claude/") {
        SourceApi::Claude
    } else {
        SourceApi::V1
    }
}

fn is_v1_models_list_path(raw_path: &str) -> bool {
    normalize_v1_path(raw_path) == "models"
}

fn is_codex_models_list_path(raw_path: &str) -> bool {
    raw_path
        .trim_start_matches('/')
        .trim_end_matches('/')
        .eq("codex/models")
}

fn v1_model_retrieve_id(raw_path: &str) -> Option<String> {
    let norm = normalize_v1_path(raw_path);
    let id = norm.strip_prefix("models/")?;
    if id.is_empty() || id.contains('/') {
        return None;
    }
    Some(id.to_string())
}

fn normalize_v1_path(raw_path: &str) -> String {
    let trimmed = raw_path.trim_start_matches('/');
    if trimmed == "v1" {
        String::new()
    } else if let Some(rest) = trimmed.strip_prefix("v1/") {
        rest.to_string()
    } else {
        trimmed.to_string()
    }
}
