use chrono::{DateTime, Utc};
use clap::{Args, Parser, Subcommand};
use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Clear, Paragraph, Row, Sparkline, Table, Tabs, Wrap},
    Frame, Terminal,
};
use reqwest::{
    header::{COOKIE, SET_COOKIE, USER_AGENT},
    Client, Method, StatusCode,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    fs,
    io::{self, Stdout, Write},
    net::SocketAddr,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

#[path = "iogw/animated.rs"]
mod animated;
#[path = "../provider_quota_summary.rs"]
mod provider_quota_summary;
#[path = "iogw/system_metrics.rs"]
mod system_metrics;
#[path = "iogw/tui_network.rs"]
mod tui_network;
use tui_network::{AccountLoginPrompt, NetworkEvent, Section, TuiCommand, TuiNetwork};

const DEFAULT_BASE_URL: &str = "http://127.0.0.1:8319";
const SESSION_COOKIE_NAME: &str = "io_gateway_admin_session";
const USER_AGENT_VALUE: &str = "iogw/0.1";

const BTOP_MAIN_BG: Color = Color::Rgb(0x00, 0x00, 0x00);
const BTOP_MAIN_FG: Color = Color::Rgb(0xcc, 0xcc, 0xcc);
const BTOP_TITLE: Color = Color::Rgb(0xee, 0xee, 0xee);
const BTOP_HI_FG: Color = Color::Rgb(0xb5, 0x40, 0x40);
const BTOP_SELECTED_BG: Color = Color::Rgb(0x6a, 0x2f, 0x2f);
const BTOP_SELECTED_FG: Color = Color::Rgb(0xee, 0xee, 0xee);
const BTOP_INACTIVE_FG: Color = Color::Rgb(0x40, 0x40, 0x40);
const BTOP_GRAPH_TEXT: Color = Color::Rgb(0x60, 0x60, 0x60);
const BTOP_CPU_BOX: Color = Color::Rgb(0x55, 0x6d, 0x59);
const BTOP_MEM_BOX: Color = Color::Rgb(0x6c, 0x6c, 0x4b);
const BTOP_NET_BOX: Color = Color::Rgb(0x5c, 0x58, 0x8d);
const BTOP_PROC_BOX: Color = Color::Rgb(0x80, 0x52, 0x52);
const BTOP_DIV_LINE: Color = Color::Rgb(0x30, 0x30, 0x30);
const BTOP_TEMP_START: Color = Color::Rgb(0x48, 0x97, 0xd4);
const BTOP_CPU_START: Color = Color::Rgb(0x77, 0xca, 0x9b);
const BTOP_STATUS_DISABLED: Color = Color::Rgb(0xff, 0x47, 0x69);
const BTOP_STATUS_COOLDOWN: Color = Color::Rgb(0x48, 0x97, 0xd4);
const BTOP_ATTENTION: Color = Color::Rgb(0xff, 0xb8, 0x14);
const BTOP_CPU_MID: Color = Color::Rgb(0xcb, 0xc0, 0x6c);
const BTOP_CACHED_MID: Color = Color::Rgb(0x74, 0xe6, 0xfc);
const BTOP_AVAILABLE_END: Color = Color::Rgb(0xff, 0xb8, 0x14);
const BTOP_USED_END: Color = Color::Rgb(0xff, 0x47, 0x69);

const PROVIDERS: &[ProviderSpec] = &[
    ProviderSpec {
        key: "compatible",
        label: "Compatible API",
        quota_path: "/compatible/quota.json",
    },
    ProviderSpec {
        key: "codex",
        label: "Codex",
        quota_path: "/quota.json",
    },
    ProviderSpec {
        key: "antigravity",
        label: "Antigravity",
        quota_path: "/agw/quota.json",
    },
    ProviderSpec {
        key: "gemini",
        label: "Gemini",
        quota_path: "/gemini/quota.json",
    },
    ProviderSpec {
        key: "qwen",
        label: "Qwen",
        quota_path: "/qwen/quota.json",
    },
    ProviderSpec {
        key: "deepseek",
        label: "DeepSeek",
        quota_path: "/deepseek/quota.json",
    },
    ProviderSpec {
        key: "minimax",
        label: "MiniMax",
        quota_path: "/minimax/quota.json",
    },
    ProviderSpec {
        key: "grok",
        label: "Grok",
        quota_path: "/grok/quota.json",
    },
    ProviderSpec {
        key: "copilot",
        label: "Copilot",
        quota_path: "/copilot/quota.json",
    },
    ProviderSpec {
        key: "claude",
        label: "Claude",
        quota_path: "/claude/quota.json",
    },
    ProviderSpec {
        key: "glm",
        label: "GLM",
        quota_path: "/glm/quota.json",
    },
];

#[derive(Parser, Debug)]
#[command(name = "iogw")]
#[command(about = "Terminal management client for IO Gateway")]
struct Cli {
    /// Gateway URL. Defaults to the port in the locally installed gateway config.
    #[arg(short = 'u', long, env = "IOGW_BASE_URL", global = true)]
    base_url: Option<String>,

    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Open the interactive terminal UI.
    Tui,
    /// Install, inspect or remove optional animated UI scene packs.
    Plugins(animated::PluginsCommand),
    /// Create and store an admin session cookie.
    Login(LoginArgs),
    /// Log out and remove the stored session cookie.
    Logout,
    /// Print gateway health, readiness, and admin session status.
    Status,
    /// Manage provider accounts and priority routing.
    Accounts(AccountsCommand),
    /// View or refresh quota snapshots.
    Quota(QuotaCommand),
    /// Manage gateway API keys.
    Keys(KeysCommand),
    /// Manage custom models.
    Models(ModelsCommand),
    /// View usage totals and history.
    Usage(UsageCommand),
    /// View or test notification settings.
    Notifications(NotificationsCommand),
    /// Call a raw gateway path.
    Raw(RawArgs),
}

#[derive(Args, Debug)]
struct LoginArgs {
    /// One-time password from the configured TOTP authenticator.
    otp: Option<String>,
    /// Administrator API key, when the gateway requires it in addition to TOTP.
    /// Prefer IOGW_ADMIN_API_KEY in scripts so the value is not placed in shell history.
    #[arg(long, env = "IOGW_ADMIN_API_KEY", hide_env_values = true)]
    api_key: Option<String>,
}

#[derive(Args, Debug)]
struct AccountsCommand {
    #[command(subcommand)]
    action: Option<AccountsAction>,
}

#[derive(Subcommand, Debug)]
enum AccountsAction {
    /// List accounts across all providers.
    List,
    /// Enable an account by file name, key, label, or account id.
    Enable(AccountTargetArgs),
    /// Disable an account by file name, key, label, or account id.
    Disable(AccountTargetArgs),
    /// Delete an account credential file.
    Delete(AccountTargetArgs),
    /// Add or remove "use first" priority routing for an account.
    Priority(PriorityArgs),
}

#[derive(Args, Debug)]
struct AccountTargetArgs {
    target: String,
    #[arg(short, long)]
    provider: Option<String>,
}

#[derive(Args, Debug)]
struct PriorityArgs {
    target: String,
    #[arg(short, long)]
    provider: Option<String>,
    #[arg(long)]
    off: bool,
}

#[derive(Args, Debug)]
struct QuotaCommand {
    #[arg(short, long)]
    provider: Option<String>,
    #[arg(long)]
    refresh: bool,
    /// Show only the most constrained limit for each account.
    #[arg(long)]
    one_limit: bool,
}

#[derive(Args, Debug)]
struct KeysCommand {
    #[command(subcommand)]
    action: Option<KeysAction>,
}

#[derive(Subcommand, Debug)]
enum KeysAction {
    /// List API keys.
    List,
    /// Create an API key. Prints the plain-text key once.
    Create(KeyCreateArgs),
    /// Replace a key's access/quota policy without resetting its usage.
    Update(KeyUpdateArgs),
    /// Show durable renewable quota balances and renewal times.
    Quotas {
        #[arg(long)]
        id: Option<String>,
    },
    /// Revoke an API key by id.
    Revoke { id: String },
}

#[derive(Args, Debug)]
struct KeyCreateArgs {
    #[arg(short, long, default_value = "API key")]
    label: String,
    /// Optional JSON access object, or @path to a JSON file. Provider rules
    /// must use account_scope="all" or account_scope="selected" explicitly.
    #[arg(long)]
    access_json: Option<String>,
    /// Optional renewable quota policy JSON, or @path. Includes timezone and rules.
    #[arg(long)]
    quota_json: Option<String>,
    /// Optional whole-key maximum estimated input tokens per request.
    /// `--prompt-token-limit` remains an alias for compatibility.
    #[arg(long, visible_alias = "prompt-token-limit")]
    max_estimated_input_tokens_per_request: Option<u64>,
}

#[derive(Args, Debug)]
struct KeyUpdateArgs {
    id: String,
    /// Complete access object (including existing scopes/caps), or @path.
    #[arg(long)]
    access_json: String,
}

#[derive(Args, Debug)]
struct ModelsCommand {
    #[command(subcommand)]
    action: Option<ModelsAction>,
}

#[derive(Subcommand, Debug)]
enum ModelsAction {
    /// List custom models.
    List,
    /// Save a custom model from a JSON object or @path.
    Save { json: String },
    /// Delete a custom model alias.
    Delete { alias: String },
    /// Refresh model and quota caches.
    Refresh {
        #[arg(short, long)]
        provider: Option<String>,
    },
}

#[derive(Args, Debug)]
struct UsageCommand {
    #[command(subcommand)]
    action: Option<UsageAction>,
}

#[derive(Subcommand, Debug)]
enum UsageAction {
    /// Print aggregate usage totals.
    Summary,
    /// Print recent usage events.
    History(UsageHistoryArgs),
}

#[derive(Args, Debug)]
struct UsageHistoryArgs {
    #[arg(short, long, default_value_t = 20)]
    limit: usize,
    #[arg(short, long)]
    provider: Option<String>,
    #[arg(long)]
    account_key: Option<String>,
    #[arg(short, long)]
    model: Option<String>,
}

#[derive(Args, Debug)]
struct NotificationsCommand {
    #[command(subcommand)]
    action: Option<NotificationsAction>,
}

#[derive(Subcommand, Debug)]
enum NotificationsAction {
    /// Show public notification settings.
    Settings,
    /// Send a test notification.
    Test,
}

#[derive(Args, Debug)]
struct RawArgs {
    path: String,
    #[arg(short, long, default_value = "GET")]
    method: String,
    /// JSON body, or @path to a JSON file.
    #[arg(long)]
    body: Option<String>,
}

#[derive(Clone, Copy)]
struct ProviderSpec {
    key: &'static str,
    label: &'static str,
    quota_path: &'static str,
}

#[derive(Clone, Default)]
struct AccountRow {
    provider: String,
    provider_label: String,
    key: String,
    label: String,
    account_id: String,
    file_name: String,
    enabled: bool,
    cooldown: bool,
    cooldown_remaining_seconds: Option<u64>,
    attention_reasons: Vec<String>,
    priority: bool,
    requests: u64,
    errors: u64,
    total_tokens: u64,
    last_success_at: String,
    last_error_at: String,
    last_error_message: String,
}

#[derive(Clone)]
struct QuotaRow {
    provider: String,
    account_key: String,
    account_label: String,
    limit_label: String,
    label: String,
    detail_text: String,
    overview: bool,
    used_percent_value: Option<f64>,
    used_percent: String,
    remaining_percent: String,
    reset: String,
}

impl Default for QuotaRow {
    fn default() -> Self {
        Self {
            provider: String::new(),
            account_key: String::new(),
            account_label: String::new(),
            limit_label: String::new(),
            label: String::new(),
            detail_text: String::new(),
            overview: true,
            used_percent_value: None,
            used_percent: String::new(),
            remaining_percent: String::new(),
            reset: String::new(),
        }
    }
}

#[derive(Clone, Default)]
struct KeyRow {
    id: String,
    label: String,
    prefix: String,
    source: String,
    revoked: bool,
    revoked_at: String,
    created_at: String,
    last_used_at: String,
    access: String,
    access_json: Value,
}

#[derive(Clone, Default)]
struct NotificationAccountRow {
    provider: String,
    provider_label: String,
    key: String,
    label: String,
    account_id: String,
    credential_file: String,
    enabled: bool,
    watched: bool,
}

#[derive(Clone, Default)]
struct ModelRow {
    id: String,
    alias: String,
    display_name: String,
    enabled: bool,
    routes: u64,
    targets: u64,
    raw: Value,
}

#[derive(Clone, Default)]
struct UsageBucket {
    label: String,
    input_tokens: u64,
    output_tokens: u64,
    total_tokens: u64,
    cache_tokens: u64,
    reasoning_tokens: u64,
    request_count: u64,
}

#[derive(Default)]
struct GatewayData {
    session: Value,
    summary: Value,
    routing: Value,
    snapshot: Value,
    keys: Vec<KeyRow>,
    key_quota_summaries: Value,
    models: Vec<ModelRow>,
    accounts: Vec<AccountRow>,
    quotas: Vec<QuotaRow>,
    usage_buckets: Vec<UsageBucket>,
    notifications: Value,
}

#[derive(Clone)]
struct GatewayClient {
    base_url: String,
    http: Client,
    session_cookie: Option<String>,
    session_path: PathBuf,
}

struct ApiResponse {
    status: StatusCode,
    body: Value,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum Tab {
    Overview,
    Keys,
    Notifications,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum LimitDisplayMode {
    OnePerAccount,
    AllPerAccount,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum OverviewSection {
    Metrics,
    ContextUsage,
    CustomModels,
    AccountUsage,
}

impl OverviewSection {
    fn all() -> &'static [OverviewSection] {
        &[
            OverviewSection::Metrics,
            OverviewSection::ContextUsage,
            OverviewSection::CustomModels,
            OverviewSection::AccountUsage,
        ]
    }

    fn title(self) -> &'static str {
        match self {
            OverviewSection::Metrics => "Metrics",
            OverviewSection::ContextUsage => "Context Usage",
            OverviewSection::CustomModels => "Custom Models",
            OverviewSection::AccountUsage => "Account Usage",
        }
    }
}

enum ConfirmationAction {
    SaveClaudeResetPolicy {
        body: Value,
    },
    ConsumeReset {
        provider: String,
        file_name: String,
        credit_id: String,
        program: String,
    },
    DeleteAccount {
        file_name: String,
    },
    DeleteCustomModel {
        alias: String,
    },
    RevokeApiKey {
        id: String,
    },
}

struct ConfirmationModal {
    title: String,
    message: String,
    action: ConfirmationAction,
}

impl LimitDisplayMode {
    fn toggle(&mut self) {
        *self = match self {
            LimitDisplayMode::OnePerAccount => LimitDisplayMode::AllPerAccount,
            LimitDisplayMode::AllPerAccount => LimitDisplayMode::OnePerAccount,
        };
    }

    fn label(self) -> &'static str {
        match self {
            LimitDisplayMode::OnePerAccount => "one limit/account",
            LimitDisplayMode::AllPerAccount => "all limits",
        }
    }

    fn preference_value(self) -> &'static str {
        match self {
            LimitDisplayMode::OnePerAccount => "one_per_account",
            LimitDisplayMode::AllPerAccount => "all_per_account",
        }
    }

    fn from_preference(value: &str) -> Self {
        match value {
            "all_per_account" => LimitDisplayMode::AllPerAccount,
            _ => LimitDisplayMode::OnePerAccount,
        }
    }
}

#[derive(Default, Deserialize, Serialize)]
struct TuiPreferences {
    limit_display_mode: Option<String>,
    hidden_usage_providers: Vec<String>,
    hidden_usage_accounts: Vec<HiddenUsageAccountPreference>,
    #[serde(default)]
    account_orders: Vec<AccountOrderPreference>,
    #[serde(default)]
    provider_order: Vec<String>,
}

#[derive(Deserialize, Serialize)]
struct HiddenUsageAccountPreference {
    provider: String,
    account_key: String,
}

#[derive(Deserialize, Serialize)]
struct AccountOrderPreference {
    provider: String,
    account_keys: Vec<String>,
}

struct TuiPreferenceState {
    limit_display_mode: LimitDisplayMode,
    hidden_usage_providers: HashSet<String>,
    hidden_usage_accounts: HashSet<(String, String)>,
    account_orders: HashMap<String, Vec<String>>,
    provider_order: Vec<String>,
}

impl Default for TuiPreferenceState {
    fn default() -> Self {
        Self {
            limit_display_mode: LimitDisplayMode::OnePerAccount,
            hidden_usage_providers: HashSet::new(),
            hidden_usage_accounts: HashSet::new(),
            account_orders: HashMap::new(),
            provider_order: Vec::new(),
        }
    }
}

impl Tab {
    fn all() -> &'static [Tab] {
        &[Tab::Overview, Tab::Keys, Tab::Notifications]
    }

    fn title(self) -> &'static str {
        match self {
            Tab::Overview => "Overview",
            Tab::Keys => "API Keys",
            Tab::Notifications => "Notifications",
        }
    }
}

struct TuiApp {
    client: GatewayClient,
    preferences_path: PathBuf,
    data: GatewayData,
    tab: Tab,
    selected: usize,
    overview_section: OverviewSection,
    overview_account_selected: usize,
    overview_model_selected: usize,
    limit_display_mode: LimitDisplayMode,
    hidden_usage_providers: HashSet<String>,
    hidden_usage_accounts: HashSet<(String, String)>,
    account_orders: HashMap<String, Vec<String>>,
    provider_order: Vec<String>,
    show_command_modal: bool,
    command_modal_scroll: usize,
    show_account_details: bool,
    account_details_scroll: usize,
    show_api_key_details: bool,
    api_key_details_scroll: usize,
    show_api_key_reveal: bool,
    api_key_reveal: String,
    pending_account_login: Option<AccountLoginPrompt>,
    confirmation_modal: Option<ConfirmationModal>,
    message: String,
    network: TuiNetwork,
    animation: animated::Animation,
}

#[tokio::main]
async fn main() {
    if let Err(err) = run().await {
        eprintln!("error: {}", err);
        std::process::exit(1);
    }
}

async fn run() -> Result<(), String> {
    let cli = Cli::parse();
    // Plugin management is entirely local and must not require gateway access.
    if let Some(Command::Plugins(command)) = cli.command {
        return print_response(cli.json, animated::command(command)?);
    }
    let mut client = GatewayClient::new(resolve_base_url(cli.base_url.as_deref()))?;
    match cli.command.unwrap_or(Command::Tui) {
        Command::Tui => run_tui(client).await,
        Command::Plugins(_) => unreachable!("handled before gateway initialization"),
        Command::Login(args) => {
            let otp = match args.otp {
                Some(otp) => otp,
                None => prompt_line("OTP: ").map_err(|err| err.to_string())?,
            };
            let response = client.login(&otp, args.api_key.as_deref()).await?;
            ensure_success(&response)?;
            print_response(cli.json, response.body)
        }
        Command::Logout => {
            let response = client.post_json("/admin/logout", None).await?;
            client.clear_session()?;
            print_response(cli.json, response.body)
        }
        Command::Status => command_status(&client, cli.json).await,
        Command::Accounts(command) => command_accounts(&client, command, cli.json).await,
        Command::Quota(command) => command_quota(&client, command, cli.json).await,
        Command::Keys(command) => command_keys(&client, command, cli.json).await,
        Command::Models(command) => command_models(&client, command, cli.json).await,
        Command::Usage(command) => command_usage(&client, command, cli.json).await,
        Command::Notifications(command) => command_notifications(&client, command, cli.json).await,
        Command::Raw(args) => command_raw(&client, args, cli.json).await,
    }
}

impl GatewayClient {
    fn new(base_url: String) -> Result<Self, String> {
        let base_url = normalize_base_url(&base_url);
        let session_path = session_path(&base_url)?;
        let session_cookie = fs::read_to_string(&session_path)
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());
        let http = Client::builder()
            .timeout(Duration::from_secs(30))
            .gzip(true)
            .build()
            .map_err(|err| err.to_string())?;
        Ok(Self {
            base_url,
            http,
            session_cookie,
            session_path,
        })
    }

    async fn get(&self, path: &str) -> Result<ApiResponse, String> {
        self.request(Method::GET, path, None, None).await
    }

    async fn post_json(&self, path: &str, body: Option<Value>) -> Result<ApiResponse, String> {
        self.request(Method::POST, path, body, None).await
    }

    async fn post_form(&self, path: &str, form: &[(&str, String)]) -> Result<ApiResponse, String> {
        let body = serde_urlencoded::to_string(form).map_err(|err| err.to_string())?;
        self.request(Method::POST, path, None, Some(body)).await
    }

    async fn request(
        &self,
        method: Method,
        path: &str,
        json_body: Option<Value>,
        form_body: Option<String>,
    ) -> Result<ApiResponse, String> {
        let mut request = self
            .http
            .request(method, self.url(path))
            .header(USER_AGENT, USER_AGENT_VALUE);
        if let Some(cookie) = self.session_cookie.as_ref() {
            request = request.header(COOKIE, cookie);
        }
        if let Some(body) = json_body {
            request = request.json(&body);
        }
        if let Some(body) = form_body {
            request = request
                .header("content-type", "application/x-www-form-urlencoded")
                .body(body);
        }
        let response = request.send().await.map_err(|err| err.to_string())?;
        response_to_api_response(response).await
    }

    async fn login(&mut self, otp: &str, api_key: Option<&str>) -> Result<ApiResponse, String> {
        let mut fields = vec![("otp", otp.trim())];
        if let Some(api_key) = api_key.map(str::trim).filter(|value| !value.is_empty()) {
            fields.push(("api_key", api_key));
        }
        let body = serde_urlencoded::to_string(fields).map_err(|err| err.to_string())?;
        let response = self
            .http
            .post(self.url("/admin/login"))
            .header(USER_AGENT, USER_AGENT_VALUE)
            .header("content-type", "application/x-www-form-urlencoded")
            .body(body)
            .send()
            .await
            .map_err(|err| err.to_string())?;
        let cookie = extract_admin_cookie(response.headers());
        let api = response_to_api_response(response).await?;
        if api.status.is_success() {
            let cookie = cookie.ok_or_else(|| {
                "login succeeded but no admin session cookie was returned".to_string()
            })?;
            self.session_cookie = Some(cookie.clone());
            if let Some(parent) = self.session_path.parent() {
                fs::create_dir_all(parent).map_err(|err| err.to_string())?;
            }
            write_session_cookie(&self.session_path, &cookie)?;
        }
        Ok(api)
    }

    fn clear_session(&mut self) -> Result<(), String> {
        self.session_cookie = None;
        match fs::remove_file(&self.session_path) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err.to_string()),
        }
    }

    fn url(&self, path: &str) -> String {
        if path.starts_with("http://") || path.starts_with("https://") {
            return path.to_string();
        }
        let path = if path.starts_with('/') {
            path.to_string()
        } else {
            format!("/{path}")
        };
        format!("{}{}", self.base_url, path)
    }
}

async fn response_to_api_response(response: reqwest::Response) -> Result<ApiResponse, String> {
    let status = response.status();
    let text = response.text().await.map_err(|err| err.to_string())?;
    let body = if text.trim().is_empty() {
        Value::Null
    } else {
        serde_json::from_str(&text).unwrap_or_else(|_| json!({ "text": text }))
    };
    Ok(ApiResponse { status, body })
}

fn extract_admin_cookie(headers: &reqwest::header::HeaderMap) -> Option<String> {
    headers.get_all(SET_COOKIE).iter().find_map(|value| {
        let text = value.to_str().ok()?;
        let cookie = text.split(';').next()?.trim();
        if cookie.starts_with(&format!("{SESSION_COOKIE_NAME}=")) {
            Some(cookie.to_string())
        } else {
            None
        }
    })
}

async fn command_status(client: &GatewayClient, json_output: bool) -> Result<(), String> {
    let health = client.get("/health").await?;
    let ready = client.get("/ready").await?;
    let session = client.get("/admin/session").await?;
    if json_output {
        return print_response(
            true,
            json!({
                "base_url": client.base_url,
                "health": health.body,
                "health_status": health.status.as_u16(),
                "ready": ready.body,
                "ready_status": ready.status.as_u16(),
                "session": session.body,
                "session_status": session.status.as_u16()
            }),
        );
    }
    println!("Base URL: {}", client.base_url);
    println!(
        "Health:   {} {}",
        health.status.as_u16(),
        message_from(&health.body)
    );
    println!(
        "Ready:    {} {}",
        ready.status.as_u16(),
        message_from(&ready.body)
    );
    println!(
        "Session:  enabled={} configured={} authenticated={}",
        bool_at(&session.body, &["enabled"]),
        bool_at(&session.body, &["configured"]),
        bool_at(&session.body, &["authenticated"])
    );
    Ok(())
}

async fn command_accounts(
    client: &GatewayClient,
    command: AccountsCommand,
    json_output: bool,
) -> Result<(), String> {
    let action = command.action.unwrap_or(AccountsAction::List);
    match action {
        AccountsAction::List => {
            let data = fetch_gateway_data(client).await?;
            if json_output {
                return print_response(true, accounts_json(&data.accounts));
            }
            print_accounts(&data.accounts);
            Ok(())
        }
        AccountsAction::Enable(args) => set_account_enabled(client, args, true, json_output).await,
        AccountsAction::Disable(args) => {
            set_account_enabled(client, args, false, json_output).await
        }
        AccountsAction::Delete(args) => {
            let data = fetch_gateway_data(client).await?;
            let account = find_account(&data.accounts, &args.target, args.provider.as_deref())?;
            if account.file_name.is_empty() {
                return Err("selected account has no credential file name".to_string());
            }
            let response = client
                .post_form(
                    "/credentials/delete",
                    &[("file_name", account.file_name.clone())],
                )
                .await?;
            ensure_success(&response)?;
            print_response(json_output, response.body)
        }
        AccountsAction::Priority(args) => {
            let data = fetch_gateway_data(client).await?;
            let account = find_account(&data.accounts, &args.target, args.provider.as_deref())?;
            if account.key.is_empty() {
                return Err("selected account has no routing key".to_string());
            }
            let response = client
                .post_json(
                    "/admin/account-routing/priority",
                    Some(json!({
                        "provider": account.provider,
                        "account": account.key,
                        "priority": !args.off
                    })),
                )
                .await?;
            ensure_success(&response)?;
            print_response(json_output, response.body)
        }
    }
}

async fn set_account_enabled(
    client: &GatewayClient,
    args: AccountTargetArgs,
    enabled: bool,
    json_output: bool,
) -> Result<(), String> {
    let data = fetch_gateway_data(client).await?;
    let account = find_account(&data.accounts, &args.target, args.provider.as_deref())?;
    if account.file_name.is_empty() {
        return Err("selected account has no credential file name".to_string());
    }
    let response = client
        .post_form(
            "/credentials/toggle",
            &[
                ("file_name", account.file_name.clone()),
                ("enabled", enabled.to_string()),
            ],
        )
        .await?;
    ensure_success(&response)?;
    print_response(json_output, response.body)
}

async fn command_quota(
    client: &GatewayClient,
    command: QuotaCommand,
    json_output: bool,
) -> Result<(), String> {
    if command.refresh {
        let mut form = Vec::new();
        if let Some(provider) = command
            .provider
            .as_deref()
            .and_then(normalize_refresh_provider)
        {
            form.push(("provider", provider.to_string()));
        }
        let response = client.post_form("/models/refresh", &form).await?;
        ensure_success(&response)?;
        if json_output {
            return print_response(true, response.body);
        }
        println!("{}", message_from(&response.body));
    }

    let mut rows = Vec::new();
    if let Some(provider) = command.provider.as_deref() {
        let spec =
            provider_spec(provider).ok_or_else(|| format!("unknown provider '{provider}'"))?;
        let response = client.get(spec.quota_path).await?;
        ensure_success(&response)?;
        rows.extend(quota_rows_from_provider(spec.key, &response.body));
        if json_output {
            if command.one_limit {
                return print_response(true, quota_json(&compact_quota_rows(&rows)));
            }
            return print_response(true, response.body);
        }
    } else {
        for spec in PROVIDERS {
            let response = client.get(spec.quota_path).await?;
            if response.status.is_success() {
                rows.extend(quota_rows_from_provider(spec.key, &response.body));
            }
        }
        if json_output {
            let output_rows = quota_rows_for_mode(&rows, command.one_limit);
            return print_response(true, quota_json(&output_rows));
        }
    }
    let output_rows = quota_rows_for_mode(&rows, command.one_limit);
    print_quota(&output_rows);
    Ok(())
}

async fn command_keys(
    client: &GatewayClient,
    command: KeysCommand,
    json_output: bool,
) -> Result<(), String> {
    match command.action.unwrap_or(KeysAction::List) {
        KeysAction::List => {
            let response = client.get("/admin/api-keys").await?;
            ensure_success(&response)?;
            if json_output {
                return print_response(true, response.body);
            }
            print_keys(&key_rows_from_response(&response.body));
            Ok(())
        }
        KeysAction::Create(args) => {
            let access = key_create_access(&args)?;
            let response = client
                .post_json(
                    "/admin/api-keys/create",
                    Some(json!({ "label": args.label, "access": access })),
                )
                .await?;
            ensure_success(&response)?;
            if json_output {
                return print_response(true, response.body);
            }
            println!(
                "Created API key: {}",
                string_at(&response.body, &["plain_text_key"])
            );
            println!("Store it now; the gateway will not show it again.");
            Ok(())
        }
        KeysAction::Update(args) => {
            let access = parse_json_arg(&args.access_json)?;
            if !access.is_object() {
                return Err("access JSON must be an object".to_string());
            }
            let response = client
                .post_json(
                    "/admin/api-keys/access",
                    Some(json!({
                        "id": args.id, "access": access
                    })),
                )
                .await?;
            ensure_success(&response)?;
            print_response(json_output, response.body)
        }
        KeysAction::Quotas { id } => {
            let mut path = "/admin/api-keys/quotas".to_string();
            push_query(&mut path, "id", id.as_deref());
            let response = client.get(&path).await?;
            ensure_success(&response)?;
            print_response(json_output, response.body)
        }
        KeysAction::Revoke { id } => {
            let response = client
                .post_json("/admin/api-keys/revoke", Some(json!({ "id": id })))
                .await?;
            ensure_success(&response)?;
            print_response(json_output, response.body)
        }
    }
}

fn key_create_access(args: &KeyCreateArgs) -> Result<Value, String> {
    let mut access = match args.access_json.as_deref() {
        Some(raw) => parse_json_arg(raw)?,
        None => json!({ "all": true, "providers": [] }),
    };
    let object = access
        .as_object_mut()
        .ok_or("access JSON must be an object")?;
    if let Some(limit) = args.max_estimated_input_tokens_per_request {
        if limit == 0 {
            return Err(
                "max estimated input tokens per request must be greater than zero".to_string(),
            );
        }
        // Explicit flags override JSON just like --quota-json. Remove the
        // compatibility alias so the server does not see duplicate cap fields.
        object.remove("prompt_token_limit");
        object.insert(
            "max_estimated_input_tokens_per_request".to_string(),
            json!(limit),
        );
    }
    if let Some(raw) = args.quota_json.as_deref() {
        let quota = parse_json_arg(raw)?;
        if !quota.is_object() {
            return Err("quota JSON must be an object containing timezone and rules".to_string());
        }
        object.insert("quota".to_string(), quota);
    }
    Ok(access)
}

async fn command_models(
    client: &GatewayClient,
    command: ModelsCommand,
    json_output: bool,
) -> Result<(), String> {
    match command.action.unwrap_or(ModelsAction::List) {
        ModelsAction::List => {
            let response = client.get("/custom-models.json").await?;
            ensure_success(&response)?;
            if json_output {
                return print_response(true, response.body);
            }
            print_models(&model_rows_from_response(&response.body));
            Ok(())
        }
        ModelsAction::Save { json: raw } => {
            let response = client
                .post_json("/custom-models/save", Some(parse_json_arg(&raw)?))
                .await?;
            ensure_success(&response)?;
            print_response(json_output, response.body)
        }
        ModelsAction::Delete { alias } => {
            let response = client
                .post_json("/custom-models/delete", Some(json!({ "alias": alias })))
                .await?;
            ensure_success(&response)?;
            print_response(json_output, response.body)
        }
        ModelsAction::Refresh { provider } => {
            let mut form = Vec::new();
            if let Some(provider) = provider.as_deref().and_then(normalize_refresh_provider) {
                form.push(("provider", provider.to_string()));
            }
            let response = client.post_form("/models/refresh", &form).await?;
            ensure_success(&response)?;
            print_response(json_output, response.body)
        }
    }
}

async fn command_usage(
    client: &GatewayClient,
    command: UsageCommand,
    json_output: bool,
) -> Result<(), String> {
    match command.action.unwrap_or(UsageAction::Summary) {
        UsageAction::Summary => {
            let response = client.get("/usage/summary.json").await?;
            ensure_success(&response)?;
            if json_output {
                return print_response(true, response.body);
            }
            print_usage_summary(&response.body);
            Ok(())
        }
        UsageAction::History(args) => {
            let mut path = format!("/usage/history.json?limit={}", args.limit);
            push_query(&mut path, "provider", args.provider.as_deref());
            push_query(&mut path, "account_key", args.account_key.as_deref());
            push_query(&mut path, "model", args.model.as_deref());
            let response = client.get(&path).await?;
            ensure_success(&response)?;
            if json_output {
                return print_response(true, response.body);
            }
            print_history(&events_from_response(&response.body));
            Ok(())
        }
    }
}

async fn command_notifications(
    client: &GatewayClient,
    command: NotificationsCommand,
    json_output: bool,
) -> Result<(), String> {
    match command.action.unwrap_or(NotificationsAction::Settings) {
        NotificationsAction::Settings => {
            let response = client.get("/notifications/settings").await?;
            ensure_success(&response)?;
            print_response(json_output, response.body)
        }
        NotificationsAction::Test => {
            let response = client.post_json("/notifications/test", None).await?;
            ensure_success(&response)?;
            print_response(json_output, response.body)
        }
    }
}

async fn command_raw(
    client: &GatewayClient,
    args: RawArgs,
    json_output: bool,
) -> Result<(), String> {
    let method = args
        .method
        .parse::<Method>()
        .map_err(|err| format!("invalid method: {err}"))?;
    let response = if method == Method::GET {
        client.get(&args.path).await?
    } else {
        let body = args.body.as_deref().map(parse_json_arg).transpose()?;
        client.request(method, &args.path, body, None).await?
    };
    if json_output {
        print_response(true, response.body)
    } else {
        println!("HTTP {}", response.status.as_u16());
        print_response(false, response.body)
    }
}

async fn run_tui(client: GatewayClient) -> Result<(), String> {
    enable_raw_mode().map_err(|err| err.to_string())?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, cursor::Hide).map_err(|err| err.to_string())?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).map_err(|err| err.to_string())?;
    let result = run_tui_loop(&mut terminal, client).await;
    disable_raw_mode().ok();
    execute!(terminal.backend_mut(), LeaveAlternateScreen, cursor::Show).ok();
    terminal.show_cursor().ok();
    result
}

fn animation_toggle_key(key: KeyEvent) -> bool {
    key.code == KeyCode::F(4)
        || (key.code == KeyCode::Char('v') && key.modifiers.contains(KeyModifiers::CONTROL))
}

async fn run_tui_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    client: GatewayClient,
) -> Result<(), String> {
    let mut app = TuiApp::new(client)?;
    let mut last_draw = Instant::now() - Duration::from_millis(250);
    loop {
        app.poll_network();
        let animated_overview = app.animation.active && app.tab == Tab::Overview;
        app.animation.poll_system(animated_overview);
        if animated_overview {
            let size = terminal.size().map_err(|err| err.to_string())?;
            app.animation.resize(size.width, size.height);
        }
        app.continue_pending_account_login(terminal)?;
        let frame_ms = if animated_overview && !app.animation.paused {
            50
        } else {
            250
        };
        let interval = Duration::from_millis(frame_ms);
        if !animated_overview || last_draw.elapsed() >= interval {
            terminal
                .draw(|frame| draw_tui(frame, &app))
                .map_err(|err| err.to_string())?;
            last_draw = Instant::now();
        }
        if !event::poll(interval.saturating_sub(last_draw.elapsed()))
            .map_err(|err| err.to_string())?
        {
            continue;
        }
        let Event::Key(key) = event::read().map_err(|err| err.to_string())? else {
            continue;
        };
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            continue;
        }
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return Ok(());
        }
        if animation_toggle_key(key) && key.kind == KeyEventKind::Press {
            if let Err(error) = app.animation.toggle() {
                app.message = error;
            } else if app.animation.active {
                app.tab = Tab::Overview;
                app.clamp_selection();
                app.animation.observe(&app.data);
                app.sync_animated_accounts();
            }
            continue;
        }
        // Modals and every ordinary Overview action use the shared dispatcher
        // below. Only scene-specific, otherwise-unused shortcuts are intercepted.
        if animated_overview && !app.has_modal() {
            match key.code {
                KeyCode::Esc => {
                    app.animation.exit();
                    continue;
                }
                KeyCode::Char(' ') if key.kind == KeyEventKind::Press => {
                    app.animation.pause();
                    continue;
                }
                KeyCode::Char('t') if key.kind == KeyEventKind::Press => {
                    app.animation.cycle_theme();
                    continue;
                }
                KeyCode::Char('b') if key.kind == KeyEventKind::Press => {
                    app.animation.toggle_bubbles();
                    continue;
                }
                KeyCode::Char('h') | KeyCode::Char('l') | KeyCode::Left | KeyCode::Right
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
                    app.animation.move_cat(
                        if matches!(key.code, KeyCode::Char('h') | KeyCode::Left) {
                            -1
                        } else {
                            1
                        },
                    );
                    app.sync_from_animated_cat();
                    continue;
                }
                KeyCode::PageDown | KeyCode::PageUp | KeyCode::Home | KeyCode::End => {
                    match key.code {
                        KeyCode::PageDown => app.animation.turn_page(1),
                        KeyCode::PageUp => app.animation.turn_page(-1),
                        KeyCode::Home => app.animation.first(),
                        _ => app.animation.last(),
                    }
                    app.sync_from_animated_cat();
                    continue;
                }
                _ => {}
            }
        }
        if app.confirmation_modal.is_some() {
            match key.code {
                KeyCode::Char('y') | KeyCode::Enter => app.confirm_selection(),
                KeyCode::Char('n')
                | KeyCode::Char('q')
                | KeyCode::Char('Q')
                | KeyCode::Char('?') => app.cancel_confirmation(),
                _ => {}
            }
            continue;
        }
        if app.show_api_key_reveal {
            match key.code {
                KeyCode::Esc
                | KeyCode::Enter
                | KeyCode::Char('?')
                | KeyCode::Char('q')
                | KeyCode::Char('Q') => {
                    app.show_api_key_reveal = false;
                }
                _ => {}
            }
            continue;
        }
        if app.show_api_key_details {
            match key.code {
                KeyCode::Esc
                | KeyCode::Enter
                | KeyCode::Char('?')
                | KeyCode::Char('q')
                | KeyCode::Char('Q') => {
                    app.show_api_key_details = false;
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    app.api_key_details_scroll = app.api_key_details_scroll.saturating_sub(1);
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    app.api_key_details_scroll = app.api_key_details_scroll.saturating_add(1);
                }
                KeyCode::PageUp => {
                    app.api_key_details_scroll = app.api_key_details_scroll.saturating_sub(10);
                }
                KeyCode::PageDown => {
                    app.api_key_details_scroll = app.api_key_details_scroll.saturating_add(10);
                }
                _ => {}
            }
            continue;
        }
        if app.show_account_details {
            match key.code {
                KeyCode::Char('s') => {
                    if let Err(error) = app.edit_claude_reset_policy(terminal) {
                        app.message = error;
                    }
                }
                KeyCode::Char('z') => {
                    if let Err(error) = app.request_reset_confirmation(terminal) {
                        app.message = error;
                    }
                }
                KeyCode::Esc
                | KeyCode::Enter
                | KeyCode::Char('?')
                | KeyCode::Char('q')
                | KeyCode::Char('Q') => {
                    app.show_account_details = false;
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    app.account_details_scroll = app.account_details_scroll.saturating_sub(1);
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    app.account_details_scroll = app.account_details_scroll.saturating_add(1);
                }
                KeyCode::PageUp => {
                    app.account_details_scroll = app.account_details_scroll.saturating_sub(10);
                }
                KeyCode::PageDown => {
                    app.account_details_scroll = app.account_details_scroll.saturating_add(10);
                }
                _ => {}
            }
            continue;
        }
        if app.show_command_modal {
            match key.code {
                KeyCode::Esc | KeyCode::Char('?') | KeyCode::Char('q') | KeyCode::Char('Q') => {
                    app.show_command_modal = false
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    app.command_modal_scroll = app.command_modal_scroll.saturating_sub(1);
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    app.command_modal_scroll = app.command_modal_scroll.saturating_add(1);
                }
                KeyCode::PageUp => {
                    app.command_modal_scroll = app.command_modal_scroll.saturating_sub(10);
                }
                KeyCode::PageDown => {
                    app.command_modal_scroll = app.command_modal_scroll.saturating_add(10);
                }
                KeyCode::Home => app.command_modal_scroll = 0,
                KeyCode::End => app.command_modal_scroll = usize::MAX,
                _ => {}
            }
            continue;
        }
        if let Some(delta) = account_reorder_delta(key) {
            app.move_selected_usage_account(delta);
            app.sync_animated_accounts();
            continue;
        }
        if let Some(delta) = provider_reorder_delta(key) {
            app.move_selected_usage_provider(delta);
            app.sync_animated_accounts();
            continue;
        }
        match key.code {
            KeyCode::Char('?') => {
                app.command_modal_scroll = 0;
                app.show_command_modal = true;
            }
            KeyCode::Tab => {
                app.next_tab();
            }
            KeyCode::BackTab => {
                app.previous_tab();
            }
            KeyCode::Down | KeyCode::Char('j') => app.move_selection(1),
            KeyCode::Up | KeyCode::Char('k') => app.move_selection(-1),
            KeyCode::Char('h') => app.previous_overview_section(),
            KeyCode::Char('l') => app.next_overview_section(),
            KeyCode::Enter => {
                if app.tab == Tab::Keys {
                    app.open_selected_api_key_details();
                } else if app.tab == Tab::Notifications {
                    app.toggle_selected_notification_account();
                } else {
                    app.open_selected_account_details();
                }
            }
            KeyCode::Esc => {}
            KeyCode::Char('v')
                if app.tab == Tab::Overview
                    && app.overview_section == OverviewSection::AccountUsage =>
            {
                app.hide_selected_usage_account();
            }
            KeyCode::Char('r' | 'R') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                app.refresh_quota(true)
            }
            KeyCode::Char('R') => app.refresh(),
            KeyCode::Char('r') => {
                if app.tab == Tab::Overview {
                    app.refresh_quota(false);
                } else {
                    app.network.refresh_visible(app.tab);
                }
            }
            KeyCode::Char('a') if app.tab == Tab::Notifications => {
                app.set_all_notification_accounts(true);
            }
            KeyCode::Char('a')
                if app.tab == Tab::Overview
                    && app.overview_section == OverviewSection::AccountUsage =>
            {
                let selected_account_index = selected_overview_account_index(&app);
                app.limit_display_mode.toggle();
                app.selected = selected_account_index
                    .and_then(|index| overview_selection_for_account(&app, index))
                    .unwrap_or(0);
                app.clamp_selection();
                app.message = format!("quota view: {}", app.limit_display_mode.label());
                app.persist_preferences();
            }
            KeyCode::Char('u') => {
                if app.tab == Tab::Notifications {
                    app.set_all_notification_accounts(false);
                } else if app.tab == Tab::Overview
                    && app.overview_section == OverviewSection::AccountUsage
                {
                    app.show_all_usage_filters();
                }
            }
            KeyCode::Char('o') => {
                if app.network.command_pending() {
                    app.message = "an action is already in progress".to_string();
                    continue;
                }
                let otp = prompt_in_terminal(terminal, "OTP: ")?;
                let api_key_required = bool_at(&app.data.session, &["requires_api_key"])
                    || bool_at(&app.data.session, &["api_key_required"]);
                let api_key = if api_key_required {
                    Some(prompt_in_terminal(terminal, "Administrator API key: ")?)
                } else {
                    None
                };
                app.start_command(TuiCommand::Login { otp, api_key });
            }
            KeyCode::Char('n') if app.tab == Tab::Keys => {
                let label = match prompt_in_terminal(terminal, "API key label (blank = API key): ")
                {
                    Ok(label) => label,
                    Err(error) => {
                        app.message = error;
                        continue;
                    }
                };
                let access = match prompt_api_key_access(terminal, None) {
                    Ok(access) => access,
                    Err(error) => {
                        app.message = error;
                        continue;
                    }
                };
                app.start_command(TuiCommand::CreateApiKey {
                    label: label.trim().to_string(),
                    access,
                });
            }
            KeyCode::Char('n')
                if app.tab == Tab::Overview
                    && app.overview_section == OverviewSection::AccountUsage =>
            {
                if let Err(error) = app.begin_add_account(terminal) {
                    app.message = error;
                }
            }
            KeyCode::Char('s') if app.tab == Tab::Notifications => {
                if let Err(error) = app.edit_notification_settings(terminal) {
                    app.message = error;
                }
            }
            KeyCode::Char('c') if app.tab == Tab::Notifications => {
                app.cycle_notification_channel();
            }
            KeyCode::Char('w') | KeyCode::Char(' ') if app.tab == Tab::Notifications => {
                app.toggle_selected_notification_account();
            }
            KeyCode::Char('i')
                if app.tab == Tab::Overview
                    && app.overview_section == OverviewSection::AccountUsage =>
            {
                if let Err(error) = app.edit_selected_compatible_account(terminal) {
                    app.message = error;
                }
            }
            KeyCode::Char('s')
                if app.tab == Tab::Overview
                    && app.overview_section == OverviewSection::AccountUsage =>
            {
                if let Err(error) = app.edit_claude_reset_policy(terminal) {
                    app.message = error;
                }
            }
            KeyCode::Char('z')
                if app.tab == Tab::Overview
                    && app.overview_section == OverviewSection::AccountUsage =>
            {
                if let Err(error) = app.request_reset_confirmation(terminal) {
                    app.message = error;
                }
            }
            KeyCode::Char('e') if app.tab == Tab::Keys => {
                if let Err(error) = app.edit_selected_api_key(terminal) {
                    app.message = error;
                }
            }
            KeyCode::Char('e') if app.tab == Tab::Notifications => {
                app.toggle_notifications_enabled();
            }
            KeyCode::Char('e')
                if app.tab == Tab::Overview
                    && matches!(
                        app.overview_section,
                        OverviewSection::CustomModels | OverviewSection::AccountUsage
                    ) =>
            {
                app.enable_selected();
            }
            KeyCode::Char('d')
                if app.tab == Tab::Overview
                    && matches!(
                        app.overview_section,
                        OverviewSection::CustomModels | OverviewSection::AccountUsage
                    ) =>
            {
                app.disable_selected();
            }
            KeyCode::Char('p') if app.tab == Tab::Notifications => {
                app.toggle_selected_notification_provider();
            }
            KeyCode::Char('p')
                if app.tab == Tab::Overview
                    && app.overview_section == OverviewSection::AccountUsage =>
            {
                app.hide_selected_usage_provider();
            }
            KeyCode::Char('x')
                if app.tab == Tab::Keys
                    || (app.tab == Tab::Overview
                        && matches!(
                            app.overview_section,
                            OverviewSection::CustomModels | OverviewSection::AccountUsage
                        )) =>
            {
                app.request_delete_confirmation();
            }
            KeyCode::Char('t') => {
                if app.tab == Tab::Notifications {
                    app.start_command(TuiCommand::TestNotification);
                }
            }
            _ => {}
        }
        app.sync_animated_accounts();
    }
}

impl TuiApp {
    fn has_modal(&self) -> bool {
        self.show_command_modal
            || self.show_account_details
            || self.show_api_key_details
            || self.show_api_key_reveal
            || self.confirmation_modal.is_some()
    }

    fn sync_animated_accounts(&mut self) {
        if !self.animation.active {
            return;
        }
        let order: Vec<_> = overview_account_usage_account_rows(self)
            .into_iter()
            .map(|row| row.account_index)
            .collect();
        if self.tab == Tab::Overview {
            if order.is_empty() {
                self.overview_section = OverviewSection::CustomModels;
            } else if matches!(
                self.overview_section,
                OverviewSection::Metrics | OverviewSection::ContextUsage
            ) {
                self.overview_section = OverviewSection::AccountUsage;
            }
            self.sync_selected_from_overview_section();
        }
        self.animation.set_view(
            order,
            self.overview_account_selected,
            self.limit_display_mode == LimitDisplayMode::AllPerAccount,
            self.overview_section == OverviewSection::CustomModels,
            self.overview_model_selected,
        );
    }

    fn sync_from_animated_cat(&mut self) {
        if self.animation.models_focused() {
            self.overview_section = OverviewSection::CustomModels;
        } else {
            self.overview_section = OverviewSection::AccountUsage;
            self.overview_account_selected = self.animation.selection();
        }
        self.sync_selected_from_overview_section();
        self.message.clear();
    }

    fn new(client: GatewayClient) -> Result<Self, String> {
        let preferences_path = preferences_path(&client.base_url)?;
        let preferences = load_tui_preferences(&preferences_path);
        Ok(Self {
            client,
            preferences_path,
            data: GatewayData::default(),
            tab: Tab::Overview,
            selected: 0,
            overview_section: OverviewSection::AccountUsage,
            overview_account_selected: 0,
            overview_model_selected: 0,
            limit_display_mode: preferences.limit_display_mode,
            hidden_usage_providers: preferences.hidden_usage_providers,
            hidden_usage_accounts: preferences.hidden_usage_accounts,
            account_orders: preferences.account_orders,
            provider_order: preferences.provider_order,
            show_command_modal: false,
            command_modal_scroll: 0,
            show_account_details: false,
            account_details_scroll: 0,
            show_api_key_details: false,
            api_key_details_scroll: 0,
            show_api_key_reveal: false,
            api_key_reveal: String::new(),
            pending_account_login: None,
            confirmation_modal: None,
            message: String::new(),
            network: TuiNetwork::new(),
            animation: animated::Animation::new(),
        })
    }

    fn refresh(&mut self) {
        self.network.refresh_all();
        self.animation.refresh_started("ALL");
        self.message.clear();
    }

    fn refresh_quota(&mut self, hard: bool) {
        if !hard && self.overview_section != OverviewSection::AccountUsage {
            self.message = "no focused account: use R to refresh all data".into();
            return;
        }
        if self.needs_login() || !self.network.has_data(Section::Session) {
            self.message = "admin login required: press o".into();
            return;
        }
        let target = if hard {
            None
        } else {
            let Some(account) =
                selected_overview_account_index(self).and_then(|i| self.data.accounts.get(i))
            else {
                self.message = "no focused account: use R to refresh all data".into();
                return;
            };
            let Some(provider) = normalize_refresh_provider(&account.provider) else {
                self.message = "provider does not support quota refresh".into();
                return;
            };
            if account.file_name.is_empty() {
                self.message =
                    "account credential file unavailable; focused refresh skipped".into();
                return;
            }
            Some((provider.to_string(), account.file_name.clone()))
        };
        if self.network.refresh_quotas(&self.client, target) {
            if hard {
                self.network.refresh_all();
            }
            self.animation
                .refresh_started(if hard { "HARD" } else { "ACCOUNT" });
            self.message = if hard {
                "fetching fresh quotas from server"
            } else {
                "refreshing focused account"
            }
            .into();
        } else {
            self.message = "a refresh/action is already in progress".into();
        }
    }

    fn start_command(&mut self, command: TuiCommand) {
        let login = matches!(command, TuiCommand::Login { .. });
        if !login && (self.needs_login() || !self.network.has_data(Section::Session)) {
            self.message = "admin login required: press o".to_string();
        } else if self.network.start_command(&self.client, command) {
            if login {
                self.animation.login_started();
            }
            self.message = "action in progress".to_string();
        } else {
            self.message = "an action is already in progress".to_string();
        }
    }

    fn edit_selected_compatible_account(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    ) -> Result<(), String> {
        if self.network.command_pending() {
            return Err("an action is already in progress".into());
        }
        if self.needs_login() || !self.network.has_data(Section::Session) {
            return Err("admin login required: press o".into());
        }
        let account = self
            .selected_account()
            .ok_or("Select a compatible account first")?;
        if account.provider != "compatible" {
            return Err("The API editor is available for compatible accounts".into());
        }
        let existing = snapshot_account_for_account(&self.data.snapshot, &account)
            .cloned()
            .ok_or("Refresh account details before editing")?;
        self.edit_compatible_account(terminal, Some(existing))
    }

    fn edit_compatible_account(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<Stdout>>,
        existing: Option<Value>,
    ) -> Result<(), String> {
        let old = existing.as_ref().unwrap_or(&Value::Null);
        let label = prompt_compatible_field(terminal, "Name", &string_at(old, &["label"]), false)?;
        let base_url = prompt_compatible_field(
            terminal,
            "OpenAI base URL (include /v1 if required)",
            &string_at(old, &["base_url"]),
            true,
        )?;
        let anthropic_base_url = prompt_compatible_field(
            terminal,
            "Anthropic base URL",
            &string_at(old, &["anthropic_base_url"]),
            true,
        )?;
        if base_url.is_empty() && anthropic_base_url.is_empty() {
            return Err("Enter at least one API base URL".into());
        }
        let api_key = prompt_secret_in_terminal(
            terminal,
            if existing.is_some() {
                "API key (blank = keep saved key): "
            } else {
                "API key: "
            },
        )?;
        let model_default = old["models"]
            .as_array()
            .map(|v| {
                v.iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default();
        let model_input = prompt_compatible_field(
            terminal,
            "Upstream model IDs (comma separated, no cmp: prefix)",
            &model_default,
            false,
        )?;
        let models: Vec<_> = model_input
            .split(',')
            .map(str::trim)
            .filter(|m| !m.is_empty())
            .collect();
        if label.is_empty() || models.is_empty() || existing.is_none() && api_key.is_empty() {
            return Err("Name, key, and at least one model are required".into());
        }
        self.start_command(TuiCommand::SaveAccountCredentials {provider:"compatible".into(),body:json!({
            "file_name":old["file_name"],"label":label,"base_url":base_url,"anthropic_base_url":anthropic_base_url,
            "api_key":api_key,"models":models
        })});
        Ok(())
    }

    fn begin_add_account(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    ) -> Result<(), String> {
        if self.network.command_pending() {
            return Err("an action is already in progress".to_string());
        }
        if self.needs_login() || !self.network.has_data(Section::Session) {
            return Err("admin login required: press o".to_string());
        }

        let provider = prompt_account_provider(terminal)?;
        match provider.as_str() {
            "codex" | "antigravity" | "gemini" | "grok" => {
                self.start_command(TuiCommand::StartAccountLogin {
                    provider,
                    label: None,
                    organization_uuid: None,
                    base_url: None,
                    account_type: None,
                });
            }
            "compatible" => self.edit_compatible_account(terminal, None)?,
            "claude" => self.begin_claude_account(terminal)?,
            "copilot" => self.begin_copilot_account(terminal)?,
            "qwen" => {
                let token = prompt_in_terminal(terminal, "Qwen browser token: ")?;
                if token.trim().is_empty() {
                    return Err("Qwen browser token is required".to_string());
                }
                self.start_command(TuiCommand::SaveAccountCredentials {
                    provider,
                    body: json!({ "token": token }),
                });
            }
            "deepseek" | "minimax" => {
                let api_key = prompt_in_terminal(terminal, "API key: ")?;
                if api_key.trim().is_empty() {
                    return Err("API key is required".to_string());
                }
                let label = prompt_optional(terminal, "Label (optional): ")?;
                let base_url = prompt_optional(terminal, "Base URL (optional): ")?;
                self.start_command(TuiCommand::SaveAccountCredentials {
                    provider: provider.clone(),
                    body: json!({
                        "api_key": api_key,
                        "label": label,
                        "base_url": base_url,
                    }),
                });
            }
            "glm" => {
                let api_key = prompt_in_terminal(terminal, "GLM API key: ")?;
                if api_key.trim().is_empty() {
                    return Err("GLM API key is required".to_string());
                }
                let label = prompt_optional(terminal, "Label (optional): ")?;
                let account_type = prompt_choice(
                    terminal,
                    "Account type [1=API usage, 2=Subscription]: ",
                    &[("1", "api_usage"), ("2", "subscription")],
                )?;
                let openai_base_url =
                    prompt_optional(terminal, "OpenAI/Codex base URL (optional): ")?;
                let anthropic_base_url =
                    prompt_optional(terminal, "Claude Code base URL (optional): ")?;
                self.start_command(TuiCommand::SaveAccountCredentials {
                    provider,
                    body: json!({
                        "api_key": api_key,
                        "label": label,
                        "account_type": account_type,
                        "openai_base_url": openai_base_url,
                        "anthropic_base_url": anthropic_base_url,
                    }),
                });
            }
            _ => unreachable!("provider picker only returns known providers"),
        }
        Ok(())
    }

    fn begin_copilot_account(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    ) -> Result<(), String> {
        let method = prompt_choice(
            terminal,
            "Copilot login [1=device login, 2=GitHub token]: ",
            &[("1", "device"), ("2", "token")],
        )?;
        let label = prompt_optional(terminal, "Label (optional): ")?;
        let account_type = prompt_choice(
            terminal,
            "Account type [1=individual, 2=business, 3=enterprise]: ",
            &[("1", "individual"), ("2", "business"), ("3", "enterprise")],
        )?;
        if method == "device" {
            self.start_command(TuiCommand::StartAccountLogin {
                provider: "copilot".to_string(),
                label,
                organization_uuid: None,
                base_url: None,
                account_type: Some(account_type),
            });
        } else {
            let github_token = prompt_in_terminal(terminal, "GitHub token: ")?;
            if github_token.trim().is_empty() {
                return Err("GitHub token is required".to_string());
            }
            self.start_command(TuiCommand::SaveAccountCredentials {
                provider: "copilot".to_string(),
                body: json!({
                    "github_token": github_token,
                    "label": label,
                    "account_type": account_type,
                }),
            });
        }
        Ok(())
    }

    fn begin_claude_account(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    ) -> Result<(), String> {
        let method = prompt_choice(
            terminal,
            "Claude login [1=OAuth, 2=access token, 3=cookie]: ",
            &[("1", "oauth"), ("2", "token"), ("3", "cookie")],
        )?;
        let label = prompt_optional(terminal, "Label (optional): ")?;
        let organization_uuid = prompt_optional(terminal, "Organization UUID (optional): ")?;
        let base_url = prompt_optional(terminal, "API base URL (optional): ")?;
        if method == "oauth" {
            self.start_command(TuiCommand::StartAccountLogin {
                provider: "claude".to_string(),
                label,
                organization_uuid,
                base_url,
                account_type: None,
            });
        } else if method == "token" {
            let access_token = prompt_in_terminal(terminal, "Claude access token: ")?;
            if access_token.trim().is_empty() {
                return Err("Claude access token is required".to_string());
            }
            let refresh_token = prompt_optional(terminal, "Refresh token (optional): ")?;
            self.start_command(TuiCommand::SaveAccountCredentials {
                provider: "claude".to_string(),
                body: json!({
                    "access_token": access_token,
                    "refresh_token": refresh_token,
                    "label": label,
                    "organization_uuid": organization_uuid,
                    "base_url": base_url,
                }),
            });
        } else {
            let cookie = prompt_in_terminal(terminal, "Claude.ai cookie: ")?;
            if cookie.trim().is_empty() {
                return Err("Claude.ai cookie is required".to_string());
            }
            self.start_command(TuiCommand::SaveAccountCredentials {
                provider: "claude".to_string(),
                body: json!({
                    "cookie": cookie,
                    "label": label,
                    "organization_uuid": organization_uuid,
                    "base_url": base_url,
                }),
            });
        }
        Ok(())
    }

    fn continue_pending_account_login(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    ) -> Result<(), String> {
        let Some(pending) = self.pending_account_login.take() else {
            return Ok(());
        };
        match pending {
            AccountLoginPrompt::Browser {
                provider,
                url,
                state,
                label,
                organization_uuid,
                base_url,
            } => {
                open_account_login_url(&url);
                let notice = format!(
                    "Open this URL in a browser to finish the {} login:\n{}",
                    provider, url
                );
                let input = prompt_in_terminal_with_notice(
                    terminal,
                    &notice,
                    "Callback URL / authorization code (blank = cancel): ",
                )?;
                if input.trim().is_empty() {
                    self.message = "account login cancelled".to_string();
                    return Ok(());
                }
                let project_id = if provider == "gemini" {
                    prompt_optional(terminal, "Gemini Project ID (optional): ")?
                } else {
                    None
                };
                self.start_command(TuiCommand::CompleteAccountLogin {
                    provider,
                    input,
                    state,
                    project_id,
                    label,
                    organization_uuid,
                    base_url,
                    account_type: None,
                    verification_uri: None,
                    user_code: None,
                });
            }
            AccountLoginPrompt::CopilotDevice {
                device_code,
                verification_uri,
                user_code,
                label,
                account_type,
            } => {
                open_account_login_url(&verification_uri);
                let notice = format!(
                    "Open {} and enter this code: {}\nApprove GitHub access, then return here.",
                    verification_uri, user_code
                );
                let input = prompt_in_terminal_with_notice(
                    terminal,
                    &notice,
                    "Press Enter to check (q = cancel): ",
                )?;
                if input.trim().eq_ignore_ascii_case("q") {
                    self.message = "Copilot login cancelled".to_string();
                    return Ok(());
                }
                self.start_command(TuiCommand::CompleteAccountLogin {
                    provider: "copilot".to_string(),
                    input: device_code,
                    state: None,
                    project_id: None,
                    label,
                    organization_uuid: None,
                    base_url: None,
                    account_type: Some(account_type),
                    verification_uri: Some(verification_uri),
                    user_code: Some(user_code),
                });
            }
        }
        Ok(())
    }

    fn expire_session(&mut self) {
        self.network.invalidate(true);
        self.network.forget_data();
        self.data = GatewayData {
            session: json!({ "enabled": true, "authenticated": false }),
            ..GatewayData::default()
        };
        self.animation.observe(&self.data);
        self.confirmation_modal = None;
        self.message = "admin login required: press o".to_string();
    }

    fn poll_network(&mut self) {
        while let Some(event) = self.network.next_event() {
            match event {
                NetworkEvent::Section {
                    section,
                    result: Ok(body),
                    ..
                } => {
                    self.apply_section(section, body);
                }
                NetworkEvent::Section {
                    section,
                    result: Err(error),
                    ..
                } => {
                    if error.unauthorized {
                        self.expire_session();
                    }
                    if section == Section::Session {
                        self.animation.login_failed();
                    }
                }
                NetworkEvent::QuotaRefresh {
                    result: Ok(body), ..
                } => {
                    self.apply_section(Section::Snapshot, body);
                    self.message = "quota refresh complete".into();
                }
                NetworkEvent::QuotaRefresh {
                    result: Err(error), ..
                } => {
                    if error.unauthorized {
                        self.expire_session();
                    }
                    self.message = error.message;
                }
                NetworkEvent::Command(Ok(outcome)) => {
                    if let Some(reveal) = outcome.reveal {
                        self.api_key_reveal = reveal;
                        self.show_api_key_reveal = true;
                    }
                    if let Some(client) = outcome.client {
                        self.animation.login_accepted();
                        self.client = client;
                        self.network.forget_data();
                        self.data = GatewayData::default();
                        self.animation.observe(&self.data);
                    }
                    self.pending_account_login = outcome.account_login;
                    if outcome.refresh {
                        // Reject reads started before the mutation/login finished.
                        self.network.invalidate(true);
                    }
                    self.message = outcome.message;
                }
                NetworkEvent::Command(Err(error)) => {
                    self.animation.login_failed();
                    if error.unauthorized {
                        self.expire_session();
                    }
                    self.message = error.message;
                }
            }
        }
        let authenticated = self.network.has_data(Section::Session) && !self.needs_login();
        self.network.schedule(&self.client, self.tab, authenticated);
        self.animation
            .refresh_pending(self.network.refresh_pending(), self.network.reads_pending());
    }

    fn apply_section(&mut self, section: Section, body: Value) {
        // Selections belong to identities, not positions in refreshed/sorted rows.
        let key = self.data.keys.get(self.selected).map(|row| row.id.clone());
        let notification_key = notification_account_rows(&self.data.notifications)
            .get(self.selected)
            .map(|row| row.key.clone());
        let model = self
            .data
            .models
            .get(self.overview_model_selected)
            .map(|row| row.alias.clone());
        let overview = selected_overview_usage_row(self).map(|row| (row.provider, row.account_key));
        match section {
            Section::Session => {
                self.data.session = body;
                if self.needs_login() {
                    let session = self.data.session.take();
                    self.network.forget_data();
                    self.data = GatewayData {
                        session,
                        ..GatewayData::default()
                    };
                    self.confirmation_modal = None;
                    self.message = "admin login required: press o".to_string();
                }
            }
            Section::Summary => {
                self.data.summary = body;
                self.data.accounts = account_rows(&self.data.summary, &self.data.routing);
                apply_account_runtime_status(&mut self.data.accounts, &self.data.snapshot);
                apply_account_attention(
                    &mut self.data.accounts,
                    &self.data.snapshot,
                    &self.data.quotas,
                );
            }
            Section::Routing => {
                self.data.routing = body;
                self.data.accounts = account_rows(&self.data.summary, &self.data.routing);
                apply_account_runtime_status(&mut self.data.accounts, &self.data.snapshot);
                apply_account_attention(
                    &mut self.data.accounts,
                    &self.data.snapshot,
                    &self.data.quotas,
                );
            }
            Section::Snapshot => {
                self.data.quotas = quota_rows_from_snapshot(&body);
                self.data.snapshot = body;
                apply_account_runtime_status(&mut self.data.accounts, &self.data.snapshot);
                apply_account_attention(
                    &mut self.data.accounts,
                    &self.data.snapshot,
                    &self.data.quotas,
                );
            }
            Section::Models => self.data.models = model_rows_from_response(&body),
            Section::Chart => self.data.usage_buckets = usage_buckets_from_context(&body),
            Section::Keys => {
                self.data.key_quota_summaries =
                    body.get("quota_summaries").cloned().unwrap_or(Value::Null);
                self.data.keys = key_rows_from_response(&body);
            }
            Section::Notifications => self.data.notifications = body,
        }
        if self.tab == Tab::Keys {
            if let Some(index) = self
                .data
                .keys
                .iter()
                .position(|row| Some(&row.id) == key.as_ref())
            {
                self.selected = index;
            }
        }
        if self.tab == Tab::Notifications {
            if let Some(index) = notification_account_rows(&self.data.notifications)
                .iter()
                .position(|row| Some(&row.key) == notification_key.as_ref())
            {
                self.selected = index;
            }
        }
        if let Some(index) = self
            .data
            .models
            .iter()
            .position(|row| Some(&row.alias) == model.as_ref())
        {
            self.overview_model_selected = index;
        }
        if let Some(index) = overview_account_usage_account_rows(self)
            .into_iter()
            .position(|row| {
                overview.as_ref().is_some_and(|(provider, key)| {
                    &row.provider == provider && &row.account_key == key
                })
            })
        {
            self.overview_account_selected = index;
        }
        self.clamp_selection();
        self.animation.observe(&self.data);
        self.sync_animated_accounts();
    }

    fn needs_login(&self) -> bool {
        bool_at(&self.data.session, &["enabled"])
            && !bool_at(&self.data.session, &["authenticated"])
    }

    fn next_tab(&mut self) {
        let tabs = Tab::all();
        let index = tabs.iter().position(|tab| *tab == self.tab).unwrap_or(0);
        self.tab = tabs[(index + 1) % tabs.len()];
        self.selected = 0;
    }

    fn previous_tab(&mut self) {
        let tabs = Tab::all();
        let index = tabs.iter().position(|tab| *tab == self.tab).unwrap_or(0);
        self.tab = tabs[(index + tabs.len() - 1) % tabs.len()];
        self.selected = 0;
    }

    fn move_selection(&mut self, delta: isize) {
        if self.tab == Tab::Overview {
            self.move_overview_selection(delta);
            return;
        }
        let len = self.selection_len();
        if len == 0 {
            self.selected = 0;
            return;
        }
        let next = (self.selected as isize + delta).clamp(0, len as isize - 1);
        self.selected = next as usize;
    }

    fn move_selected_usage_account(&mut self, delta: isize) {
        if self.tab != Tab::Overview || self.overview_section != OverviewSection::AccountUsage {
            return;
        }
        let Some(selected) = selected_overview_usage_row(self) else {
            return;
        };
        let rows = overview_account_usage_account_rows(self);
        let Some(current_index) = rows
            .iter()
            .position(|row| row.account_index == selected.account_index)
        else {
            return;
        };
        let next_index = moved_index(current_index, rows.len(), delta);
        if next_index == current_index {
            self.message = format!(
                "{} is already at the provider boundary",
                selected.account_label
            );
            return;
        }
        let Some(target) = rows.get(next_index) else {
            return;
        };
        if target.provider != selected.provider {
            self.message = "accounts can only be reordered within a provider".to_string();
            return;
        }

        let provider = selected.provider.clone();
        let selected_key = selected.account_key.clone();
        let target_key = target.account_key.clone();
        let mut order = account_order_for_provider(self, &provider);
        let Some(selected_order_index) = order.iter().position(|key| key == &selected_key) else {
            return;
        };
        let selected_key = order.remove(selected_order_index);
        let Some(target_order_index) = order.iter().position(|key| key == &target_key) else {
            return;
        };
        let insert_at = if delta < 0 {
            target_order_index
        } else {
            target_order_index + 1
        };
        order.insert(insert_at.min(order.len()), selected_key);
        self.account_orders.insert(provider, order);
        self.overview_account_selected =
            overview_selection_for_account(self, selected.account_index).unwrap_or(current_index);
        self.sync_selected_from_overview_section();
        self.message = format!(
            "moved account {} {}",
            selected.account_label,
            if delta < 0 { "up" } else { "down" }
        );
        self.persist_preferences();
    }

    fn move_selected_usage_provider(&mut self, delta: isize) {
        if self.tab != Tab::Overview || self.overview_section != OverviewSection::AccountUsage {
            return;
        }
        let Some(selected) = selected_overview_usage_row(self) else {
            return;
        };
        let current_account_selection = self.overview_account_selected;
        let groups = overview_account_usage_groups(self);
        let Some(current_index) = groups
            .iter()
            .position(|group| group.provider == selected.provider)
        else {
            return;
        };
        let next_index = moved_index(current_index, groups.len(), delta);
        if next_index == current_index {
            self.message = format!(
                "provider {} is already at the display boundary",
                selected.provider
            );
            return;
        }
        let Some(target) = groups.get(next_index) else {
            return;
        };
        let provider = selected.provider.clone();
        let target_provider = target.provider.clone();
        let mut order = provider_order_for_app(self);
        let Some(selected_order_index) = order.iter().position(|key| key == &provider) else {
            return;
        };
        let selected_provider = order.remove(selected_order_index);
        let Some(target_order_index) = order.iter().position(|key| key == &target_provider) else {
            return;
        };
        let insert_at = if delta < 0 {
            target_order_index
        } else {
            target_order_index + 1
        };
        order.insert(insert_at.min(order.len()), selected_provider);
        self.provider_order = order;
        self.overview_account_selected =
            overview_selection_for_account(self, selected.account_index)
                .unwrap_or(current_account_selection);
        self.sync_selected_from_overview_section();
        self.message = format!(
            "moved provider {} {}",
            provider,
            if delta < 0 { "up" } else { "down" }
        );
        self.persist_preferences();
    }

    fn clamp_selection(&mut self) {
        self.clamp_overview_selections();
        let len = self.selection_len();
        if len == 0 {
            self.selected = 0;
        } else if self.selected >= len {
            self.selected = len - 1;
        }
    }

    fn selection_len(&self) -> usize {
        match self.tab {
            Tab::Overview => match self.overview_section {
                OverviewSection::CustomModels => self.data.models.len(),
                OverviewSection::AccountUsage => overview_account_usage_account_count(self),
                _ => 0,
            },
            Tab::Keys => self.data.keys.len(),
            Tab::Notifications => notification_account_rows(&self.data.notifications).len(),
        }
    }

    fn move_overview_selection(&mut self, delta: isize) {
        match self.overview_section {
            OverviewSection::CustomModels => {
                self.overview_model_selected =
                    moved_index(self.overview_model_selected, self.data.models.len(), delta);
            }
            OverviewSection::AccountUsage => {
                self.overview_account_selected = moved_index(
                    self.overview_account_selected,
                    overview_account_usage_account_count(self),
                    delta,
                );
            }
            _ => {}
        }
        self.sync_selected_from_overview_section();
    }

    fn previous_overview_section(&mut self) {
        if self.tab != Tab::Overview {
            return;
        }
        let sections = OverviewSection::all();
        let index = sections
            .iter()
            .position(|section| *section == self.overview_section)
            .unwrap_or(0);
        self.overview_section = sections[(index + sections.len() - 1) % sections.len()];
        self.sync_selected_from_overview_section();
        self.message = format!("focused section: {}", self.overview_section.title());
    }

    fn next_overview_section(&mut self) {
        if self.tab != Tab::Overview {
            return;
        }
        let sections = OverviewSection::all();
        let index = sections
            .iter()
            .position(|section| *section == self.overview_section)
            .unwrap_or(0);
        self.overview_section = sections[(index + 1) % sections.len()];
        self.sync_selected_from_overview_section();
        self.message = format!("focused section: {}", self.overview_section.title());
    }

    fn sync_selected_from_overview_section(&mut self) {
        if self.tab != Tab::Overview {
            return;
        }
        self.selected = match self.overview_section {
            OverviewSection::CustomModels => self.overview_model_selected,
            OverviewSection::AccountUsage => self.overview_account_selected,
            _ => 0,
        };
    }

    fn clamp_overview_selections(&mut self) {
        self.overview_model_selected =
            clamp_index(self.overview_model_selected, self.data.models.len());
        self.overview_account_selected = clamp_index(
            self.overview_account_selected,
            overview_account_usage_account_count(self),
        );
        self.sync_selected_from_overview_section();
    }

    fn selected_account(&self) -> Option<AccountRow> {
        let account_index = if self.tab == Tab::Overview {
            selected_overview_account_index(self)?
        } else {
            self.selected
        };
        self.data.accounts.get(account_index).cloned()
    }

    fn selected_api_key(&self) -> Option<&KeyRow> {
        (self.tab == Tab::Keys).then(|| self.data.keys.get(self.selected))?
    }

    fn selected_notification_account(&self) -> Option<NotificationAccountRow> {
        (self.tab == Tab::Notifications).then(|| {
            notification_account_rows(&self.data.notifications)
                .into_iter()
                .nth(self.selected)
        })?
    }

    fn selected_model(&self) -> Option<ModelRow> {
        if self.tab == Tab::Overview && self.overview_section == OverviewSection::CustomModels {
            return self.data.models.get(self.overview_model_selected).cloned();
        }
        None
    }

    fn open_selected_account_details(&mut self) {
        if self.tab == Tab::Overview && self.overview_section == OverviewSection::AccountUsage {
            if self.selected_account().is_some() {
                self.account_details_scroll = 0;
                self.show_account_details = true;
            } else {
                self.message = "no account selected".to_string();
            }
        }
    }

    fn open_selected_api_key_details(&mut self) {
        if self.selected_api_key().is_some() {
            self.api_key_details_scroll = 0;
            self.show_api_key_details = true;
        } else {
            self.message = "no API key selected".to_string();
        }
    }

    fn edit_selected_api_key(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    ) -> Result<(), String> {
        let Some(key) = self.selected_api_key().cloned() else {
            self.message = "no API key selected".to_string();
            return Ok(());
        };
        if key.revoked {
            self.message = "revoked API keys cannot be edited".to_string();
            return Ok(());
        }
        let access = prompt_api_key_access(terminal, Some(&key.access_json))?;
        self.start_command(TuiCommand::UpdateApiKeyAccess { id: key.id, access });
        Ok(())
    }

    fn update_notification_settings(&mut self, body: Value) {
        if self.tab == Tab::Notifications {
            self.start_command(TuiCommand::UpdateNotificationSettings { body });
        }
    }

    fn toggle_notifications_enabled(&mut self) {
        let settings = notification_settings_value(&self.data.notifications);
        let body = notification_settings_update_body(
            !bool_at(settings, &["enabled"]),
            notification_channel(settings),
            notification_watched_keys(settings),
        );
        self.update_notification_settings(body);
    }

    fn cycle_notification_channel(&mut self) {
        let settings = notification_settings_value(&self.data.notifications);
        let channel = if notification_channel(settings) == "telegram" {
            "google_chat"
        } else {
            "telegram"
        };
        let body = notification_settings_update_body(
            bool_at(settings, &["enabled"]),
            channel.to_string(),
            notification_watched_keys(settings),
        );
        self.update_notification_settings(body);
    }

    fn toggle_selected_notification_account(&mut self) {
        let Some(account) = self.selected_notification_account() else {
            self.message = "no notification account selected".to_string();
            return;
        };
        let settings = notification_settings_value(&self.data.notifications);
        let mut watched = notification_watched_keys(settings);
        if let Some(index) = watched.iter().position(|key| key == &account.key) {
            watched.remove(index);
        } else {
            watched.push(account.key);
            watched.sort();
        }
        let body = notification_settings_update_body(
            bool_at(settings, &["enabled"]),
            notification_channel(settings),
            watched,
        );
        self.update_notification_settings(body);
    }

    fn set_all_notification_accounts(&mut self, watched: bool) {
        let settings = notification_settings_value(&self.data.notifications);
        let keys = if watched {
            notification_account_rows(&self.data.notifications)
                .into_iter()
                .map(|account| account.key)
                .collect()
        } else {
            Vec::new()
        };
        let body = notification_settings_update_body(
            bool_at(settings, &["enabled"]),
            notification_channel(settings),
            keys,
        );
        self.update_notification_settings(body);
    }

    fn toggle_selected_notification_provider(&mut self) {
        let Some(account) = self.selected_notification_account() else {
            self.message = "no notification account selected".to_string();
            return;
        };
        let rows = notification_account_rows(&self.data.notifications);
        let provider = normalize_usage_provider(&account.provider);
        let provider_rows = rows
            .iter()
            .filter(|row| normalize_usage_provider(&row.provider) == provider)
            .collect::<Vec<_>>();
        let watch = !provider_rows.iter().all(|row| row.watched);
        let provider_keys = provider_rows
            .into_iter()
            .map(|row| row.key.clone())
            .collect::<HashSet<_>>();
        let settings = notification_settings_value(&self.data.notifications);
        let mut watched = notification_watched_keys(settings)
            .into_iter()
            .filter(|key| !provider_keys.contains(key))
            .collect::<HashSet<_>>();
        if watch {
            watched.extend(provider_keys);
        }
        let body = notification_settings_update_body(
            bool_at(settings, &["enabled"]),
            notification_channel(settings),
            watched.into_iter().collect(),
        );
        self.update_notification_settings(body);
    }

    fn edit_notification_settings(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    ) -> Result<(), String> {
        let settings = notification_settings_value(&self.data.notifications).clone();
        let body = prompt_notification_settings(terminal, &settings)?;
        self.update_notification_settings(body);
        Ok(())
    }

    fn enable_selected(&mut self) {
        if self.overview_section == OverviewSection::CustomModels {
            self.toggle_selected_model(true);
        } else {
            self.toggle_selected_account(true);
        }
    }

    fn disable_selected(&mut self) {
        if self.overview_section == OverviewSection::CustomModels {
            self.toggle_selected_model(false);
        } else {
            self.toggle_selected_account(false);
        }
    }

    fn hide_selected_usage_account(&mut self) {
        if self.tab != Tab::Overview {
            return;
        }
        let Some(row) = selected_overview_usage_row(self) else {
            self.message = "no visible account usage row selected".to_string();
            return;
        };
        self.hidden_usage_accounts
            .insert((row.provider.clone(), row.account_key));
        self.clamp_selection();
        self.message = format!("hidden account from usage view: {}", row.label);
        self.persist_preferences();
    }

    fn hide_selected_usage_provider(&mut self) {
        if self.tab != Tab::Overview {
            return;
        }
        let Some(row) = selected_overview_usage_row(self) else {
            self.message = "no visible account usage row selected".to_string();
            return;
        };
        self.hidden_usage_providers.insert(row.provider.clone());
        self.clamp_selection();
        self.message = format!(
            "hidden provider from usage view: {}",
            provider_label(&row.provider)
        );
        self.persist_preferences();
    }

    fn show_all_usage_filters(&mut self) {
        let hidden = self.hidden_usage_providers.len() + self.hidden_usage_accounts.len();
        self.hidden_usage_providers.clear();
        self.hidden_usage_accounts.clear();
        self.clamp_selection();
        self.message = if hidden == 0 {
            "no account usage filters were active".to_string()
        } else {
            "showing all account usage rows".to_string()
        };
        self.persist_preferences();
    }

    fn usage_filter_count(&self) -> usize {
        self.hidden_usage_providers.len() + self.hidden_usage_accounts.len()
    }

    fn persist_preferences(&mut self) {
        if let Err(err) = save_tui_preferences(&self.preferences_path, self) {
            self.message = format!("{}; preferences not saved: {}", self.message, err);
        }
    }

    fn toggle_selected_account(&mut self, enabled: bool) {
        if self.tab != Tab::Overview {
            return;
        }
        let Some(account) = self.selected_account() else {
            return;
        };
        if account.file_name.is_empty() {
            self.message = "selected account has no credential file".to_string();
            return;
        }
        self.start_command(TuiCommand::ToggleAccount {
            file_name: account.file_name,
            enabled,
        });
    }

    fn toggle_selected_model(&mut self, enabled: bool) {
        let Some(model) = self.selected_model() else {
            return;
        };
        let mut body = model.raw.clone();
        body["enabled"] = json!(enabled);
        self.start_command(TuiCommand::SaveModel { body });
    }

    fn request_reset_confirmation(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    ) -> Result<(), String> {
        let account = self.selected_account().ok_or("Select an account")?;
        if !account.enabled
            || account.file_name.is_empty()
            || !["codex", "claude"].contains(&account.provider.as_str())
        {
            return Err("Select an enabled Codex or Claude account".into());
        }
        let values = account_snapshot_values(&self.data.snapshot, &account);
        if values.iter().any(|v| {
            matches!(
                v.pointer("/last_reset_action/state")
                    .and_then(Value::as_str),
                Some("submitted" | "manual_review")
            )
        }) {
            return Err("Review the previous Claude reset outcome before another reset".into());
        }
        let credits = selectable_reset_credits(&values, &account.provider);
        if credits.is_empty() {
            return Err("No concrete usable reset reported; refresh the account".into());
        }
        let notice = credits
            .iter()
            .enumerate()
            .map(|(i, c)| {
                format!(
                    "{}: {} · {}",
                    i + 1,
                    string_at(c, &["title"]),
                    string_at(c, &["description"])
                )
                .chars()
                .filter(|c| !c.is_control())
                .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        let choice =
            prompt_in_terminal_with_notice(terminal, &notice, "Reset number (blank = cancel): ")?;
        if choice.trim().is_empty() {
            return Ok(());
        }
        let selected = choice
            .trim()
            .parse::<usize>()
            .ok()
            .and_then(|n| n.checked_sub(1))
            .and_then(|n| credits.get(n))
            .ok_or("Choose a listed reset number")?;
        self.show_account_details = false;
        self.confirmation_modal = Some(ConfirmationModal {
            title: "Use limit reset".into(),
            message: format!("Use {} for {}? This spends a reset allowance. The gateway will check fresh eligibility first.", string_at(selected,&["title"]), account_display(&account)),
            action: ConfirmationAction::ConsumeReset { provider: account.provider, file_name: account.file_name,
                credit_id:string_at(selected,&["id"]), program:string_at(selected,&["program"]) },
        });
        Ok(())
    }

    fn edit_claude_reset_policy(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    ) -> Result<(), String> {
        let account = self.selected_account().ok_or("Select a Claude account")?;
        if account.provider != "claude" {
            return Err("Scheduling shortcut applies to Claude accounts".into());
        }
        let values = account_snapshot_values(&self.data.snapshot, &account);
        let mut body = values
            .iter()
            .find_map(|v| v.get("reset_automation"))
            .cloned()
            .ok_or("Refresh account data to load Claude scheduling")?;
        let enabled = prompt_in_terminal_with_notice(terminal, "Automatic resets can spend expiring reset allowances when a limit is reached. Blank cancels.", "Enable automatic resets? (yes/no, blank = cancel): ")?;
        if enabled.trim().is_empty() {
            return Ok(());
        }
        body["enabled"] = match enabled.trim().to_ascii_lowercase().as_str() {
            "yes" | "y" => json!(true),
            "no" | "n" => json!(false),
            _ => return Err("Enter yes or no".into()),
        };
        for (key, label) in [
            ("scan_interval_minutes", "Scan interval"),
            ("expiry_window_minutes", "Upper expiry window"),
            ("final_attempt_minutes", "Final check before expiry"),
            (
                "min_natural_reset_remaining_minutes",
                "Avoid natural reset within",
            ),
        ] {
            let current = body[key].as_i64().unwrap_or(0);
            let input = prompt_in_terminal(terminal, &format!("{label} minutes [{current}]: "))?;
            if !input.trim().is_empty() {
                body[key] = json!(input
                    .trim()
                    .parse::<i64>()
                    .map_err(|_| "Enter whole minutes")?);
            }
        }
        self.show_account_details = false;
        self.confirmation_modal=Some(ConfirmationModal{title:"Save Claude automatic resets".into(),message:format!("Save {} scheduling for {}? Enabled scheduling may spend expiring reset allowances.",if body["enabled"]==true{"enabled"}else{"disabled"},account_display(&account)),action:ConfirmationAction::SaveClaudeResetPolicy{body}});
        Ok(())
    }

    fn request_delete_confirmation(&mut self) {
        if self.tab == Tab::Keys {
            let Some(key) = self.selected_api_key() else {
                return;
            };
            if key.revoked {
                self.message = "API key is already revoked".to_string();
                return;
            }
            self.confirmation_modal = Some(ConfirmationModal {
                title: "Revoke API Key".to_string(),
                message: format!("Revoke API key {}?", display_or(&key.label, "API key")),
                action: ConfirmationAction::RevokeApiKey { id: key.id.clone() },
            });
        } else if self.tab == Tab::Overview
            && self.overview_section == OverviewSection::AccountUsage
        {
            let Some(account) = self.selected_account() else {
                return;
            };
            if account.file_name.is_empty() {
                self.message = "selected account has no credential file".to_string();
                return;
            }
            self.confirmation_modal = Some(ConfirmationModal {
                title: "Delete Account".to_string(),
                message: format!("Delete account credential {}?", account_display(&account)),
                action: ConfirmationAction::DeleteAccount {
                    file_name: account.file_name,
                },
            });
        } else if self.tab == Tab::Overview
            && self.overview_section == OverviewSection::CustomModels
        {
            let Some(model) = self.selected_model() else {
                return;
            };
            self.confirmation_modal = Some(ConfirmationModal {
                title: "Delete Custom Model".to_string(),
                message: format!("Delete custom model {}?", model.alias),
                action: ConfirmationAction::DeleteCustomModel { alias: model.alias },
            });
        }
    }

    fn cancel_confirmation(&mut self) {
        self.confirmation_modal = None;
        self.message = "action cancelled".to_string();
    }

    fn confirm_selection(&mut self) {
        let Some(modal) = self.confirmation_modal.take() else {
            return;
        };
        match modal.action {
            ConfirmationAction::SaveClaudeResetPolicy { body } => {
                self.start_command(TuiCommand::SaveClaudeResetPolicy { body })
            }
            ConfirmationAction::ConsumeReset {
                provider,
                file_name,
                credit_id,
                program,
            } => self.start_command(TuiCommand::ConsumeReset {
                provider,
                file_name,
                credit_id,
                program,
            }),
            ConfirmationAction::DeleteAccount { file_name } => {
                self.start_command(TuiCommand::DeleteAccount { file_name })
            }
            ConfirmationAction::DeleteCustomModel { alias } => {
                self.start_command(TuiCommand::DeleteModel { alias })
            }
            ConfirmationAction::RevokeApiKey { id } => {
                self.start_command(TuiCommand::RevokeApiKey { id })
            }
        }
    }
}

async fn fetch_gateway_data(client: &GatewayClient) -> Result<GatewayData, String> {
    let session_response = client.get("/admin/session").await?;
    let session = session_response.body;
    let mut data = GatewayData {
        session: session.clone(),
        ..GatewayData::default()
    };
    if bool_at(&session, &["enabled"]) && !bool_at(&session, &["authenticated"]) {
        return Ok(data);
    }

    let summary = client.get("/usage/summary.json").await?;
    ensure_success(&summary)?;
    let routing = client.get("/admin/account-routing").await?;
    ensure_success(&routing)?;
    let snapshot = client.get("/dashboard/snapshot.json").await?;
    ensure_success(&snapshot)?;
    let keys = client.get("/admin/api-keys").await?;
    ensure_success(&keys)?;
    let models = client.get("/custom-models.json").await?;
    ensure_success(&models)?;
    let notifications = client.get("/notifications/settings").await?;
    ensure_success(&notifications)?;

    data.summary = summary.body;
    data.routing = routing.body;
    data.snapshot = snapshot.body;
    data.accounts = account_rows(&data.summary, &data.routing);
    data.quotas = quota_rows_from_snapshot(&data.snapshot);
    apply_account_runtime_status(&mut data.accounts, &data.snapshot);
    apply_account_attention(&mut data.accounts, &data.snapshot, &data.quotas);
    data.key_quota_summaries = keys
        .body
        .get("quota_summaries")
        .cloned()
        .unwrap_or(Value::Null);
    data.keys = key_rows_from_response(&keys.body);
    data.models = model_rows_from_response(&models.body);
    data.notifications = notifications.body;

    Ok(data)
}

fn btop_style() -> Style {
    Style::default().fg(BTOP_MAIN_FG).bg(BTOP_MAIN_BG)
}

fn btop_fg(color: Color) -> Style {
    btop_style().fg(color)
}

fn btop_header_style() -> Style {
    btop_fg(BTOP_HI_FG).add_modifier(Modifier::BOLD)
}

fn btop_title_style() -> Style {
    btop_fg(BTOP_TITLE).add_modifier(Modifier::BOLD)
}

fn btop_muted_style() -> Style {
    btop_fg(BTOP_GRAPH_TEXT)
}

fn btop_selected_style() -> Style {
    Style::default().fg(BTOP_SELECTED_FG).bg(BTOP_SELECTED_BG)
}

fn selected_or(style: Style, selected: bool) -> Style {
    if selected {
        btop_selected_style()
    } else {
        style
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AccountStatus {
    Enabled,
    Disabled,
    Cooldown,
}

fn account_status(enabled: bool, cooldown: bool) -> AccountStatus {
    if !enabled {
        AccountStatus::Disabled
    } else if cooldown {
        AccountStatus::Cooldown
    } else {
        AccountStatus::Enabled
    }
}

fn account_status_label(status: AccountStatus) -> &'static str {
    match status {
        AccountStatus::Enabled => "Enabled",
        AccountStatus::Disabled => "Disabled",
        AccountStatus::Cooldown => "Cooldown",
    }
}

fn account_status_circle(status: AccountStatus, selected: bool) -> Span<'static> {
    let color = match status {
        AccountStatus::Enabled => BTOP_CPU_START,
        AccountStatus::Disabled => BTOP_STATUS_DISABLED,
        AccountStatus::Cooldown => BTOP_STATUS_COOLDOWN,
    };
    let style = if selected {
        btop_selected_style().fg(color)
    } else {
        btop_fg(color)
    };
    Span::styled("●", style)
}

fn account_has_attention_marker(account: &AccountRow) -> bool {
    account.enabled && !account.cooldown && !account.attention_reasons.is_empty()
}

fn account_status_circle_for_usage(
    account: &OverviewAccountUsageRow,
    selected: bool,
) -> Span<'static> {
    account_status_circle(account_status(account.enabled, account.cooldown), selected)
}

fn attention_marker(attention: bool, selected: bool) -> Option<Span<'static>> {
    attention.then(|| {
        let style = if selected {
            btop_selected_style().fg(BTOP_ATTENTION)
        } else {
            btop_fg(BTOP_ATTENTION).add_modifier(Modifier::BOLD)
        };
        Span::styled("!", style)
    })
}

fn attention_marker_for_usage(
    account: &OverviewAccountUsageRow,
    selected: bool,
) -> Option<Span<'static>> {
    attention_marker(account.attention, selected)
}

fn moved_index(current: usize, len: usize, delta: isize) -> usize {
    if len == 0 {
        return 0;
    }
    (current as isize + delta).clamp(0, len as isize - 1) as usize
}

fn account_reorder_delta(key: KeyEvent) -> Option<isize> {
    let control = key.modifiers.contains(KeyModifiers::CONTROL);
    if !control {
        return match key.code {
            // These are the raw control bytes some terminal adapters expose
            // before Crossterm decorates them with KeyModifiers::CONTROL.
            KeyCode::Char('\u{0b}') => Some(-1),
            KeyCode::Char('\u{0a}') => Some(1),
            _ => None,
        };
    }

    match key.code {
        KeyCode::Char('k' | 'K' | '\u{0b}') => Some(-1),
        KeyCode::Char('j' | 'J' | '\u{0a}') => Some(1),
        // A few terminal/multiplexer combinations normalize Ctrl+J to Enter.
        KeyCode::Enter => Some(1),
        _ => None,
    }
}

fn provider_reorder_delta(key: KeyEvent) -> Option<isize> {
    if !key.modifiers.contains(KeyModifiers::CONTROL) {
        return None;
    }
    match key.code {
        KeyCode::Up => Some(-1),
        KeyCode::Down => Some(1),
        _ => None,
    }
}

fn clamp_index(current: usize, len: usize) -> usize {
    if len == 0 {
        0
    } else {
        current.min(len - 1)
    }
}

fn btop_block_with_border<'a>(title: impl Into<Line<'a>>, border: Color) -> Block<'a> {
    Block::default()
        .title(title)
        .title_style(btop_title_style())
        .borders(Borders::ALL)
        .border_style(btop_fg(border))
        .style(btop_style())
}

fn btop_plain_block() -> Block<'static> {
    Block::default().style(btop_style())
}

fn draw_tui(frame: &mut Frame<'_>, app: &TuiApp) {
    if app.animation.active && app.tab == Tab::Overview {
        app.animation.draw(
            frame,
            app.network
                .connection_label(Tab::Overview, &app.data.session),
        );
        draw_animated_overview_context(frame, app);
        draw_tui_modals(frame, app);
        return;
    }
    frame.render_widget(btop_plain_block(), frame.area());

    let root = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Min(5),
            Constraint::Length(2),
        ])
        .split(frame.area());

    let title = format!(
        " iogw  {}  {}",
        app.client.base_url,
        app.network.connection_label(app.tab, &app.data.session)
    );
    frame.render_widget(
        Paragraph::new(title)
            .style(btop_title_style())
            .block(btop_block_with_border("", BTOP_PROC_BOX)),
        root[0],
    );

    let titles = Tab::all()
        .iter()
        .map(|tab| Line::from(Span::raw(tab.title())))
        .collect::<Vec<_>>();
    let selected = Tab::all()
        .iter()
        .position(|tab| *tab == app.tab)
        .unwrap_or(0);
    frame.render_widget(
        Tabs::new(titles)
            .select(selected)
            .style(btop_style())
            .highlight_style(btop_selected_style().add_modifier(Modifier::BOLD))
            .block(btop_block_with_border("", BTOP_DIV_LINE)),
        root[1],
    );

    match app.tab {
        Tab::Overview => draw_overview(frame, root[2], app),
        Tab::Keys => draw_api_keys(frame, root[2], app),
        Tab::Notifications => draw_notifications(frame, root[2], app),
    }

    let mut footer = if app.message.trim().is_empty() {
        if app.tab == Tab::Notifications {
            " ? commands | tab switch | j/k move | Enter/Space watch | e enable | c channel | s settings | a/u all/none | p provider | t test "
                .to_string()
        } else if app.tab == Tab::Keys {
            " ? commands | tab switch | j/k move | r refresh | n new key | Enter details | e edit | x revoke "
                .to_string()
        } else {
            let section_hint = match app.overview_section {
                OverviewSection::AccountUsage => format!(
                    "a quota: {} | n add account | i edit API | e/d enable/disable | x delete",
                    app.limit_display_mode.label()
                ),
                OverviewSection::CustomModels => "e/d enable/disable | x delete".to_string(),
                _ => "".to_string(),
            };
            format!(
                " ? commands | tab switch | h/l section | j/k move | r account | R all | Ctrl+R hard | o login | {} ",
                section_hint
            )
        }
    } else {
        format!(" {}", app.message)
    };
    if app.message.trim().is_empty() && animated::any_installed() {
        footer = format!(" F4 animated |{footer}");
    }
    let status = app.network.status(app.tab);
    frame.render_widget(
        Paragraph::new(vec![Line::from(footer), Line::from(status)]).style(btop_muted_style()),
        root[3],
    );

    draw_tui_modals(frame, app);
}

fn draw_animated_overview_context(frame: &mut Frame<'_>, app: &TuiApp) {
    let area = frame.area();
    let footer = animated::footer_area(area);
    if footer.height >= 3 {
        let status = format!(
            "{} · {} · quota {} · {} hidden{}{}",
            app.overview_section.title(),
            app.animation.selected_identity(),
            app.limit_display_mode.label(),
            app.usage_filter_count(),
            if app.message.is_empty() { "" } else { " · " },
            app.message
        );
        frame.render_widget(
            Clear,
            Rect::new(footer.x, footer.y, footer.width, footer.height - 1),
        );
        frame.render_widget(
            Paragraph::new(status).style(btop_title_style()),
            Rect::new(footer.x, footer.y, footer.width, 1),
        );
        let lines = focused_account_footer(app, footer.width);
        frame.render_widget(
            Paragraph::new(lines.into_iter().map(Line::from).collect::<Vec<_>>())
                .style(btop_style()),
            Rect::new(footer.x, footer.y + 1, footer.width, footer.height - 2),
        );
    }
    // Gateway statistics live on the wall TV; custom models live with their cat.
    // Keep the normal Overview's panels and action routing without floating them
    // over the artwork in animated mode.
}

/// Pack prioritized fields into the terminal width. Never dump credential JSON
/// or account IDs; the modal remains available for longer details.
fn focused_account_footer(app: &TuiApp, width: u16) -> Vec<String> {
    if width == 0 {
        return vec![];
    }
    let mut fields = Vec::new();
    if let Some(notice) = app.animation.notice() {
        fields.push(format!("! {notice}"));
    }
    if app.animation.models_focused() {
        fields.push("j/k models · e/d enable/disable · x delete · Enter details".into());
        fields.extend(app.data.models.iter().map(|model| {
            format!(
                "{} {}",
                if model.alias.is_empty() {
                    &model.id
                } else {
                    &model.alias
                },
                if model.enabled { "on" } else { "off" }
            )
        }));
    } else if let Some(account) = app.selected_account() {
        fields.push(format!(
            "{} · {} req · {} tokens · {} errors ({}){}",
            account.provider_label,
            format_short(account.requests),
            format_short(account.total_tokens),
            format_count(account.errors),
            error_rate(account.requests, account.errors),
            if account.priority { " · priority" } else { "" }
        ));
        let status = account_status(account.enabled, account.cooldown);
        fields.push(format!(
            "Status {}{}",
            account_status_label(status),
            account
                .cooldown_remaining_seconds
                .filter(|_| account.cooldown)
                .map(|s| format!(" {}", format_duration(s)))
                .unwrap_or_default()
        ));
        if !account.attention_reasons.is_empty() {
            fields.push(format!(
                "Attention {}",
                account.attention_reasons.join(" / ")
            ));
        }
        fields.extend(app.animation.focused_limits(width));
        let values = account_snapshot_values(&app.data.snapshot, &account);
        if ["codex", "claude"].contains(&account.provider.as_str()) {
            if let Some(value) = values.iter().find(|v| v.get("credits").is_some()) {
                fields.extend(
                    provider_quota_summary::credit_fields(value)
                        .into_iter()
                        .map(|f| format!("{} {}", f.label, f.value)),
                );
            }
        }
        if let Some(plan) = values
            .iter()
            .find_map(|value| first_string_recursive(value, &["plan_type", "account_type"]))
        {
            fields.push(format!("Plan {plan}"));
        }
        if let Some((count, _)) = reset_credit_details(&values) {
            fields.push(format!("Reset credits {count}"));
        }
        let models = snapshot_model_names(&values);
        fields.push(format!(
            "Snapshot {}",
            display_or(&string_at(&app.data.snapshot, &["generated_at"]), "pending")
        ));
        if !account.last_success_at.is_empty() {
            fields.push(format!("Last OK {}", account.last_success_at));
        }
        if !account.last_error_at.is_empty() {
            fields.push(format!("Last error {}", account.last_error_at));
        }
        fields.extend(models.into_iter().map(|model| format!("Model {model}")));
        if !account.file_name.is_empty() {
            fields.push(format!("Credential {}", account.file_name));
        }
    } else {
        fields.push("No focused account · R refresh · Enter details".into());
    }
    pack_footer_fields(&fields, width)
}

fn pack_footer_fields(fields: &[String], width: u16) -> Vec<String> {
    let mut lines = Vec::<String>::new();
    for field in fields {
        let field = animated::clipped(
            &field.split_whitespace().collect::<Vec<_>>().join(" "),
            width,
        );
        if field.is_empty() {
            continue;
        }
        if let Some(line) = lines.last_mut() {
            if Line::from(format!("{line} · {field}")).width() <= width as usize {
                line.push_str(" · ");
                line.push_str(&field);
                continue;
            }
        }
        lines.push(field);
    }
    lines
}

fn draw_tui_modals(frame: &mut Frame<'_>, app: &TuiApp) {
    if app.show_command_modal {
        draw_command_modal(frame, frame.area(), app);
    }
    if app.show_account_details {
        draw_account_details_modal(frame, frame.area(), app);
    }
    if app.show_api_key_details {
        draw_api_key_details_modal(frame, frame.area(), app);
    }
    if let Some(modal) = &app.confirmation_modal {
        draw_confirmation_modal(frame, frame.area(), modal);
    }
    if app.show_api_key_reveal {
        draw_api_key_reveal_modal(frame, frame.area(), app);
    }
}

fn draw_overview(frame: &mut Frame<'_>, area: Rect, app: &TuiApp) {
    if app.needs_login() {
        frame.render_widget(
            Paragraph::new(vec![
                Line::from("Admin login required."),
                Line::from("Press o to enter your TOTP code, or run iogw login from your shell."),
                Line::from(format!("Gateway: {}", app.client.base_url)),
            ])
            .style(btop_style())
            .block(btop_block_with_border("Overview", BTOP_CPU_BOX))
            .wrap(Wrap { trim: false }),
            area,
        );
        return;
    }

    if area.width < 72 {
        draw_stacked_overview(frame, area, app);
        return;
    }

    let root = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(5),
            Constraint::Length(8),
            Constraint::Min(7),
        ])
        .split(area);

    draw_overview_metrics(frame, root[0], app);
    draw_context_usage_chart(frame, root[1], app);

    let lower = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(54), Constraint::Percentage(46)])
        .split(root[2]);
    draw_custom_models_overview(frame, lower[0], app);
    draw_account_usage_percentages(frame, lower[1], app);
}

fn draw_stacked_overview(frame: &mut Frame<'_>, area: Rect, app: &TuiApp) {
    let root = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(10),
            Constraint::Length(8),
            Constraint::Length(7),
            Constraint::Min(6),
        ])
        .split(area);

    draw_overview_metrics(frame, root[0], app);
    draw_context_usage_chart(frame, root[1], app);
    draw_custom_models_overview(frame, root[2], app);
    draw_account_usage_percentages(frame, root[3], app);
}

fn draw_overview_metrics(frame: &mut Frame<'_>, area: Rect, app: &TuiApp) {
    let totals = app.data.summary.get("totals").unwrap_or(&Value::Null);
    let requests = number_at(totals, &["requests"]);
    let errors = number_at(totals, &["errors"]);
    let total_tokens = number_at(totals, &["total_tokens"]);
    let input_tokens = number_at(totals, &["input_tokens"]);
    let output_tokens = number_at(totals, &["output_tokens"]);
    let tracked = app.data.accounts.len() as u64;
    let active = active_account_count(&app.data.accounts) as u64;
    let enabled = app
        .data
        .accounts
        .iter()
        .filter(|account| account.enabled)
        .count() as u64;

    let metrics = [
        (
            "Requests",
            format_count(requests),
            format!("{} errors", format_count(errors)),
            BTOP_TEMP_START,
        ),
        (
            "Tokens",
            format_count(total_tokens),
            format!(
                "{} in / {} out",
                format_short(input_tokens),
                format_short(output_tokens)
            ),
            BTOP_AVAILABLE_END,
        ),
        (
            "Accounts",
            format_count(active),
            format!(
                "{} tracked / {} enabled",
                format_count(tracked),
                format_count(enabled)
            ),
            BTOP_CPU_START,
        ),
        (
            "Error Rate",
            error_rate(requests, errors),
            app.network
                .age(Section::Summary)
                .map(|age| format!("usage fetched {}s ago", age.as_secs()))
                .unwrap_or_else(|| "waiting for data".to_string()),
            if errors > 0 {
                BTOP_USED_END
            } else {
                BTOP_CPU_START
            },
        ),
    ];

    if area.width < 72 {
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(area);
        let top = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(rows[0]);
        let bottom = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(rows[1]);
        let cells = [top[0], top[1], bottom[0], bottom[1]];

        for (index, (label, value, detail, color)) in metrics.iter().enumerate() {
            let label =
                overview_metric_label(label, app.overview_section == OverviewSection::Metrics);
            draw_metric(frame, cells[index], &label, value, detail, *color);
        }
        return;
    }

    let cells = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(25),
            Constraint::Percentage(25),
            Constraint::Percentage(25),
            Constraint::Percentage(25),
        ])
        .split(area);

    for (index, (label, value, detail, color)) in metrics.iter().enumerate() {
        let label = overview_metric_label(label, app.overview_section == OverviewSection::Metrics);
        draw_metric(frame, cells[index], &label, value, detail, *color);
    }
}

fn overview_metric_label(label: &str, focused: bool) -> String {
    if focused {
        format!("> {label}")
    } else {
        label.to_string()
    }
}

fn draw_metric(
    frame: &mut Frame<'_>,
    area: Rect,
    label: &str,
    value: &str,
    detail: &str,
    color: Color,
) {
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(
                label,
                btop_fg(color).add_modifier(Modifier::BOLD),
            )),
            Line::from(Span::styled(value, btop_title_style())),
            Line::from(Span::styled(detail, btop_muted_style())),
        ])
        .style(btop_style())
        .block(btop_block_with_border("", color)),
        area,
    );
}

fn draw_context_usage_chart(frame: &mut Frame<'_>, area: Rect, app: &TuiApp) {
    let data = app
        .data
        .usage_buckets
        .iter()
        .map(|bucket| bucket.total_tokens)
        .collect::<Vec<_>>();
    let total = data.iter().sum::<u64>();
    let peak = data.iter().copied().max().unwrap_or(0);
    let requests = app
        .data
        .usage_buckets
        .iter()
        .map(|bucket| bucket.request_count)
        .sum::<u64>();
    let input = app
        .data
        .usage_buckets
        .iter()
        .map(|bucket| bucket.input_tokens)
        .sum::<u64>();
    let output = app
        .data
        .usage_buckets
        .iter()
        .map(|bucket| bucket.output_tokens)
        .sum::<u64>();
    let cache = app
        .data
        .usage_buckets
        .iter()
        .map(|bucket| bucket.cache_tokens)
        .sum::<u64>();
    let reasoning = app
        .data
        .usage_buckets
        .iter()
        .map(|bucket| bucket.reasoning_tokens)
        .sum::<u64>();
    let title = format!(
        "Context Usage 24h  total {}  peak {}  req {}",
        format_short(total),
        format_short(peak),
        format_short(requests)
    );
    let title = overview_block_title(
        &title,
        app.overview_section == OverviewSection::ContextUsage,
    );

    if data.iter().all(|value| *value == 0) {
        let lines = vec![
            Line::from("No usage buckets for the last 24h."),
            Line::from(format!(
                "Input {} / output {} / cache {} / reasoning {}",
                format_short(input),
                format_short(output),
                format_short(cache),
                format_short(reasoning)
            )),
        ];
        frame.render_widget(
            Paragraph::new(lines)
                .style(btop_style())
                .block(btop_block_with_border(Line::from(title), BTOP_CPU_BOX))
                .wrap(Wrap { trim: false }),
            area,
        );
        return;
    }

    let footer = overview_chart_footer(&app.data.usage_buckets);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(4), Constraint::Length(1)])
        .split(area);
    frame.render_widget(
        Sparkline::default()
            .block(btop_block_with_border(Line::from(title), BTOP_CPU_BOX))
            .style(btop_fg(BTOP_CACHED_MID))
            .data(data)
            .max(peak.max(1)),
        chunks[0],
    );
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(footer, btop_muted_style()),
            Span::raw(format!(
                "  in {} / out {} / cache {} / reasoning {}",
                format_short(input),
                format_short(output),
                format_short(cache),
                format_short(reasoning)
            )),
        ])),
        chunks[1],
    );
}

fn draw_custom_models_overview(frame: &mut Frame<'_>, area: Rect, app: &TuiApp) {
    if area.width < 58 {
        draw_custom_models_overview_narrow(frame, area, app);
        return;
    }

    let max_rows = area.height.saturating_sub(3).max(1) as usize;
    let start = app
        .overview_model_selected
        .saturating_add(1)
        .saturating_sub(max_rows);
    let rows = app
        .data
        .models
        .iter()
        .enumerate()
        .skip(start)
        .take(max_rows)
        .map(|(index, model)| {
            let selected = app.tab == Tab::Overview
                && app.overview_section == OverviewSection::CustomModels
                && index == app.overview_model_selected;
            Row::new(vec![
                Cell::from(if selected { ">" } else { "" }),
                Cell::from(if model.enabled { "on" } else { "off" }),
                Cell::from(model.alias.clone()),
                Cell::from(model.display_name.clone()),
                Cell::from(model.routes.to_string()),
                Cell::from(model.targets.to_string()),
                Cell::from(model.id.clone()),
            ])
            .style(if selected {
                btop_selected_style()
            } else if model.enabled {
                btop_style()
            } else {
                btop_muted_style()
            })
        });
    let header = Row::new(["", "State", "Alias", "Name", "Routes", "Targets", "Id"])
        .style(btop_header_style());
    let title = overview_block_title(
        "Custom Models",
        app.overview_section == OverviewSection::CustomModels,
    );
    frame.render_widget(
        Table::new(
            rows,
            [
                Constraint::Length(1),
                Constraint::Length(5),
                Constraint::Length(14),
                Constraint::Min(14),
                Constraint::Length(7),
                Constraint::Length(8),
                Constraint::Min(12),
            ],
        )
        .header(header)
        .style(btop_style())
        .block(btop_block_with_border(title, BTOP_PROC_BOX))
        .column_spacing(1),
        area,
    );
}

fn draw_custom_models_overview_narrow(frame: &mut Frame<'_>, area: Rect, app: &TuiApp) {
    let content_width = area.width.saturating_sub(2) as usize;
    let max_rows = area.height.saturating_sub(2).max(1) as usize;

    let lines = if app.data.models.is_empty() {
        vec![Line::from("No custom models configured.")]
    } else {
        app.data
            .models
            .iter()
            .enumerate()
            .skip(
                app.overview_model_selected
                    .saturating_add(1)
                    .saturating_sub(max_rows),
            )
            .take(max_rows)
            .map(|(index, model)| {
                let selected = app.tab == Tab::Overview
                    && app.overview_section == OverviewSection::CustomModels
                    && index == app.overview_model_selected;
                let marker = if selected { ">" } else { " " };
                let state = if model.enabled { "on" } else { "off" };
                let counts = format!("{}r {}t", model.routes, model.targets);
                let marker_width = 2;
                let state_width = 4;
                let count_width = counts.len().max(5);
                let alias_width = content_width
                    .saturating_sub(marker_width + state_width + count_width + 2)
                    .max(6);
                let alias = truncate_text(&model.alias, alias_width);
                Line::from(vec![
                    Span::styled(
                        format!("{marker:<marker_width$}"),
                        selected_or(btop_style(), selected),
                    ),
                    Span::styled(
                        format!("{state:<state_width$}"),
                        selected_or(
                            if model.enabled {
                                btop_fg(BTOP_CPU_START)
                            } else {
                                btop_muted_style()
                            },
                            selected,
                        ),
                    ),
                    Span::styled(
                        format!("{alias:<alias_width$} "),
                        selected_or(btop_style(), selected),
                    ),
                    Span::styled(
                        format!("{counts:>count_width$}"),
                        selected_or(btop_muted_style(), selected),
                    ),
                ])
            })
            .collect()
    };

    let title = overview_block_title(
        "Custom Models",
        app.overview_section == OverviewSection::CustomModels,
    );
    frame.render_widget(
        Paragraph::new(lines)
            .style(btop_style())
            .block(btop_block_with_border(title, BTOP_PROC_BOX)),
        area,
    );
}

fn overview_block_title(title: &str, focused: bool) -> String {
    if focused {
        format!("> {title}")
    } else {
        title.to_string()
    }
}

fn draw_account_usage_percentages(frame: &mut Frame<'_>, area: Rect, app: &TuiApp) {
    let groups = overview_account_usage_groups(app);
    let selected_account_index = (app.tab == Tab::Overview
        && app.overview_section == OverviewSection::AccountUsage)
        .then(|| selected_overview_account_index(app))
        .flatten();
    let content_width = area.width.saturating_sub(2) as usize;
    let max_rows = area.height.saturating_sub(2).max(1) as usize;

    let mut all_lines = Vec::new();
    let mut selected_line_index = None;
    for group in &groups {
        all_lines.push(account_usage_provider_header(group, app.limit_display_mode));
        let mut last_account_index = None;

        for account in &group.accounts {
            let account_selected = selected_account_index == Some(account.account_index);
            let first_account_row = last_account_index != Some(account.account_index);
            if app.limit_display_mode == LimitDisplayMode::AllPerAccount && first_account_row {
                if account_selected {
                    selected_line_index = Some(all_lines.len());
                }
                all_lines.push(account_usage_account_header_line(
                    account,
                    content_width,
                    account_selected,
                ));
                last_account_index = Some(account.account_index);
            }
            let selected =
                app.limit_display_mode == LimitDisplayMode::OnePerAccount && account_selected;
            if selected && first_account_row {
                selected_line_index = Some(all_lines.len());
            }
            all_lines.push(account_quota_usage_line(
                account,
                content_width,
                selected,
                app.limit_display_mode,
            ));
        }
    }

    let start = selected_line_index
        .map(|index| index.saturating_add(1).saturating_sub(max_rows))
        .unwrap_or(0);
    let mut lines = all_lines
        .into_iter()
        .skip(start)
        .take(max_rows)
        .collect::<Vec<_>>();

    if lines.is_empty() {
        lines.push(Line::from(if app.usage_filter_count() > 0 {
            "No visible account usage rows. Press u to show all."
        } else {
            "No account quota usage recorded yet."
        }));
    }

    frame.render_widget(
        Paragraph::new(lines)
            .style(btop_style())
            .block(btop_block_with_border(
                account_usage_title(app),
                BTOP_MEM_BOX,
            )),
        area,
    );
}

fn account_usage_title(app: &TuiApp) -> String {
    let hidden = app.usage_filter_count();
    let title = if hidden == 0 {
        format!("Account Usage % ({})", app.limit_display_mode.label())
    } else {
        format!(
            "Account Usage % ({}, {} hidden)",
            app.limit_display_mode.label(),
            hidden
        )
    };
    overview_block_title(
        &title,
        app.overview_section == OverviewSection::AccountUsage,
    )
}

fn account_usage_provider_header(
    group: &OverviewAccountUsageGroup,
    mode: LimitDisplayMode,
) -> Line<'static> {
    let suffix = match mode {
        LimitDisplayMode::OnePerAccount => {
            if group.accounts.len() == 1 {
                "1 account".to_string()
            } else {
                format!("{} accounts", group.accounts.len())
            }
        }
        LimitDisplayMode::AllPerAccount => {
            if group.accounts.len() == 1 {
                "1 limit".to_string()
            } else {
                format!("{} limits", group.accounts.len())
            }
        }
    };
    Line::from(vec![
        Span::styled(provider_label(&group.provider), btop_header_style()),
        Span::styled(format!("  {suffix}"), btop_muted_style()),
    ])
}

fn account_quota_usage_line(
    account: &OverviewAccountUsageRow,
    content_width: usize,
    selected: bool,
    mode: LimitDisplayMode,
) -> Line<'static> {
    if mode == LimitDisplayMode::AllPerAccount {
        return account_quota_usage_detail_line(account, content_width, selected);
    }

    let percent_text = account
        .percent
        .map(|percent| format!("{percent:.1}%"))
        .unwrap_or_else(|| "No quota".to_string());
    let priority = if account.priority { "first" } else { "" };
    let selection = if selected { ">" } else { " " };

    if content_width < 38 {
        let style = if selected {
            btop_selected_style()
        } else {
            btop_style()
        };
        let prefix_width = 4 + if account.attention { 2 } else { 0 };
        let compact = format!("{priority} {} {}", account.label, percent_text);
        let mut spans = vec![
            Span::styled(format!("{selection} "), style),
            account_status_circle_for_usage(account, selected),
            Span::styled(" ", style),
        ];
        if let Some(marker) = attention_marker_for_usage(account, selected) {
            spans.push(marker);
            spans.push(Span::styled(" ", style));
        }
        spans.push(Span::styled(
            truncate_text(&compact, content_width.saturating_sub(prefix_width)),
            style,
        ));
        return Line::from(spans);
    }

    let marker_width = 2;
    let state_width = 2 + if account.attention { 2 } else { 0 };
    let priority_width = 6;
    let pct_width = if account.percent.is_some() { 7 } else { 8 };
    let request_width = if content_width < 64 { 0 } else { 8 };
    let bar_width = content_width
        .saturating_sub(marker_width + state_width + priority_width + pct_width + request_width + 6)
        .clamp(6, 18);
    let account_width = content_width
        .saturating_sub(
            marker_width + state_width + priority_width + pct_width + bar_width + request_width + 5,
        )
        .max(8);
    let account_name = truncate_text(&account.label, account_width);
    let percent = account.percent.unwrap_or(0.0);
    let bar_style = account
        .percent
        .map(quota_usage_color)
        .unwrap_or(BTOP_INACTIVE_FG);
    let percent_style = account
        .percent
        .map(quota_usage_color)
        .unwrap_or(BTOP_INACTIVE_FG);

    let mut spans = vec![
        Span::styled(
            format!("{selection:<marker_width$}"),
            selected_or(btop_style(), selected),
        ),
        account_status_circle_for_usage(account, selected),
        Span::styled(" ", selected_or(btop_style(), selected)),
    ];

    if let Some(marker) = attention_marker_for_usage(account, selected) {
        spans.push(marker);
        spans.push(Span::styled(" ", selected_or(btop_style(), selected)));
    }
    spans.extend([
        Span::styled(
            format!("{priority:<priority_width$}"),
            selected_or(btop_muted_style(), selected),
        ),
        Span::styled(
            format!("{account_name:<account_width$} "),
            selected_or(btop_style(), selected),
        ),
        Span::styled(
            format!("{percent_text:>pct_width$} "),
            selected_or(btop_fg(percent_style), selected),
        ),
        Span::styled(
            percent_bar(percent, bar_width),
            selected_or(btop_fg(bar_style), selected),
        ),
    ]);

    if request_width > 0 {
        spans.push(Span::styled(
            format!(
                " {:>request_width$}",
                format!("{} req", format_short(account.requests))
            ),
            selected_or(btop_muted_style(), selected),
        ));
    }

    Line::from(spans)
}

fn account_usage_account_header_line(
    account: &OverviewAccountUsageRow,
    content_width: usize,
    selected: bool,
) -> Line<'static> {
    let priority = if account.priority { " first" } else { "" };
    let requests = if content_width >= 64 {
        format!("  {} req", format_short(account.requests))
    } else {
        String::new()
    };
    let status_width = 2 + if account.attention { 2 } else { 0 };
    let label_width = content_width
        .saturating_sub(2 + status_width + priority.len() + requests.len())
        .max(8);
    let marker = if selected { "> " } else { "  " };
    let mut spans = vec![
        Span::styled(marker, selected_or(btop_style(), selected)),
        account_status_circle_for_usage(account, selected),
        Span::styled(" ", selected_or(btop_style(), selected)),
    ];
    if let Some(marker) = attention_marker_for_usage(account, selected) {
        spans.push(marker);
        spans.push(Span::styled(" ", selected_or(btop_style(), selected)));
    }
    spans.extend([
        Span::styled(priority, selected_or(btop_muted_style(), selected)),
        Span::styled(" ", selected_or(btop_style(), selected)),
        Span::styled(
            truncate_text(&account.account_label, label_width),
            selected_or(btop_title_style(), selected),
        ),
        Span::styled(requests, selected_or(btop_muted_style(), selected)),
    ]);
    Line::from(spans)
}

fn account_quota_usage_detail_line(
    account: &OverviewAccountUsageRow,
    content_width: usize,
    selected: bool,
) -> Line<'static> {
    let percent_text = account
        .detail_text
        .clone()
        .filter(|text| !text.trim().is_empty())
        .unwrap_or_else(|| {
            account
                .percent
                .map(|percent| format!("{percent:.1}% used"))
                .unwrap_or_else(|| "No quota".to_string())
        });
    let limit_name = if account.limit_label.trim().is_empty() {
        "usage".to_string()
    } else {
        account.limit_label.clone()
    };
    let reset_label = account_usage_reset_label(&account.provider, &account.reset);
    let reset = if reset_label.is_empty() {
        String::new()
    } else {
        format!("  {reset_label}")
    };
    let remaining = if account.provider == "minimax" || account.remaining_percent.trim().is_empty()
    {
        String::new()
    } else {
        format!("  remaining {}", account.remaining_percent)
    };
    let selection = if selected { ">" } else { " " };

    let style = if selected {
        btop_selected_style()
    } else {
        btop_muted_style()
    };

    if content_width < 48 {
        let text = format!("{selection}   {limit_name}  {percent_text}{reset}");
        return Line::from(Span::styled(truncate_text(&text, content_width), style));
    }

    let percent = account.percent.unwrap_or(0.0);
    let bar_width = content_width.saturating_sub(42).clamp(6, 16);
    let suffix = format!("{remaining}{reset}");
    let limit_width = content_width
        .saturating_sub(6 + percent_text.len() + bar_width + suffix.len())
        .max(8);

    Line::from(vec![
        Span::styled(
            format!("{selection}   "),
            selected_or(btop_style(), selected),
        ),
        Span::styled(
            format!("{:<limit_width$} ", truncate_text(&limit_name, limit_width)),
            style,
        ),
        Span::styled(
            format!("{percent_text:>10} "),
            selected_or(
                btop_fg(
                    account
                        .percent
                        .map(quota_usage_color)
                        .unwrap_or(BTOP_INACTIVE_FG),
                ),
                selected,
            ),
        ),
        Span::styled(
            percent_bar(percent, bar_width),
            selected_or(
                btop_fg(
                    account
                        .percent
                        .map(quota_usage_color)
                        .unwrap_or(BTOP_INACTIVE_FG),
                ),
                selected,
            ),
        ),
        Span::styled(suffix, style),
    ])
}

fn account_usage_reset_label(provider: &str, reset: &str) -> String {
    let reset = reset.trim();
    if reset.is_empty() {
        return String::new();
    }

    if provider == "codex" {
        return reset
            .strip_prefix("resets in ")
            .or_else(|| reset.strip_prefix("Resets in "))
            .unwrap_or(reset)
            .trim()
            .to_string();
    }

    if provider == "copilot" {
        return reset
            .strip_prefix("resets ")
            .or_else(|| reset.strip_prefix("Resets "))
            .or_else(|| reset.strip_prefix("resets"))
            .or_else(|| reset.strip_prefix("Resets"))
            .unwrap_or(reset)
            .trim()
            .to_string();
    }

    reset.to_string()
}

fn draw_api_keys(frame: &mut Frame<'_>, area: Rect, app: &TuiApp) {
    let content_width = area.width.saturating_sub(2) as usize;
    let max_rows = area.height.saturating_sub(2).max(1) as usize;
    let mut all_lines = Vec::new();
    let mut selected_line_index = None;

    for (index, key) in app.data.keys.iter().enumerate() {
        let selected = index == app.selected;
        if selected {
            selected_line_index = Some(all_lines.len());
        }
        all_lines.push(api_key_header_line(key, content_width, selected));
        all_lines.push(api_key_meta_line(key, content_width, selected));
        all_lines.push(api_key_access_line(key, content_width, selected));
        if let Some(summary) = app.data.key_quota_summaries.get(&key.id) {
            if let Some(text) = api_key_quota_summary_text(summary) {
                all_lines.push(api_key_meta_text_line(
                    &format!("Quota: {text}"),
                    content_width,
                    selected,
                ));
            }
        }
        if index + 1 < app.data.keys.len() {
            all_lines.push(Line::from(""));
        }
    }

    if all_lines.is_empty() {
        all_lines.push(Line::from("No API keys available."));
    }

    let start = selected_line_index
        .map(|index| index.saturating_add(1).saturating_sub(max_rows))
        .unwrap_or(0);
    let lines = all_lines
        .into_iter()
        .skip(start)
        .take(max_rows)
        .collect::<Vec<_>>();
    frame.render_widget(
        Paragraph::new(lines)
            .style(btop_style())
            .block(btop_block_with_border("API Keys", BTOP_NET_BOX)),
        area,
    );
}

fn api_key_header_line(key: &KeyRow, content_width: usize, selected: bool) -> Line<'static> {
    let marker = if selected { "> " } else { "  " };
    let status = if key.revoked { "revoked" } else { "active" };
    let prefix = if key.prefix.trim().is_empty() {
        key.id.as_str()
    } else {
        key.prefix.as_str()
    };
    let prefix = truncate_text(prefix, 24);
    let label_width = content_width
        .saturating_sub(2 + 2 + prefix.len() + status.len() + 4)
        .max(8);
    Line::from(vec![
        Span::styled(marker, selected_or(btop_style(), selected)),
        account_status_circle(
            if key.revoked {
                AccountStatus::Disabled
            } else {
                AccountStatus::Enabled
            },
            selected,
        ),
        Span::styled(" ", selected_or(btop_style(), selected)),
        Span::styled(
            format!(
                "{:<label_width$} ",
                truncate_text(&display_or(&key.label, "API key"), label_width)
            ),
            selected_or(btop_title_style(), selected),
        ),
        Span::styled(prefix, selected_or(btop_muted_style(), selected)),
        Span::styled("  ", selected_or(btop_style(), selected)),
        Span::styled(
            status,
            selected_or(
                if key.revoked {
                    btop_fg(BTOP_STATUS_DISABLED)
                } else {
                    btop_fg(BTOP_CPU_START)
                },
                selected,
            ),
        ),
    ])
}

fn api_key_meta_line(key: &KeyRow, content_width: usize, selected: bool) -> Line<'static> {
    api_key_meta_text_line(
        &format!(
            "  {} · created {} · last used {}{}",
            api_key_source_label(&key.source),
            display_or(&key.created_at, "unknown"),
            display_or(&key.last_used_at, "never"),
            if key.revoked_at.is_empty() {
                String::new()
            } else {
                format!(" · revoked {}", key.revoked_at)
            }
        ),
        content_width,
        selected,
    )
}

fn api_key_access_line(key: &KeyRow, content_width: usize, selected: bool) -> Line<'static> {
    api_key_meta_text_line(
        &format!("  Access: {}", key.access),
        content_width,
        selected,
    )
}

fn api_key_meta_text_line(text: &str, content_width: usize, selected: bool) -> Line<'static> {
    Line::from(Span::styled(
        truncate_text(text, content_width),
        selected_or(btop_muted_style(), selected),
    ))
}

fn api_key_source_label(source: &str) -> &str {
    match source {
        "legacy_config" => "legacy config",
        "managed" => "managed",
        value if !value.trim().is_empty() => value,
        _ => "unknown source",
    }
}

fn api_key_quota_summary_text(summary: &Value) -> Option<String> {
    if summary.get("error").and_then(Value::as_bool) == Some(true) {
        return Some("accounting unavailable; requests fail closed".to_string());
    }
    let rules = summary.get("rules").and_then(Value::as_array)?;
    if rules.is_empty() {
        return summary
            .get("anchor")
            .and_then(Value::as_str)
            .map(|anchor| format!("anchor {anchor}"));
    }
    Some(
        rules
            .iter()
            .filter_map(api_key_quota_rule_text)
            .collect::<Vec<_>>()
            .join(" · "),
    )
}

fn api_key_quota_detail_lines(summary: &Value) -> Vec<String> {
    if summary.get("error").and_then(Value::as_bool) == Some(true) {
        return vec!["Quota accounting unavailable; requests fail closed".to_string()];
    }
    let mut lines = Vec::new();
    if let Some(anchor) = summary.get("anchor").and_then(Value::as_str) {
        lines.push(format!("Quota anchor: {anchor}"));
    } else {
        lines.push("Quota timer starts on first accepted generation request".to_string());
    }
    if let Some(rules) = summary.get("rules").and_then(Value::as_array) {
        for rule in rules {
            if let Some(text) = api_key_quota_rule_text(rule) {
                lines.push(text);
            }
        }
    }
    lines
}

fn api_key_quota_rule_text(rule: &Value) -> Option<String> {
    let metric = rule.get("metric").and_then(Value::as_str)?;
    let period = rule
        .get("period")
        .and_then(Value::as_str)
        .unwrap_or("period");
    let confirmed = json_amount(rule.get("confirmed"));
    let reserved = json_amount(rule.get("reserved"));
    let uncertain = json_amount(rule.get("uncertain"));
    let remaining = json_amount(rule.get("remaining"));
    let limit = json_amount(rule.get("limit"));
    let reset = rule
        .get("reset_at")
        .and_then(Value::as_str)
        .map(|value| format!(" · renews {value}"))
        .unwrap_or_default();
    Some(format!(
        "{}/{}: {} confirmed · {} reserved · {} uncertain · {}/{} remaining{}",
        api_key_quota_metric_label(metric),
        period,
        confirmed,
        reserved,
        uncertain,
        remaining,
        limit,
        reset
    ))
}

fn api_key_quota_metric_label(metric: &str) -> &str {
    match metric {
        "requests" => "requests",
        "input_tokens" => "input tokens (incl. cache)",
        "uncached_input_tokens" => "uncached input tokens",
        "output_tokens" => "output tokens (incl. reasoning)",
        "cache_read_tokens" => "cache-read tokens",
        "cache_write_tokens" => "cache-write tokens",
        "cache_tokens" => "cache tokens (read + write)",
        value => value,
    }
}

fn json_amount(value: Option<&Value>) -> String {
    value
        .and_then(Value::as_u64)
        .map(|value| value.to_string())
        .or_else(|| value.and_then(Value::as_i64).map(|value| value.to_string()))
        .unwrap_or_else(|| "unknown".to_string())
}

fn draw_api_key_details_modal(frame: &mut Frame<'_>, area: Rect, app: &TuiApp) {
    let Some(key) = app.selected_api_key() else {
        return;
    };
    let status = if key.revoked {
        AccountStatus::Disabled
    } else {
        AccountStatus::Enabled
    };
    let mut lines = vec![Line::from(vec![
        account_status_circle(status, false),
        Span::raw(format!(
            "  {} ({})",
            display_or(&key.label, "API key"),
            if key.revoked { "revoked" } else { "active" }
        )),
    ])];
    lines.extend([
        detail_line("ID", display_or(&key.id, "not reported")),
        detail_line("Prefix", display_or(&key.prefix, "not reported")),
        detail_line("Source", api_key_source_label(&key.source).to_string()),
        detail_line("Created", display_or(&key.created_at, "not reported")),
        detail_line("Last used", display_or(&key.last_used_at, "never")),
    ]);
    if !key.revoked_at.is_empty() {
        lines.push(detail_line("Revoked", key.revoked_at.clone()));
    }

    lines.push(detail_section("Access policy"));
    lines.push(detail_line("Summary", key.access.clone()));
    let access = &key.access_json;
    lines.push(detail_line(
        "Request limit",
        access
            .get("max_estimated_input_tokens_per_request")
            .or_else(|| access.get("prompt_token_limit"))
            .map(|value| format!("{} tokens", json_amount(Some(value))))
            .unwrap_or_else(|| "Unlimited".to_string()),
    ));
    if let Some(budget) = access.get("input_token_budget") {
        lines.push(detail_line(
            "Input budget",
            format!(
                "{} tokens ({})",
                json_amount(budget.get("limit")),
                budget
                    .get("period")
                    .and_then(Value::as_str)
                    .unwrap_or("lifetime")
            ),
        ));
    }
    if let Some(providers) = access.get("providers").and_then(Value::as_array) {
        for provider in providers {
            let name = provider
                .get("provider")
                .and_then(Value::as_str)
                .unwrap_or("provider");
            let scope = provider
                .get("account_scope")
                .and_then(Value::as_str)
                .unwrap_or_else(|| {
                    if provider
                        .get("accounts")
                        .and_then(Value::as_array)
                        .is_some_and(|accounts| accounts.is_empty())
                    {
                        "all"
                    } else {
                        "selected"
                    }
                });
            let accounts = provider
                .get("accounts")
                .and_then(Value::as_array)
                .map(|accounts| {
                    accounts
                        .iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_default();
            let scope_text = if scope == "all" {
                "all accounts".to_string()
            } else if accounts.is_empty() {
                "selected accounts".to_string()
            } else {
                format!("selected: {accounts}")
            };
            lines.push(detail_line("Provider", format!("{name} ({scope_text})")));
            if let Some(limit) = provider
                .get("max_estimated_input_tokens_per_request")
                .or_else(|| provider.get("prompt_token_limit"))
            {
                lines.push(detail_line(
                    "Provider limit",
                    format!("{} tokens/request", json_amount(Some(limit))),
                ));
            }
            if let Some(account_limits) = provider.get("account_limits").and_then(Value::as_array) {
                for account_limit in account_limits {
                    let account = account_limit
                        .get("account")
                        .and_then(Value::as_str)
                        .unwrap_or("account");
                    let limit = account_limit
                        .get("max_estimated_input_tokens_per_request")
                        .or_else(|| account_limit.get("prompt_token_limit"));
                    lines.push(detail_line(
                        "Account limit",
                        format!("{}: {} tokens/request", account, json_amount(limit)),
                    ));
                }
            }
        }
    }
    if let Some(rules) = access
        .get("quota")
        .and_then(|quota| quota.get("rules"))
        .and_then(Value::as_array)
    {
        lines.push(detail_section("Renewable quotas"));
        for rule in rules {
            lines.push(detail_line(
                "Rule",
                format!(
                    "{} {} / {}",
                    json_amount(rule.get("limit")),
                    api_key_quota_metric_label(
                        rule.get("metric")
                            .and_then(Value::as_str)
                            .unwrap_or("quota")
                    ),
                    rule.get("period")
                        .and_then(Value::as_str)
                        .unwrap_or("period")
                ),
            ));
        }
    }

    if let Some(summary) = app.data.key_quota_summaries.get(&key.id) {
        lines.push(detail_section("Quota usage"));
        for line in api_key_quota_detail_lines(summary) {
            lines.push(detail_line("Balance", line));
        }
    }
    lines.push(Line::from(vec![
        Span::styled("j/k or Up/Down", btop_header_style()),
        Span::raw(" scroll  "),
        Span::styled("Enter/Esc/q", btop_header_style()),
        Span::raw(" close details"),
    ]));

    let modal = centered_rect(area, 96, 32);
    let max_scroll = lines
        .len()
        .saturating_sub(modal.height.saturating_sub(2) as usize);
    let scroll = app.api_key_details_scroll.min(max_scroll) as u16;
    frame.render_widget(Clear, modal);
    frame.render_widget(
        Paragraph::new(lines)
            .style(btop_style())
            .block(btop_block_with_border("API Key Details", BTOP_NET_BOX))
            .scroll((scroll, 0))
            .wrap(Wrap { trim: false }),
        modal,
    );
}

fn draw_api_key_reveal_modal(frame: &mut Frame<'_>, area: Rect, app: &TuiApp) {
    let modal = centered_rect(area, 96, 9);
    let inner_width = modal.width.saturating_sub(4) as usize;
    let lines = vec![
        Line::from("The API key will not be shown again."),
        Line::from("Copy it now and store it securely:"),
        Line::from(Span::styled(
            truncate_text(&app.api_key_reveal, inner_width),
            btop_header_style(),
        )),
        Line::from("Enter/Esc/q close"),
    ];
    frame.render_widget(Clear, modal);
    frame.render_widget(
        Paragraph::new(lines)
            .style(btop_style())
            .block(btop_block_with_border("New API Key", BTOP_USED_END))
            .wrap(Wrap { trim: false }),
        modal,
    );
}

fn draw_notifications(frame: &mut Frame<'_>, area: Rect, app: &TuiApp) {
    let settings = notification_settings_value(&app.data.notifications);
    let accounts = notification_account_rows(&app.data.notifications);
    let watched_count = accounts.iter().filter(|account| account.watched).count();
    let status = if bool_at(settings, &["enabled"]) {
        AccountStatus::Enabled
    } else {
        AccountStatus::Disabled
    };
    let channel = notification_channel(settings);
    let channel_details = match channel.as_str() {
        "google_chat" => format!(
            "Google Chat · webhook {}",
            if bool_at(settings, &["google_chat", "webhook_configured"]) {
                "configured"
            } else {
                "not configured"
            }
        ),
        _ => format!(
            "Telegram · bot {} · chat ID {}",
            if bool_at(settings, &["telegram", "bot_token_configured"]) {
                "configured"
            } else {
                "not configured"
            },
            display_or(
                &string_at(settings, &["telegram", "chat_id"]),
                "not configured",
            )
        ),
    };
    let mut lines = vec![
        Line::from(vec![
            account_status_circle(status, false),
            Span::styled(
                format!(
                    "  Notifications {}",
                    if status == AccountStatus::Enabled {
                        "enabled"
                    } else {
                        "disabled"
                    }
                ),
                btop_title_style(),
            ),
        ]),
        notification_info_line("Channel", channel_label(&channel)),
        notification_info_line("Connection", channel_details),
        notification_info_line(
            "Watching",
            format!("{} of {} accounts", watched_count, accounts.len()),
        ),
        Line::from(""),
        Line::from(Span::styled("Watched accounts", btop_header_style())),
    ];

    let mut last_provider = String::new();
    let mut selected_line_index = None;
    for (index, account) in accounts.iter().enumerate() {
        if account.provider != last_provider {
            if !last_provider.is_empty() {
                lines.push(Line::from(""));
            }
            lines.push(notification_provider_line(
                &account.provider_label,
                accounts
                    .iter()
                    .filter(|row| row.provider == account.provider)
                    .count(),
            ));
            last_provider = account.provider.clone();
        }
        if index == app.selected {
            selected_line_index = Some(lines.len());
        }
        lines.push(notification_account_header_line(
            account,
            area.width.saturating_sub(4) as usize,
            index == app.selected,
        ));
        lines.push(notification_account_meta_line(
            account,
            area.width.saturating_sub(4) as usize,
            index == app.selected,
        ));
    }
    if accounts.is_empty() {
        lines.push(Line::from("No provider accounts available."));
    }

    let max_rows = area.height.saturating_sub(2).max(1) as usize;
    let start = selected_line_index
        .map(|index| index.saturating_add(1).saturating_sub(max_rows))
        .unwrap_or(0);
    let visible = lines
        .into_iter()
        .skip(start)
        .take(max_rows)
        .collect::<Vec<_>>();
    frame.render_widget(
        Paragraph::new(visible)
            .style(btop_style())
            .block(btop_block_with_border("Notifications", BTOP_NET_BOX))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn notification_info_line(label: &str, value: impl Into<String>) -> Line<'static> {
    detail_line(label, value)
}

fn notification_provider_line(label: &str, count: usize) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("  {label}"), btop_header_style()),
        Span::styled(
            format!("  {count} account{}", if count == 1 { "" } else { "s" }),
            btop_muted_style(),
        ),
    ])
}

fn notification_account_header_line(
    account: &NotificationAccountRow,
    content_width: usize,
    selected: bool,
) -> Line<'static> {
    let marker = if selected { "> " } else { "  " };
    let status = account_status(account.enabled, false);
    let watched = if account.watched { "[watch]" } else { "[ ]" };
    let label_width = content_width
        .saturating_sub(marker.len() + 1 + watched.len() + 2)
        .max(8);
    let label = if account.label.trim().is_empty() {
        display_or(&account.account_id, "Account")
    } else {
        account.label.clone()
    };
    Line::from(vec![
        Span::styled(marker, selected_or(btop_style(), selected)),
        account_status_circle(status, selected),
        Span::styled(" ", selected_or(btop_style(), selected)),
        Span::styled(watched, selected_or(btop_header_style(), selected)),
        Span::styled(" ", selected_or(btop_style(), selected)),
        Span::styled(
            truncate_text(&label, label_width),
            selected_or(btop_title_style(), selected),
        ),
    ])
}

fn notification_account_meta_line(
    account: &NotificationAccountRow,
    content_width: usize,
    selected: bool,
) -> Line<'static> {
    let mut details = Vec::new();
    if !account.account_id.trim().is_empty() {
        details.push(account.account_id.clone());
    }
    if !account.credential_file.trim().is_empty() {
        details.push(account.credential_file.clone());
    }
    if details.is_empty() {
        details.push(account.key.clone());
    }
    Line::from(Span::styled(
        truncate_text(&format!("     {}", details.join(" · ")), content_width),
        selected_or(btop_muted_style(), selected),
    ))
}

fn draw_command_modal(frame: &mut Frame<'_>, area: Rect, app: &TuiApp) {
    let modal_width = 86u16.min(area.width.saturating_sub(2)).max(1);
    let content_width = modal_width.saturating_sub(2) as usize;
    let lines = command_modal_rows(app)
        .into_iter()
        .map(|(key, description)| {
            if key.is_empty() {
                if description.is_empty() {
                    Line::from("")
                } else {
                    Line::from(Span::styled(description, btop_title_style()))
                }
            } else {
                command_modal_key_line(&key, description, content_width)
            }
        })
        .collect::<Vec<_>>();
    let modal_height = lines.len().saturating_add(2).min(u16::MAX as usize) as u16;
    let modal = centered_rect(area, modal_width, modal_height);
    let block = btop_block_with_border("Commands", BTOP_DIV_LINE);
    let inner = block.inner(modal);
    let visible_height = inner.height as usize;
    let max_scroll = lines.len().saturating_sub(visible_height);
    let scroll = app.command_modal_scroll.min(max_scroll) as u16;
    frame.render_widget(Clear, modal);
    frame.render_widget(
        Paragraph::new(lines)
            .style(btop_style())
            .block(block)
            .scroll((scroll, 0)),
        modal,
    );
}

fn command_modal_rows(app: &TuiApp) -> Vec<(String, String)> {
    let mut rows = vec![
        (String::new(), "GENERAL".to_string()),
        ("?".to_string(), "open/close this shortcut list".to_string()),
        ("q/Q".to_string(), "close the active modal".to_string()),
        (
            "Esc".to_string(),
            "close command/details modal or cancel".to_string(),
        ),
        ("Ctrl+C".to_string(), "quit the TUI".to_string()),
        ("Tab".to_string(), "next tab".to_string()),
        ("Shift-Tab".to_string(), "previous tab".to_string()),
        (
            "r".to_string(),
            "refresh focused account (other tabs: current data)".to_string(),
        ),
        ("R".to_string(), "refresh all displayed data".to_string()),
        (
            "Ctrl+R".to_string(),
            "force fresh server quotas (Ctrl+Shift+R too)".to_string(),
        ),
        (
            "b".to_string(),
            "animated UI: toggle usage bubbles".to_string(),
        ),
        ("o".to_string(), "log in to the gateway".to_string()),
        (String::new(), "MODAL NAVIGATION".to_string()),
        (
            "j/k · Up/Down".to_string(),
            "scroll the shortcut list or details".to_string(),
        ),
        (
            "PageUp/PageDown".to_string(),
            "scroll ten lines in a modal".to_string(),
        ),
        (
            "Home/End".to_string(),
            "jump to the start/end of a modal".to_string(),
        ),
    ];

    rows.insert(
        5,
        (
            "F4 / Ctrl+V".to_string(),
            "switch to/from optional animated UI (Esc returns)".to_string(),
        ),
    );
    rows.insert(
        6,
        (
            "Space · PgUp/PgDn".to_string(),
            "animated UI: pause motion · page cats (Home/End: first/last)".to_string(),
        ),
    );
    rows.insert(
        7,
        (
            "t · j/k".to_string(),
            "animated Overview: switch theme · move in focused section".to_string(),
        ),
    );

    match app.tab {
        Tab::Overview => {
            rows.push((String::new(), "OVERVIEW".to_string()));
            rows.push((
                if app.animation.active {
                    "h/l · Left/Right"
                } else {
                    "h/l"
                }
                .to_string(),
                if app.animation.active {
                    "previous/next cat; model cat is leftmost".into()
                } else {
                    format!(
                        "previous/next section (currently {})",
                        app.overview_section.title()
                    )
                },
            ));
            match app.overview_section {
                OverviewSection::Metrics => {
                    rows.push((String::new(), "METRICS".to_string()));
                }
                OverviewSection::ContextUsage => {
                    rows.push((String::new(), "CONTEXT USAGE".to_string()));
                }
                OverviewSection::CustomModels => {
                    rows.push((String::new(), "CUSTOM MODELS".to_string()));
                    rows.push(("j/k · Up/Down".to_string(), "select a model".to_string()));
                    rows.push(("e".to_string(), "enable selected model".to_string()));
                    rows.push(("d".to_string(), "disable selected model".to_string()));
                    rows.push(("x".to_string(), "delete selected model".to_string()));
                }
                OverviewSection::AccountUsage => {
                    rows.push((String::new(), "ACCOUNT USAGE".to_string()));
                    rows.push(("j/k · Up/Down".to_string(), "select an account".to_string()));
                    rows.push((
                        "Ctrl+j/k/Enter".to_string(),
                        "reorder selected account within its provider".to_string(),
                    ));
                    rows.push((
                        "Ctrl+Up/Down".to_string(),
                        "reorder selected provider".to_string(),
                    ));
                    rows.push(("Enter".to_string(), "show account details".to_string()));
                    rows.push((
                        "s".to_string(),
                        "edit Claude automatic reset policy (confirmation required)".to_string(),
                    ));
                    rows.push((
                        "z".to_string(),
                        "select a Codex/Claude limit reset (confirmation required)".to_string(),
                    ));
                    rows.push((
                        "n".to_string(),
                        "add an account (choose provider)".to_string(),
                    ));
                    rows.push((
                        "a".to_string(),
                        format!("toggle quota view ({})", app.limit_display_mode.label()),
                    ));
                    rows.push((
                        "i".to_string(),
                        "edit compatible API name, URLs, key, and models".to_string(),
                    ));
                    rows.push(("e".to_string(), "enable selected account".to_string()));
                    rows.push(("d".to_string(), "disable selected account".to_string()));
                    rows.push(("p".to_string(), "hide selected provider".to_string()));
                    rows.push(("v".to_string(), "hide selected account".to_string()));
                    rows.push(("u".to_string(), "show all hidden account rows".to_string()));
                    rows.push(("x".to_string(), "delete selected account".to_string()));
                }
            }
        }
        Tab::Keys => {
            rows.push((String::new(), "API KEYS".to_string()));
            rows.push(("j/k · Up/Down".to_string(), "select an API key".to_string()));
            rows.push(("Enter".to_string(), "show API key details".to_string()));
            rows.push(("n".to_string(), "create an API key".to_string()));
            rows.push(("e".to_string(), "edit selected key access".to_string()));
            rows.push(("x".to_string(), "revoke selected API key".to_string()));
        }
        Tab::Notifications => {
            rows.push((String::new(), "NOTIFICATIONS".to_string()));
            rows.push((
                "j/k · Up/Down".to_string(),
                "select a provider account".to_string(),
            ));
            rows.push((
                "Enter · Space · w".to_string(),
                "watch/unwatch selected account".to_string(),
            ));
            rows.push((
                "p".to_string(),
                "watch/unwatch selected provider".to_string(),
            ));
            rows.push((
                "a/u".to_string(),
                "watch all / unwatch all accounts".to_string(),
            ));
            rows.push(("e".to_string(), "enable/disable notifications".to_string()));
            rows.push(("c".to_string(), "switch Telegram/Google Chat".to_string()));
            rows.push(("s".to_string(), "edit notification settings".to_string()));
            rows.push(("t".to_string(), "send a test notification".to_string()));
        }
    }
    let context = if app.tab == Tab::Overview {
        format!("{} / {}", app.tab.title(), app.overview_section.title())
    } else {
        app.tab.title().to_string()
    };
    rows.push((String::new(), format!("CURRENT CONTEXT: {context}")));
    rows
}

const COMMAND_MODAL_KEY_WIDTH: usize = 18;

fn command_modal_key_line(
    key: &str,
    description: impl Into<String>,
    column_width: usize,
) -> Line<'static> {
    let style = if key == "!" {
        btop_fg(BTOP_ATTENTION).add_modifier(Modifier::BOLD)
    } else {
        btop_header_style()
    };
    let key_width = COMMAND_MODAL_KEY_WIDTH.min(column_width.saturating_sub(1));
    let description_width = column_width.saturating_sub(key_width);
    let key = truncate_text(key, key_width);
    let description = truncate_text(&description.into(), description_width);
    Line::from(vec![
        Span::styled(format!("{key:<key_width$}"), style),
        Span::raw(description),
    ])
}

fn draw_account_details_modal(frame: &mut Frame<'_>, area: Rect, app: &TuiApp) {
    let Some(account) = app.selected_account() else {
        return;
    };
    let status = account_status(account.enabled, account.cooldown);
    let snapshot_values = account_snapshot_values(&app.data.snapshot, &account);
    let quota_rows = app
        .data
        .quotas
        .iter()
        .filter(|quota| {
            quota.provider == account.provider && quota_matches_account(quota, &account)
        })
        .collect::<Vec<_>>();
    let models = snapshot_model_names(&snapshot_values);
    let reset_credits = reset_credit_details(&snapshot_values);
    let endpoint = snapshot_values.iter().find_map(|value| {
        first_string_recursive(
            value,
            &[
                "api_base_url",
                "base_url",
                "resource_url",
                "openai_base_url",
                "anthropic_base_url",
            ],
        )
    });
    let project = snapshot_values.iter().find_map(|value| {
        first_string_recursive(value, &["project_id", "account_type", "plan_type"])
    });
    let generated_at = string_at(&app.data.snapshot, &["generated_at"]);
    let summary = account_quota_summary_fields(&app.data.snapshot, &account);

    let mut lines = vec![
        Line::from(vec![
            account_status_circle(status, false),
            Span::raw(format!(
                "  {} ({})",
                account_display(&account),
                account_status_label(status)
            )),
        ]),
        detail_line(
            "Provider / ID",
            format!(
                "{} / {}",
                account.provider_label,
                display_or(&account.account_id, "not reported")
            ),
        ),
        detail_line(
            "Credential / priority",
            format!(
                "{} / {}",
                display_or(&account.file_name, "not reported"),
                if account.priority { "first" } else { "normal" }
            ),
        ),
        detail_section("Quota"),
    ];

    if !summary.is_empty() {
        for field in &summary {
            lines.push(detail_line(&field.label, field.value.clone()));
        }
    } else if quota_rows.is_empty() {
        lines.push(detail_line("Usage", "No quota snapshot for this account"));
    } else {
        for quota in &quota_rows {
            let label = if quota.limit_label.trim().is_empty() {
                "usage".to_string()
            } else {
                quota.limit_label.clone()
            };
            let text = if quota.detail_text.trim().is_empty() {
                let used = display_or(&quota.used_percent, "not reported");
                let remaining = if quota.remaining_percent.trim().is_empty() {
                    String::new()
                } else {
                    format!("; remaining {}", quota.remaining_percent)
                };
                let reset = if quota.reset.trim().is_empty() {
                    String::new()
                } else {
                    format!(
                        "; reset {}",
                        account_usage_reset_label(&account.provider, &quota.reset)
                    )
                };
                format!("{used} used{remaining}{reset}")
            } else {
                quota.detail_text.clone()
            };
            lines.push(detail_line(&label, text));
        }
    }

    if account.provider == "compatible" {
        for value in &snapshot_values {
            for (field, label) in [
                ("base_url", "OpenAI base URL"),
                ("anthropic_base_url", "Anthropic base URL"),
            ] {
                if let Some(url) = value.get(field).and_then(Value::as_str) {
                    lines.push(detail_line(label, url.to_string()));
                }
            }
        }
    }
    if ["codex", "claude"].contains(&account.provider.as_str()) {
        lines.push(detail_section("Credits"));
        let empty = json!({});
        for field in provider_quota_summary::credit_fields(
            snapshot_values
                .iter()
                .copied()
                .find(|v| v.get("credits").is_some())
                .unwrap_or(&empty),
        ) {
            lines.push(detail_line(&field.label, field.value));
        }
    }
    if account.provider == "claude" {
        for value in &snapshot_values {
            if let Some(p) = value.get("reset_automation") {
                lines.push(detail_section("Automatic resets"));
                lines.push(detail_line(
                    "Policy",
                    if p["enabled"] == true {
                        "Enabled"
                    } else {
                        "Disabled"
                    },
                ));
                lines.push(detail_line("Schedule", format!("Every {} min; expiry window {} min; final check {} min; avoid natural reset within {} min", p["scan_interval_minutes"],p["expiry_window_minutes"],p["final_attempt_minutes"],p["min_natural_reset_remaining_minutes"])));
                lines.push(detail_line(
                    "Manage",
                    "s: edit Claude automatic reset policy",
                ));
            }
        }
    }
    lines.push(detail_section("Available reset limit"));
    match reset_credits {
        Some((count, credits)) => {
            lines.push(detail_line(
                "Available",
                format!(
                    "{count} reset credit{} (snapshot; fresh check before use)",
                    if count == 1 { "" } else { "s" },
                ),
            ));
            for credit in credits {
                lines.push(detail_line("Reset credit", credit));
            }
        }
        None => lines.push(detail_line("Available", "Not reported by this provider")),
    }
    if ["codex", "claude"].contains(&account.provider.as_str()) {
        lines.push(detail_line(
            "Manage",
            "z: select a reset, then confirm; R: refresh from account list",
        ));
        for value in &snapshot_values {
            if let Some(action) = value.get("last_reset_action") {
                lines.push(detail_line(
                    "Last reset",
                    format!(
                        "{} {}",
                        string_at(action, &["state"]),
                        string_at(action, &["updated_at"])
                    ),
                ));
            }
        }
    }

    lines.push(detail_section("Models"));
    if models.is_empty() {
        lines.push(detail_line(
            "Available",
            "No model metadata for this account",
        ));
    } else {
        for model in models {
            lines.push(detail_line("Model", model));
        }
    }

    lines.push(detail_section("Connection details"));
    lines.push(detail_line(
        "Endpoint / plan",
        format!(
            "{}{}",
            endpoint.unwrap_or_else(|| "not reported".to_string()),
            project
                .map(|value| format!(" / {value}"))
                .unwrap_or_default()
        ),
    ));
    lines.push(detail_line(
        "Requests / errors",
        format!(
            "{} / {}",
            format_count(account.requests),
            format_count(account.errors)
        ),
    ));
    lines.push(detail_line(
        "Last success / snapshot",
        format!(
            "{} / {}",
            display_or(&account.last_success_at, "not recorded"),
            display_or(&generated_at, "not recorded")
        ),
    ));

    lines.push(detail_section("Attention needed / Last error"));
    let mut attention = Vec::new();
    attention.extend(account.attention_reasons.iter().cloned());
    if status == AccountStatus::Disabled {
        attention.push("Account is disabled".to_string());
    }
    if status == AccountStatus::Cooldown {
        let remaining = account
            .cooldown_remaining_seconds
            .map(format_duration)
            .unwrap_or_else(|| "remaining time unavailable".to_string());
        attention.push(format!("Router cooldown active ({remaining} remaining)"));
    }
    if !account.last_error_message.trim().is_empty() {
        let when = if account.last_error_at.trim().is_empty() {
            String::new()
        } else {
            format!(" at {}", account.last_error_at)
        };
        attention.push(format!(
            "{}{}",
            short_message(&account.last_error_message),
            when
        ));
    } else if account.errors > 0 {
        attention.push(format!(
            "{} recorded error{}",
            account.errors,
            if account.errors == 1 { "" } else { "s" }
        ));
    }
    if quota_rows.is_empty() {
        attention.push("Quota data is missing from the current snapshot".to_string());
    }
    if attention.is_empty() {
        lines.push(detail_line("Status", "No attention needed"));
    } else {
        lines.push(detail_line(
            "Attention",
            truncate(&attention.join(" | "), 120),
        ));
    }
    lines.push(Line::from(vec![
        Span::styled("j/k or Up/Down", btop_header_style()),
        Span::raw(" scroll  "),
        Span::styled("Enter/Esc/q", btop_header_style()),
        Span::raw(" close details"),
    ]));

    let modal = centered_rect(area, 86, 32);
    // Summaries can wrap on compact terminals. Count rendered rows so the last
    // balances/models remain reachable rather than stopping at logical lines.
    let lines = wrap_detail_lines(lines, modal.width.saturating_sub(2));
    let max_scroll = lines
        .len()
        .saturating_sub(modal.height.saturating_sub(2) as usize);
    let scroll = app.account_details_scroll.min(max_scroll) as u16;
    frame.render_widget(Clear, modal);
    frame.render_widget(
        Paragraph::new(lines)
            .style(btop_style())
            .block(btop_block_with_border("Account Details", BTOP_MEM_BOX))
            .scroll((scroll, 0)),
        modal,
    );
}

fn wrap_detail_lines(lines: Vec<Line<'static>>, width: u16) -> Vec<Line<'static>> {
    if width == 0 {
        return Vec::new();
    }
    let mut out = Vec::new();
    for line in lines {
        let mut row = Line::default().style(line.style);
        let mut columns = 0;
        for span in line.spans {
            let mut part = String::new();
            for ch in span.content.chars() {
                let size = Span::raw(ch.to_string()).width();
                if columns + size > usize::from(width) && columns > 0 {
                    row.spans
                        .push(Span::styled(std::mem::take(&mut part), span.style));
                    out.push(row);
                    row = Line::default().style(line.style);
                    columns = 0;
                }
                if size <= usize::from(width) {
                    part.push(ch);
                    columns += size;
                }
            }
            row.spans.push(Span::styled(part, span.style));
        }
        out.push(row);
    }
    out
}

fn account_quota_summary_fields(
    snapshot: &Value,
    account: &AccountRow,
) -> Vec<provider_quota_summary::Field> {
    let mut fields = Vec::new();
    for value in snapshot_quota_values_for_provider(snapshot, &account.provider) {
        if !snapshot_value_matches_account(value, account) {
            continue;
        }
        // Prefer the gateway's additive contract. Older gateways still work
        // through the identical local allowlisted projection, never raw JSON.
        let summary = value
            .get("quota_summary")
            .and_then(|value| {
                serde_json::from_value::<Vec<provider_quota_summary::Field>>(value.clone()).ok()
            })
            .unwrap_or_else(|| provider_quota_summary::summarize(value));
        for mut field in summary {
            field.label = field
                .label
                .chars()
                .filter(|c| !c.is_control())
                .take(180)
                .collect();
            field.value = field
                .value
                .chars()
                .filter(|c| !c.is_control())
                .take(600)
                .collect();
            if !fields.contains(&field) {
                fields.push(field);
            }
        }
    }
    fields
}

fn detail_section(title: &str) -> Line<'static> {
    Line::from(Span::styled(title.to_string(), btop_header_style()))
}

fn detail_line(label: &str, value: impl Into<String>) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{label:<18}  "), btop_muted_style()),
        Span::raw(value.into()),
    ])
}

fn display_or(value: &str, fallback: &str) -> String {
    if value.trim().is_empty() {
        fallback.to_string()
    } else {
        value.to_string()
    }
}

fn format_duration(seconds: u64) -> String {
    if seconds < 60 {
        format!("{seconds}s")
    } else {
        format!("{}m {}s", seconds / 60, seconds % 60)
    }
}

fn draw_confirmation_modal(frame: &mut Frame<'_>, area: Rect, modal: &ConfirmationModal) {
    let area = centered_rect(area, 62, 7);
    let lines = vec![
        Line::from(modal.message.clone()),
        Line::from(""),
        Line::from(vec![
            Span::styled("y/Enter", btop_header_style()),
            Span::raw(" confirm"),
            Span::styled("    n/q/Esc", btop_header_style()),
            Span::raw(" cancel"),
        ]),
    ];
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines)
            .style(btop_style())
            .block(btop_block_with_border(modal.title.clone(), BTOP_USED_END))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn centered_rect(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width.saturating_sub(2)).max(1);
    let height = height.min(area.height.saturating_sub(2)).max(1);
    Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    }
}

fn prompt_account_provider(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
) -> Result<String, String> {
    let value = prompt_in_terminal(
        terminal,
        "Provider [1=Codex, 2=Antigravity, 3=Gemini, 4=Qwen, 5=DeepSeek, 6=MiniMax, 7=Grok, 8=Copilot, 9=Claude, 10=GLM, 11=Compatible API]: ",
    )?;
    let normalized = value.trim().to_ascii_lowercase();
    let provider = match normalized.as_str() {
        "1" | "codex" | "chatgpt" => "codex",
        "2" | "antigravity" | "agw" => "antigravity",
        "3" | "gemini" => "gemini",
        "4" | "qwen" => "qwen",
        "5" | "deepseek" => "deepseek",
        "6" | "minimax" => "minimax",
        "7" | "grok" | "xai" => "grok",
        "8" | "copilot" | "github" => "copilot",
        "9" | "claude" | "anthropic" => "claude",
        "10" | "glm" | "zai" | "z.ai" => "glm",
        "11" | "compatible" | "cmp" | "openai" | "anthropic-compatible" => "compatible",
        _ => return Err("unknown provider; choose a listed number or provider name".to_string()),
    };
    Ok(provider.to_string())
}

fn prompt_compatible_field(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    label: &str,
    current: &str,
    removable: bool,
) -> Result<String, String> {
    let suffix = if removable { "; - clears" } else { "" };
    let prompt = if current.is_empty() {
        format!("{label} (blank = none): ")
    } else {
        format!("{label} [{current}] (blank = keep{suffix}): ")
    };
    let value = prompt_in_terminal(terminal, &prompt)?.trim().to_string();
    Ok(if value.is_empty() {
        current.to_string()
    } else if removable && value == "-" {
        String::new()
    } else {
        value
    })
}

fn prompt_secret_in_terminal(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    prompt: &str,
) -> Result<String, String> {
    execute!(terminal.backend_mut(), LeaveAlternateScreen, cursor::Show)
        .map_err(|e| e.to_string())?;
    let result = (|| {
        let mut output = io::stdout();
        write!(output, "\r\n{prompt}").map_err(|e| e.to_string())?;
        output.flush().map_err(|e| e.to_string())?;
        let mut value = String::new();
        loop {
            match event::read().map_err(|e| e.to_string())? {
                Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                    KeyCode::Enter => break,
                    KeyCode::Esc => return Err("API key entry cancelled".to_string()),
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        return Err("API key entry cancelled".to_string())
                    }
                    KeyCode::Char(ch) => {
                        value.push(ch);
                    }
                    KeyCode::Backspace => {
                        value.pop();
                    }
                    _ => {}
                },
                Event::Paste(text) => value.push_str(&text),
                _ => {}
            }
        }
        Ok(value.trim().to_string())
    })();
    execute!(terminal.backend_mut(), EnterAlternateScreen, cursor::Hide)
        .map_err(|e| e.to_string())?;
    terminal.clear().map_err(|e| e.to_string())?;
    result
}

fn prompt_optional(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    prompt: &str,
) -> Result<Option<String>, String> {
    let value = prompt_in_terminal(terminal, prompt)?.trim().to_string();
    Ok((!value.is_empty()).then_some(value))
}

fn prompt_choice(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    prompt: &str,
    choices: &[(&str, &str)],
) -> Result<String, String> {
    let value = prompt_in_terminal(terminal, prompt)?;
    let value = value.trim().to_ascii_lowercase();
    choices
        .iter()
        .find(|(key, label)| value == *key || value == *label)
        .map(|(_, label)| (*label).to_string())
        .ok_or_else(|| "invalid choice".to_string())
}

fn open_account_login_url(url: &str) {
    #[cfg(target_os = "linux")]
    let _ = std::process::Command::new("xdg-open").arg(url).spawn();
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(url).spawn();
    #[cfg(target_os = "windows")]
    let _ = std::process::Command::new("cmd")
        .args(["/C", "start", "", url])
        .spawn();
}

fn prompt_in_terminal(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    prompt: &str,
) -> Result<String, String> {
    disable_raw_mode().map_err(|err| err.to_string())?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen, cursor::Show)
        .map_err(|err| err.to_string())?;
    let value = prompt_line(prompt).map_err(|err| err.to_string());
    execute!(io::stdout(), EnterAlternateScreen, cursor::Hide).map_err(|err| err.to_string())?;
    enable_raw_mode().map_err(|err| err.to_string())?;
    terminal.clear().map_err(|err| err.to_string())?;
    value
}

fn prompt_in_terminal_with_notice(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    notice: &str,
    prompt: &str,
) -> Result<String, String> {
    disable_raw_mode().map_err(|err| err.to_string())?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen, cursor::Show)
        .map_err(|err| err.to_string())?;
    println!("{notice}");
    let value = prompt_line(prompt).map_err(|err| err.to_string());
    execute!(io::stdout(), EnterAlternateScreen, cursor::Hide).map_err(|err| err.to_string())?;
    enable_raw_mode().map_err(|err| err.to_string())?;
    terminal.clear().map_err(|err| err.to_string())?;
    value
}

fn prompt_api_key_access(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    existing: Option<&Value>,
) -> Result<Value, String> {
    let prompt = if existing.is_some() {
        "Access JSON (blank = keep current): "
    } else {
        "Access JSON (blank = all providers/accounts): "
    };
    let input = prompt_in_terminal(terminal, prompt)?;
    let input = input.trim();
    let access = if input.is_empty() {
        existing
            .cloned()
            .unwrap_or_else(|| json!({ "all": true, "providers": [] }))
    } else {
        parse_json_arg(input)?
    };
    if !access.is_object() {
        return Err("API key access JSON must be an object".to_string());
    }
    Ok(access)
}

fn account_rows(summary: &Value, routing: &Value) -> Vec<AccountRow> {
    let mut usage = HashMap::<(String, String), Value>::new();
    if let Some(providers) = summary.get("providers").and_then(Value::as_object) {
        for (provider, rows) in providers {
            for row in rows.as_array().into_iter().flatten() {
                let key = string_at(row, &["key"]);
                if !key.is_empty() {
                    usage.insert(
                        (normalize_usage_provider(provider), key),
                        row.get("usage").cloned().unwrap_or(Value::Null),
                    );
                }
            }
        }
    }

    let priority = priority_map(routing);
    let mut rows = Vec::new();
    if let Some(accounts) = routing.get("accounts").and_then(Value::as_array) {
        for account in accounts {
            let provider = normalize_usage_provider(&string_at(account, &["provider"]));
            let key = string_at(account, &["key"]);
            let account_usage = usage
                .get(&(provider.clone(), key.clone()))
                .cloned()
                .unwrap_or(Value::Null);
            let provider_priority = priority.get(&provider).cloned().unwrap_or_default();
            rows.push(AccountRow {
                provider: provider.clone(),
                provider_label: string_at(account, &["provider_label"])
                    .if_empty(provider_label(&provider).to_string()),
                key: key.clone(),
                label: string_at(account, &["label"]),
                account_id: string_at(account, &["account_id"]),
                file_name: string_at(account, &["credential_file"]),
                enabled: bool_at(account, &["enabled"]),
                cooldown: false,
                cooldown_remaining_seconds: None,
                attention_reasons: Vec::new(),
                priority: provider_priority.contains(&key),
                requests: number_at(&account_usage, &["requests"]),
                errors: number_at(&account_usage, &["errors"]),
                total_tokens: number_at(&account_usage, &["total_tokens"]),
                last_success_at: string_at(&account_usage, &["last_success_at"]),
                last_error_at: string_at(&account_usage, &["last_error_at"]),
                last_error_message: string_at(&account_usage, &["last_error_message"]),
            });
        }
    }

    if rows.is_empty() {
        for ((provider, key), account_usage) in usage {
            rows.push(AccountRow {
                provider: provider.clone(),
                provider_label: provider_label(&provider).to_string(),
                key,
                label: string_at(&account_usage, &["label"]),
                account_id: string_at(&account_usage, &["account_id"]),
                enabled: true,
                cooldown: false,
                cooldown_remaining_seconds: None,
                attention_reasons: Vec::new(),
                requests: number_at(&account_usage, &["requests"]),
                errors: number_at(&account_usage, &["errors"]),
                total_tokens: number_at(&account_usage, &["total_tokens"]),
                last_success_at: string_at(&account_usage, &["last_success_at"]),
                last_error_at: string_at(&account_usage, &["last_error_at"]),
                last_error_message: string_at(&account_usage, &["last_error_message"]),
                ..AccountRow::default()
            });
        }
    }

    rows.sort_by(|left, right| {
        left.provider
            .cmp(&right.provider)
            .then_with(|| left.label.cmp(&right.label))
            .then_with(|| left.key.cmp(&right.key))
    });
    rows
}

fn apply_account_runtime_status(accounts: &mut [AccountRow], snapshot: &Value) {
    for account in accounts {
        let runtime =
            snapshot_account_for_account(snapshot, account).and_then(|value| value.get("runtime"));
        account.cooldown = runtime
            .and_then(|value| value.get("status"))
            .and_then(Value::as_str)
            .is_some_and(|status| status.eq_ignore_ascii_case("cooldown"));
        account.cooldown_remaining_seconds = runtime
            .and_then(|value| value.get("cooldown_remaining_seconds"))
            .and_then(|value| {
                value
                    .as_u64()
                    .or_else(|| value.as_i64().map(|n| n.max(0) as u64))
            });
    }
}

fn apply_account_attention(accounts: &mut [AccountRow], snapshot: &Value, quotas: &[QuotaRow]) {
    for account in accounts {
        let mut reasons = Vec::new();
        if snapshot_account_is_expired(snapshot, account) {
            reasons.push("expired tokens".to_string());
        }
        if has_current_account_error(account) {
            reasons.push("errors".to_string());
        }

        let max_quota = quotas
            .iter()
            .filter(|quota| {
                quota.provider == account.provider && quota_matches_account(quota, account)
            })
            .filter_map(|quota| quota.used_percent_value)
            .filter(|percent| percent.is_finite())
            .fold(0.0_f64, f64::max);
        if max_quota >= 95.0 {
            reasons.push("quota almost exhausted".to_string());
        } else if max_quota >= 75.0 {
            reasons.push("near quota".to_string());
        }
        account.attention_reasons = reasons;
    }
}

fn snapshot_account_is_expired(snapshot: &Value, account: &AccountRow) -> bool {
    let Some(value) = snapshot_account_for_account(snapshot, account) else {
        return false;
    };
    ["expired_at", "expires_at", "copilot_expires_at"]
        .iter()
        .filter_map(|key| {
            let value = string_at(value, &[*key]);
            (!value.trim().is_empty()).then_some(value)
        })
        .any(|value| is_expired_timestamp(&value))
}

fn has_current_account_error(account: &AccountRow) -> bool {
    if account.errors == 0 {
        return false;
    }
    let last_error = parse_timestamp_millis(&account.last_error_at);
    let last_success = parse_timestamp_millis(&account.last_success_at);
    !(last_error.is_some() && last_success.is_some() && last_success > last_error)
}

fn is_expired_timestamp(value: &str) -> bool {
    parse_timestamp_millis(value).is_some_and(|timestamp| timestamp < Utc::now().timestamp_millis())
}

fn parse_timestamp_millis(value: &str) -> Option<i64> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    if let Ok(timestamp) = DateTime::parse_from_rfc3339(value) {
        return Some(timestamp.timestamp_millis());
    }
    value.parse::<i64>().ok().map(|timestamp| {
        if timestamp.unsigned_abs() < 100_000_000_000 {
            timestamp.saturating_mul(1_000)
        } else {
            timestamp
        }
    })
}

fn snapshot_account_for_account<'a>(
    snapshot: &'a Value,
    account: &AccountRow,
) -> Option<&'a Value> {
    let candidates = snapshot_accounts_for_provider(snapshot, &account.provider);
    let account_keys = normalized_match_keys(&account_row_identity_keys(account));

    let exact = candidates.iter().find(|candidate| {
        let candidate_keys = normalized_match_keys(&account_identity_keys(candidate));
        candidate_keys
            .iter()
            .any(|key| account_keys.iter().any(|account_key| key == account_key))
    });
    if exact.is_some() {
        return exact.copied();
    }

    let fuzzy = candidates.iter().find(|candidate| {
        let candidate_keys = normalized_match_keys(&account_identity_keys(candidate));
        candidate_keys.iter().any(|candidate_key| {
            account_keys
                .iter()
                .any(|account_key| fuzzy_key_match(candidate_key, account_key))
        })
    });
    if fuzzy.is_some() {
        return fuzzy.copied();
    }

    (candidates.len() == 1).then(|| candidates[0])
}

fn account_snapshot_values<'a>(snapshot: &'a Value, account: &AccountRow) -> Vec<&'a Value> {
    let mut values = Vec::new();
    if let Some(value) = snapshot_account_for_account(snapshot, account) {
        values.push(value);
    }
    for value in snapshot_quota_values_for_provider(snapshot, &account.provider) {
        if snapshot_value_matches_account(value, account) {
            values.push(value);
        }
    }
    values
}

fn snapshot_quota_values_for_provider<'a>(snapshot: &'a Value, provider: &str) -> Vec<&'a Value> {
    let Some(quotas) = snapshot.get("quotas").and_then(Value::as_object) else {
        return Vec::new();
    };
    snapshot_provider_keys(provider)
        .into_iter()
        .find_map(|key| quotas.get(key).map(quota_account_values))
        .unwrap_or_default()
}

fn snapshot_value_matches_account(value: &Value, account: &AccountRow) -> bool {
    let account_keys = normalized_match_keys(&account_row_identity_keys(account));
    let value_keys = normalized_match_keys(&account_identity_keys(value));
    value_keys.iter().any(|value_key| {
        account_keys
            .iter()
            .any(|account_key| value_key == account_key || fuzzy_key_match(value_key, account_key))
    })
}

fn find_value_by_key<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    match value {
        Value::Object(map) => {
            if let Some(found) = map.get(key) {
                return Some(found);
            }
            map.values().find_map(|child| find_value_by_key(child, key))
        }
        Value::Array(items) => items.iter().find_map(|item| find_value_by_key(item, key)),
        _ => None,
    }
}

fn reset_credit_details(values: &[&Value]) -> Option<(u64, Vec<String>)> {
    for value in values {
        let Some(details) = find_value_by_key(value, "rate_limit_reset_credits") else {
            continue;
        };
        let count =
            number_at(details, &["available_count"]).max(number_at(details, &["availableCount"]));
        let credits = details
            .get("credits")
            .and_then(Value::as_array)
            .map(|credits| {
                credits
                    .iter()
                    .filter_map(|credit| {
                        let title =
                            first_string_recursive(credit, &["title", "description", "id"])?;
                        let status = string_at(credit, &["status"]);
                        let expires = first_string_recursive(credit, &["expires_at", "expiresAt"]);
                        let mut line = title;
                        if !status.trim().is_empty() {
                            line.push_str(&format!(" ({status})"));
                        }
                        if let Some(expires) = expires.filter(|value| !value.trim().is_empty()) {
                            line.push_str(&format!("; expires {expires}"));
                        }
                        if let Some(description) = credit.get("description").and_then(Value::as_str)
                        {
                            line.push_str(&format!("; {description}"));
                        }
                        if credit.get("usable_now") == Some(&Value::Bool(false)) {
                            line.push_str("; currently unavailable");
                        }
                        Some(line)
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        return Some((count, credits));
    }
    None
}

fn selectable_reset_credits(values: &[&Value], provider: &str) -> Vec<Value> {
    values
        .iter()
        .filter_map(|v| v.get("rate_limit_reset_credits"))
        .filter(|s| number_at(s, &["available_count"]) > 0)
        .filter_map(|s| s.get("credits").and_then(Value::as_array))
        .flatten()
        .filter(|c| {
            !string_at(c, &["id"]).is_empty()
                && c.get("status")
                    .and_then(Value::as_str)
                    .unwrap_or("available")
                    == "available"
                && (provider != "claude" || c.get("usable_now") == Some(&Value::Bool(true)))
        })
        .cloned()
        .collect()
}

fn snapshot_model_names(values: &[&Value]) -> Vec<String> {
    let mut names = Vec::new();
    for value in values {
        collect_model_arrays(value, &mut names);
    }
    let mut seen = HashSet::new();
    names
        .into_iter()
        .filter(|name| !name.trim().is_empty() && seen.insert(name.clone()))
        .collect()
}

fn collect_model_arrays(value: &Value, names: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                if matches!(key.as_str(), "models" | "available_models") {
                    collect_model_values(child, names);
                } else {
                    collect_model_arrays(child, names);
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_model_arrays(item, names);
            }
        }
        _ => {}
    }
}

fn collect_model_values(value: &Value, names: &mut Vec<String>) {
    match value {
        Value::String(name) => names.push(name.clone()),
        Value::Array(items) => {
            for item in items {
                collect_model_values(item, names);
            }
        }
        Value::Object(map) => {
            if let Some(name) = [
                "display_name",
                "displayName",
                "model_id",
                "modelId",
                "id",
                "name",
                "alias",
            ]
            .iter()
            .find_map(|key| map.get(*key).and_then(Value::as_str))
            {
                names.push(name.to_string());
            } else {
                for (key, child) in map {
                    if child.is_object() || child.is_array() {
                        collect_model_values(child, names);
                    } else if child.is_null() {
                        names.push(key.clone());
                    }
                }
            }
        }
        _ => {}
    }
}

fn priority_map(routing: &Value) -> HashMap<String, HashSet<String>> {
    let mut out = HashMap::new();
    if let Some(providers) = routing
        .get("settings")
        .and_then(|settings| settings.get("providers"))
        .and_then(Value::as_object)
    {
        for (provider, settings) in providers {
            let accounts = settings
                .get("priority_accounts")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect::<HashSet<_>>();
            out.insert(normalize_usage_provider(provider), accounts);
        }
    }
    out
}

fn quota_rows_from_snapshot(snapshot: &Value) -> Vec<QuotaRow> {
    let mut rows = Vec::new();
    if let Some(quotas) = snapshot.get("quotas").and_then(Value::as_object) {
        for (provider, quota) in quotas {
            rows.extend(quota_rows_from_provider(
                &normalize_usage_provider(provider),
                quota,
            ));
        }
    }
    rows.sort_by(|left, right| {
        left.provider
            .cmp(&right.provider)
            .then_with(|| left.label.cmp(&right.label))
    });
    rows
}

fn quota_rows_from_provider(provider: &str, value: &Value) -> Vec<QuotaRow> {
    let provider = normalize_usage_provider(provider);
    let mut source_accounts = Vec::new();
    if let Some(accounts) = value.get("accounts").and_then(Value::as_array) {
        source_accounts.extend(accounts.iter().cloned());
    } else if let Some(account) = value.get("account") {
        source_accounts.push(account.clone());
    } else if value.is_array() {
        source_accounts.extend(value.as_array().into_iter().flatten().cloned());
    }

    // Grok's live quota endpoint keeps the account identity under `account`
    // and the live request/token limits beside it under `kinds`. Attach the
    // latter to the account copy so the row parser can preserve every kind.
    if provider == "grok" {
        let kinds = value.get("kinds").cloned();
        let observed = value.get("observed_rate_limits").cloned();
        for account in &mut source_accounts {
            if let Value::Object(map) = account {
                if let Some(kinds) = &kinds {
                    map.insert("kinds".to_string(), kinds.clone());
                }
                if let Some(observed) = &observed {
                    map.insert("limits".to_string(), observed.clone());
                }
            }
        }
    }

    source_accounts
        .iter()
        .flat_map(|account| quota_rows_from_account(&provider, account))
        .collect()
}

fn quota_rows_from_account(provider: &str, account: &Value) -> Vec<QuotaRow> {
    let base_label = first_string_recursive(
        account,
        &["label", "email", "account_id", "file_name", "name", "model"],
    )
    .unwrap_or_else(|| "account".to_string());
    let account_key = account_identity_keys(account)
        .into_iter()
        .next()
        .unwrap_or_else(|| base_label.clone());
    let mut rows = Vec::new();
    match provider {
        "codex" => push_codex_quota_rows(&mut rows, provider, &account_key, &base_label, account),
        "antigravity" | "gemini" => {
            push_google_quota_rows(&mut rows, provider, &account_key, &base_label, account)
        }
        "qwen" | "claude" => {
            push_named_limit_rows(&mut rows, provider, &account_key, &base_label, account)
        }
        "minimax" => {
            push_minimax_quota_rows(&mut rows, provider, &account_key, &base_label, account)
        }
        "grok" => push_grok_quota_rows(&mut rows, provider, &account_key, &base_label, account),
        _ => {}
    }
    if rows.is_empty() {
        collect_quota_limit_rows(
            provider,
            &account_key,
            &base_label,
            account,
            None,
            &mut rows,
        );
    }
    if rows.is_empty() {
        let used_percent_value =
            first_number_recursive(account, &["used_percent", "usage_percent", "percent_used"])
                .or_else(|| quota_percent_value(account));
        let remaining_percent = quota_remaining_percent_value(account);
        let reset = first_string_recursive(account, &["reset_label", "reset_at", "window"])
            .unwrap_or_default();
        rows.push(QuotaRow {
            provider: provider.to_string(),
            account_key,
            account_label: base_label.clone(),
            limit_label: String::new(),
            label: base_label,
            detail_text: quota_detail_from_parts(
                provider,
                used_percent_value,
                remaining_percent,
                &reset,
                account,
            ),
            overview: true,
            used_percent_value,
            used_percent: percent(used_percent_value),
            remaining_percent: percent(remaining_percent),
            reset,
        });
    }
    disambiguate_quota_labels(&mut rows);
    rows
}

fn push_codex_quota_rows(
    rows: &mut Vec<QuotaRow>,
    provider: &str,
    account_key: &str,
    account_label: &str,
    account: &Value,
) {
    for (field, label) in [
        ("code_generation", "Code Gen"),
        ("code_review", "Code Review"),
    ] {
        if let Some(value) = account.get(field) {
            push_window_rows(
                rows,
                provider,
                account_key,
                account_label,
                label,
                value,
                true,
            );
        }
    }
    for limit in array_values(account.get("additional_rate_limits")) {
        let label = first_string_recursive(
            limit,
            &[
                "display_name",
                "displayName",
                "limit_name",
                "limitName",
                "name",
            ],
        )
        .unwrap_or_else(|| "Model limit".to_string());
        if let Some(rate_limit) = limit.get("rate_limit").or_else(|| limit.get("rateLimit")) {
            push_window_rows(
                rows,
                provider,
                account_key,
                account_label,
                &label,
                rate_limit,
                true,
            );
        } else {
            push_window_rows(
                rows,
                provider,
                account_key,
                account_label,
                &label,
                limit,
                true,
            );
        }
    }
}

fn push_google_quota_rows(
    rows: &mut Vec<QuotaRow>,
    provider: &str,
    account_key: &str,
    account_label: &str,
    account: &Value,
) {
    for group in array_values(account.get("groups")) {
        let label = first_string_recursive(group, &["display_name", "displayName", "name"])
            .unwrap_or_else(|| "Group".to_string());
        push_window_rows(
            rows,
            provider,
            account_key,
            account_label,
            &label,
            group,
            true,
        );
    }

    let mut families = HashMap::<String, (Option<f64>, Option<f64>, String)>::new();
    for model in array_values(account.get("models")) {
        let model_label = first_string_recursive(
            model,
            &["display_name", "displayName", "model_id", "modelId", "name"],
        )
        .unwrap_or_else(|| "Model".to_string());
        let family = gemini_model_family(&model_label);
        let bucket = model
            .get("current")
            .or_else(|| model.get("quota"))
            .or_else(|| model.get("limit"));
        if let Some(bucket) = bucket {
            let used = quota_percent_value(bucket);
            let remaining = quota_remaining_percent_value(bucket);
            if provider == "gemini" {
                if let Some(family) = family {
                    let entry =
                        families
                            .entry(family.to_string())
                            .or_insert((None, None, String::new()));
                    if used.is_some_and(|value| entry.0.is_none_or(|current| value > current)) {
                        entry.0 = used;
                        entry.2 = first_string_recursive(bucket, &["reset_label", "reset_at"])
                            .unwrap_or_default();
                    }
                    if remaining.is_some_and(|value| entry.1.is_none_or(|current| value < current))
                    {
                        entry.1 = remaining;
                    }
                }
            }
            let label = if provider == "gemini" {
                model_label.clone()
            } else {
                model_label.clone()
            };
            rows.push(quota_row_from_value(
                provider,
                account_key,
                account_label,
                &label,
                bucket,
                false,
            ));
        }
    }

    if provider == "gemini" {
        for family in ["Flash", "Flash Lite", "Pro"] {
            if let Some((used, remaining, reset)) = families.remove(family) {
                rows.push(quota_row_from_parts(
                    provider,
                    account_key,
                    account_label,
                    &format!("Gemini {family}"),
                    used,
                    remaining,
                    &reset,
                    true,
                ));
            }
        }
        let mut extra = families.into_iter().collect::<Vec<_>>();
        extra.sort_by(|left, right| left.0.cmp(&right.0));
        for (family, (used, remaining, reset)) in extra {
            rows.push(quota_row_from_parts(
                provider,
                account_key,
                account_label,
                &format!("Gemini {family}"),
                used,
                remaining,
                &reset,
                true,
            ));
        }
    }
}

fn push_named_limit_rows(
    rows: &mut Vec<QuotaRow>,
    provider: &str,
    account_key: &str,
    account_label: &str,
    account: &Value,
) {
    for limit in array_values(account.get("limits")) {
        let label = first_string_recursive(limit, &["label", "scope", "name"])
            .unwrap_or_else(|| "Limit".to_string());
        rows.push(quota_row_from_value(
            provider,
            account_key,
            account_label,
            &label,
            limit,
            true,
        ));
    }
    if let Some(current) = account.get("current_window") {
        rows.push(quota_row_from_value(
            provider,
            account_key,
            account_label,
            "5h window",
            current,
            true,
        ));
    }
    if let Some(weekly) = account.get("weekly") {
        rows.push(quota_row_from_value(
            provider,
            account_key,
            account_label,
            "Weekly window",
            weekly,
            true,
        ));
    }
    for limit in array_values(account.get("additional_rate_limits")) {
        let label = first_string_recursive(limit, &["display_name", "displayName", "name"])
            .unwrap_or_else(|| "Model limit".to_string());
        push_window_rows(
            rows,
            provider,
            account_key,
            account_label,
            &label,
            limit,
            true,
        );
    }
}

fn push_minimax_quota_rows(
    rows: &mut Vec<QuotaRow>,
    provider: &str,
    account_key: &str,
    account_label: &str,
    account: &Value,
) {
    if let Some(current) = account.get("current_window") {
        rows.push(quota_row_from_value(
            provider,
            account_key,
            account_label,
            "5h window",
            current,
            true,
        ));
    }
    if let Some(weekly) = account.get("weekly") {
        rows.push(quota_row_from_value(
            provider,
            account_key,
            account_label,
            "Weekly window",
            weekly,
            true,
        ));
    }
    for model in array_values(account.get("models")) {
        let model_label = first_string_recursive(
            model,
            &[
                "display_name",
                "displayName",
                "model_id",
                "model_name",
                "name",
            ],
        )
        .unwrap_or_else(|| "Model".to_string());
        for (field, suffix) in [("current", "5h window"), ("weekly", "Weekly window")] {
            if let Some(bucket) = model.get(field) {
                rows.push(quota_row_from_value(
                    provider,
                    account_key,
                    account_label,
                    &format!("{model_label} {suffix}"),
                    bucket,
                    false,
                ));
            }
        }
    }
}

fn push_grok_quota_rows(
    rows: &mut Vec<QuotaRow>,
    provider: &str,
    account_key: &str,
    account_label: &str,
    account: &Value,
) {
    for (kind, kind_label, show_tokens) in [
        ("DEFAULT_TEXT", "Text", true),
        ("DEFAULT_IMAGE", "Image", false),
        ("DEFAULT_VIDEO", "Video", false),
    ] {
        let Some(rate_limits) = account
            .get("kinds")
            .and_then(|kinds| kinds.get(kind))
            .and_then(|kind| kind.get("rate_limits"))
        else {
            continue;
        };
        for (scope, label) in [("requests", "requests"), ("tokens", "tokens")] {
            if scope == "tokens" && !show_tokens {
                continue;
            }
            if let Some(bucket) = rate_limits.get(scope) {
                rows.push(quota_row_from_value(
                    provider,
                    account_key,
                    account_label,
                    &format!("{kind_label} {label}"),
                    bucket,
                    true,
                ));
            }
        }
    }
    for limit in array_values(account.get("limits")) {
        let label = first_string_recursive(limit, &["label", "scope", "name"])
            .unwrap_or_else(|| "Limit".to_string());
        rows.push(quota_row_from_value(
            provider,
            account_key,
            account_label,
            &label,
            limit,
            true,
        ));
    }
}

fn push_window_rows(
    rows: &mut Vec<QuotaRow>,
    provider: &str,
    account_key: &str,
    account_label: &str,
    label: &str,
    value: &Value,
    overview: bool,
) {
    for (field, suffix) in [("five_hour", "5h"), ("weekly", "Weekly")] {
        if let Some(bucket) = value.get(field) {
            rows.push(quota_row_from_value(
                provider,
                account_key,
                account_label,
                &format!("{label} {suffix}"),
                bucket,
                overview,
            ));
        }
    }
}

fn quota_row_from_value(
    provider: &str,
    account_key: &str,
    account_label: &str,
    limit_label: &str,
    value: &Value,
    overview: bool,
) -> QuotaRow {
    let used_percent_value = quota_percent_value(value);
    let remaining_percent = quota_remaining_percent_value(value);
    let reset = first_string_recursive(value, &["reset_label", "reset_at", "reset_time"])
        .unwrap_or_default();
    QuotaRow {
        provider: provider.to_string(),
        account_key: account_key.to_string(),
        account_label: account_label.to_string(),
        limit_label: limit_label.to_string(),
        label: format!("{account_label} / {limit_label}"),
        detail_text: quota_detail_from_parts(
            provider,
            used_percent_value,
            remaining_percent,
            &reset,
            value,
        ),
        overview,
        used_percent_value,
        used_percent: percent(used_percent_value),
        remaining_percent: percent(remaining_percent),
        reset,
    }
}

fn quota_row_from_parts(
    provider: &str,
    account_key: &str,
    account_label: &str,
    limit_label: &str,
    used_percent_value: Option<f64>,
    remaining_percent: Option<f64>,
    reset: &str,
    overview: bool,
) -> QuotaRow {
    QuotaRow {
        provider: provider.to_string(),
        account_key: account_key.to_string(),
        account_label: account_label.to_string(),
        limit_label: limit_label.to_string(),
        label: format!("{account_label} / {limit_label}"),
        detail_text: quota_detail_from_parts(
            provider,
            used_percent_value,
            remaining_percent,
            reset,
            &Value::Null,
        ),
        overview,
        used_percent_value,
        used_percent: percent(used_percent_value),
        remaining_percent: percent(remaining_percent),
        reset: reset.to_string(),
    }
}

fn quota_detail_from_parts(
    provider: &str,
    used_percent: Option<f64>,
    remaining_percent: Option<f64>,
    reset: &str,
    value: &Value,
) -> String {
    let used_text = first_string_recursive(value, &["used_text"])
        .or_else(|| scalar_text(value, &["used"]))
        .unwrap_or_default();
    let limit_text = first_string_recursive(value, &["limit_text"])
        .or_else(|| scalar_text(value, &["limit"]))
        .unwrap_or_default();
    if (provider == "qwen" || provider == "claude")
        && (!used_text.is_empty() || !limit_text.is_empty())
    {
        let mut text = format!("{used_text}/{limit_text}");
        if !reset.trim().is_empty() {
            text.push(' ');
            text.push_str(reset.trim());
        }
        return text;
    }
    if provider == "grok" && (!used_text.is_empty() || !limit_text.is_empty()) {
        let remaining = scalar_text(value, &["remaining"]).unwrap_or_else(|| "?".to_string());
        let limit = if limit_text.is_empty() {
            "?".to_string()
        } else {
            limit_text
        };
        return format!("{remaining}/{limit} (no reset info)");
    }
    if provider == "minimax" {
        let used = used_percent
            .map(|value| format!("{value:.1}% used"))
            .unwrap_or_else(|| "— used".to_string());
        let reset = if reset.trim().is_empty() {
            "resets in —".to_string()
        } else if reset.to_ascii_lowercase().starts_with("resets in ") {
            reset.trim().to_string()
        } else {
            format!("resets in {}", reset.trim())
        };
        return format!("{used} · {reset}");
    }
    let used = used_percent
        .or_else(|| remaining_percent.map(|value| 100.0 - value))
        .map(|value| format!("{value:.1}%"))
        .unwrap_or_else(|| "N/A".to_string());
    if reset.trim().is_empty() {
        used
    } else {
        format!("{used} {}", reset.trim())
    }
}

fn scalar_text(value: &Value, keys: &[&str]) -> Option<String> {
    for key in keys {
        let Some(candidate) = value.get(*key) else {
            continue;
        };
        let text = match candidate {
            Value::String(text) => text.clone(),
            Value::Number(number) => number.to_string(),
            _ => continue,
        };
        if !text.trim().is_empty() {
            return Some(text);
        }
    }
    None
}

fn quota_remaining_percent_value(value: &Value) -> Option<f64> {
    number_field(value, "remaining_percent").or_else(|| {
        let limit = number_field(value, "limit")?;
        let remaining = number_field(value, "remaining")?;
        (limit > 0.0).then_some(remaining * 100.0 / limit)
    })
}

fn gemini_model_family(value: &str) -> Option<&'static str> {
    let lower = value.to_ascii_lowercase();
    if !lower.contains("gemini") {
        return None;
    }
    if lower.contains("flash-lite") || lower.contains("flash_lite") || lower.contains("flash lite")
    {
        Some("Flash Lite")
    } else if lower.contains("flash") {
        Some("Flash")
    } else if lower.contains("pro") {
        Some("Pro")
    } else {
        None
    }
}

fn disambiguate_quota_labels(rows: &mut [QuotaRow]) {
    let mut counts = HashMap::<(String, String), usize>::new();
    for row in rows.iter_mut() {
        let key = (row.account_key.clone(), row.limit_label.clone());
        let count = counts.entry(key).or_insert(0);
        *count += 1;
        if *count > 1 {
            row.limit_label = format!("{} ({})", row.limit_label, *count);
            row.label = format!("{} / {}", row.account_label, row.limit_label);
        }
    }
}

fn collect_quota_limit_rows(
    provider: &str,
    account_key: &str,
    parent_label: &str,
    value: &Value,
    label_hint: Option<&str>,
    out: &mut Vec<QuotaRow>,
) {
    match value {
        Value::Object(map) => {
            let has_limit = map.contains_key("used_percent")
                || map.contains_key("usage_percent")
                || map.contains_key("remaining_percent")
                || map.contains_key("limit")
                || map.contains_key("remaining");
            if has_limit {
                let limit_label =
                    first_string_recursive(value, &["label", "model", "scope", "limit_name"])
                        .filter(|label| label != parent_label)
                        .or_else(|| label_hint.map(format_quota_label_hint))
                        .unwrap_or_default();
                let label = if limit_label.is_empty() {
                    parent_label.to_string()
                } else {
                    format!("{parent_label} / {limit_label}")
                };
                let used_percent_value = first_number_recursive(
                    value,
                    &["used_percent", "usage_percent", "percent_used"],
                )
                .or_else(|| quota_percent_value(value));
                let remaining_percent = quota_remaining_percent_value(value);
                let reset =
                    first_string_recursive(value, &["reset_label", "reset_at", "reset_time"])
                        .unwrap_or_default();
                out.push(QuotaRow {
                    provider: provider.to_string(),
                    account_key: account_key.to_string(),
                    account_label: parent_label.to_string(),
                    limit_label: limit_label.clone(),
                    label,
                    detail_text: quota_detail_from_parts(
                        provider,
                        used_percent_value,
                        remaining_percent,
                        &reset,
                        value,
                    ),
                    overview: true,
                    used_percent_value,
                    used_percent: percent(used_percent_value),
                    remaining_percent: percent(remaining_percent),
                    reset,
                });
            }
            for (key, child) in map {
                if matches!(child, Value::Object(_) | Value::Array(_)) {
                    let child_hint = quota_child_label_hint(label_hint, key);
                    collect_quota_limit_rows(
                        provider,
                        account_key,
                        parent_label,
                        child,
                        Some(&child_hint),
                        out,
                    );
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_quota_limit_rows(
                    provider,
                    account_key,
                    parent_label,
                    item,
                    label_hint,
                    out,
                );
            }
        }
        _ => {}
    }
}

fn format_quota_label_hint(value: &str) -> String {
    match value.trim().trim_matches('_') {
        "code_generation" => "Code Gen".to_string(),
        "code_review" => "Code Review".to_string(),
        "five_hour" | "5h" => "5h".to_string(),
        "weekly" => "Weekly".to_string(),
        "current_window" => "5h window".to_string(),
        other => other.replace(['_', '-'], " "),
    }
}

fn quota_child_label_hint(parent_hint: Option<&str>, child_key: &str) -> String {
    let child = format_quota_label_hint(child_key);
    match parent_hint {
        Some("Code Gen") => format!("Code Gen {child}"),
        Some("Code Review") => format!("Code Review {child}"),
        _ => child,
    }
}

fn key_rows_from_response(value: &Value) -> Vec<KeyRow> {
    let mut rows = value
        .get("keys")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|key| {
            let access_json = key.get("access").cloned().unwrap_or(Value::Null);
            let revoked_at = string_at(key, &["revoked_at"]);
            KeyRow {
                id: string_at(key, &["id"]),
                label: string_at(key, &["label"]),
                prefix: string_at(key, &["key_prefix"]),
                source: string_at(key, &["source"]),
                revoked: !revoked_at.is_empty(),
                revoked_at,
                created_at: string_at(key, &["created_at"]),
                last_used_at: string_at(key, &["last_used_at"]),
                access: access_summary(&access_json),
                access_json,
            }
        })
        .collect::<Vec<_>>();
    rows.sort_by(|left, right| {
        left.label
            .cmp(&right.label)
            .then_with(|| left.id.cmp(&right.id))
    });
    rows
}

fn notification_settings_value(value: &Value) -> &Value {
    value
        .get("settings")
        .filter(|settings| settings.is_object())
        .unwrap_or(value)
}

fn notification_channel(settings: &Value) -> String {
    match string_at(settings, &["channel"]).as_str() {
        "google_chat" => "google_chat".to_string(),
        _ => "telegram".to_string(),
    }
}

fn channel_label(channel: &str) -> &'static str {
    match channel {
        "google_chat" => "Google Chat",
        _ => "Telegram",
    }
}

fn notification_watched_keys(settings: &Value) -> Vec<String> {
    let mut keys = settings
        .get("watched_accounts")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::trim)
        .filter(|key| !key.is_empty())
        .map(str::to_string)
        .collect::<Vec<_>>();
    keys.sort();
    keys.dedup();
    keys
}

fn notification_settings_update_body(
    enabled: bool,
    channel: String,
    watched_accounts: Vec<String>,
) -> Value {
    json!({
        "enabled": enabled,
        "channel": channel,
        "watched_accounts": watched_accounts,
    })
}

fn notification_account_rows(value: &Value) -> Vec<NotificationAccountRow> {
    let settings = notification_settings_value(value);
    let watched = notification_watched_keys(settings)
        .into_iter()
        .collect::<HashSet<_>>();
    let mut rows = settings
        .get("accounts")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|account| {
            let key = string_at(account, &["key"]);
            if key.trim().is_empty() {
                return None;
            }
            let provider = normalize_usage_provider(&string_at(account, &["provider"]));
            let provider_label = {
                let label = string_at(account, &["provider_label"]);
                if label.trim().is_empty() {
                    provider_label(&provider).to_string()
                } else {
                    label
                }
            };
            Some(NotificationAccountRow {
                provider,
                provider_label,
                key: key.clone(),
                label: string_at(account, &["label"]),
                account_id: string_at(account, &["account_id"]),
                credential_file: string_at(account, &["credential_file"]),
                enabled: account
                    .get("enabled")
                    .and_then(Value::as_bool)
                    .unwrap_or(true),
                watched: watched.contains(&key),
            })
        })
        .collect::<Vec<_>>();
    rows.sort_by(|left, right| {
        notification_provider_rank(&left.provider)
            .cmp(&notification_provider_rank(&right.provider))
            .then_with(|| left.provider_label.cmp(&right.provider_label))
            .then_with(|| left.label.cmp(&right.label))
            .then_with(|| left.key.cmp(&right.key))
    });
    rows
}

fn notification_provider_rank(provider: &str) -> usize {
    let provider = normalize_usage_provider(provider);
    PROVIDERS
        .iter()
        .position(|spec| spec.key == provider)
        .unwrap_or(PROVIDERS.len())
}

fn prompt_notification_settings(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    settings: &Value,
) -> Result<Value, String> {
    let current_channel = notification_channel(settings);
    let channel_input = prompt_in_terminal(
        terminal,
        &format!(
            "Channel (telegram/google_chat, blank = {}): ",
            channel_label(&current_channel)
        ),
    )?;
    let channel = if channel_input.trim().is_empty() {
        current_channel
    } else {
        match channel_input.trim().to_ascii_lowercase().as_str() {
            "telegram" => "telegram".to_string(),
            "google_chat" | "google chat" | "google" => "google_chat".to_string(),
            _ => return Err("channel must be telegram or google_chat".to_string()),
        }
    };

    let mut body = notification_settings_update_body(
        bool_at(settings, &["enabled"]),
        channel.clone(),
        notification_watched_keys(settings),
    );
    if channel == "telegram" {
        let bot_token =
            prompt_in_terminal(terminal, "Telegram bot token (blank = keep current): ")?;
        let chat_id = prompt_in_terminal(
            terminal,
            &format!(
                "Telegram chat ID (blank = {}): ",
                display_or(
                    &string_at(settings, &["telegram", "chat_id"]),
                    "keep current"
                )
            ),
        )?;
        let mut telegram = json!({
            "chat_id": if chat_id.trim().is_empty() {
                string_at(settings, &["telegram", "chat_id"])
            } else {
                chat_id.trim().to_string()
            }
        });
        if !bot_token.trim().is_empty() {
            telegram["bot_token"] = json!(bot_token.trim());
        }
        body["telegram"] = telegram;
    } else {
        let webhook_url =
            prompt_in_terminal(terminal, "Google Chat webhook URL (blank = keep current): ")?;
        if !webhook_url.trim().is_empty() {
            body["google_chat"] = json!({ "webhook_url": webhook_url.trim() });
        }
    }
    Ok(body)
}

fn model_rows_from_response(value: &Value) -> Vec<ModelRow> {
    let mut rows = value
        .get("models")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|model| ModelRow {
            id: string_at(model, &["id"]),
            alias: string_at(model, &["alias"]),
            display_name: string_at(model, &["display_name"]),
            enabled: bool_at(model, &["enabled"]),
            routes: number_at(model, &["route_group_count"]),
            targets: number_at(model, &["target_count"]),
            raw: model.clone(),
        })
        .collect::<Vec<_>>();
    rows.sort_by(|left, right| left.alias.cmp(&right.alias));
    rows
}

fn events_from_response(value: &Value) -> Vec<Value> {
    value
        .get("events")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

fn usage_buckets_from_context(value: &Value) -> Vec<UsageBucket> {
    let labels = value
        .get("labels")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|label| label.as_str().unwrap_or_default().to_string())
        .collect::<Vec<_>>();

    if let Some(buckets) = value.get("buckets").and_then(Value::as_array) {
        return buckets
            .iter()
            .enumerate()
            .map(|(index, bucket)| UsageBucket {
                label: labels
                    .get(index)
                    .cloned()
                    .unwrap_or_else(|| format!("Bucket {}", index + 1)),
                input_tokens: number_at(bucket, &["input_tokens"]),
                output_tokens: number_at(bucket, &["output_tokens"]),
                total_tokens: number_at(bucket, &["total_tokens"]),
                cache_tokens: number_at(bucket, &["cache_tokens"]),
                reasoning_tokens: number_at(bucket, &["reasoning_tokens"]),
                request_count: number_at(bucket, &["request_count"]),
            })
            .collect();
    }

    let Some(models) = value.get("models").and_then(Value::as_object) else {
        return Vec::new();
    };
    let len = models
        .values()
        .filter_map(Value::as_array)
        .map(Vec::len)
        .max()
        .unwrap_or(0);
    let mut out = (0..len)
        .map(|index| UsageBucket {
            label: labels
                .get(index)
                .cloned()
                .unwrap_or_else(|| format!("Bucket {}", index + 1)),
            ..UsageBucket::default()
        })
        .collect::<Vec<_>>();
    for rows in models.values().filter_map(Value::as_array) {
        for (index, row) in rows.iter().enumerate() {
            if let Some(bucket) = out.get_mut(index) {
                bucket.input_tokens += number_at(row, &["input"]);
                bucket.output_tokens += number_at(row, &["output"]);
                bucket.cache_tokens += number_at(row, &["cache"]);
                bucket.reasoning_tokens += number_at(row, &["reasoning"]);
                bucket.total_tokens = bucket.input_tokens
                    + bucket.output_tokens
                    + bucket.cache_tokens
                    + bucket.reasoning_tokens;
            }
        }
    }
    out
}

fn active_account_count(accounts: &[AccountRow]) -> usize {
    accounts
        .iter()
        .filter(|account| account.requests > 0 || account.total_tokens > 0)
        .count()
}

fn account_display(account: &AccountRow) -> String {
    [
        account.label.as_str(),
        account.account_id.as_str(),
        account.file_name.as_str(),
        account.key.as_str(),
    ]
    .into_iter()
    .map(str::trim)
    .find(|value| !value.is_empty())
    .unwrap_or("account")
    .to_string()
}

fn usage_account_filter_key(account: &AccountRow) -> String {
    account_row_identity_keys(account)
        .into_iter()
        .next()
        .unwrap_or_else(|| account_display(account))
}

struct AccountQuotaUsageGroup {
    provider: String,
    accounts: Vec<AccountQuotaUsageRow>,
}

struct AccountQuotaUsageRow {
    label: String,
    keys: Vec<String>,
    percent: Option<f64>,
    requests: u64,
}

struct OverviewAccountUsageGroup {
    provider: String,
    accounts: Vec<OverviewAccountUsageRow>,
}

#[derive(Clone)]
struct OverviewAccountUsageRow {
    provider: String,
    account_key: String,
    account_index: usize,
    account_label: String,
    limit_label: String,
    label: String,
    detail_text: Option<String>,
    percent: Option<f64>,
    remaining_percent: String,
    reset: String,
    requests: u64,
    enabled: bool,
    cooldown: bool,
    attention: bool,
    priority: bool,
}

struct QuotaUsageSource {
    label: String,
    keys: Vec<String>,
    percent: Option<f64>,
}

fn overview_account_usage_groups(app: &TuiApp) -> Vec<OverviewAccountUsageGroup> {
    let groups = if app.limit_display_mode == LimitDisplayMode::AllPerAccount {
        overview_account_usage_limit_groups(app)
    } else {
        overview_account_usage_account_groups(app)
    };

    let groups = filter_overview_account_usage_groups(app, groups);
    order_overview_account_usage_groups(app, groups)
}

fn overview_account_usage_account_groups(app: &TuiApp) -> Vec<OverviewAccountUsageGroup> {
    let mut quota_by_provider = HashMap::<String, Vec<AccountQuotaUsageRow>>::new();
    for group in account_quota_usage_groups(&app.data.snapshot) {
        quota_by_provider.insert(group.provider, group.accounts);
    }

    let mut groups = Vec::new();
    let mut group_indexes = HashMap::<String, usize>::new();
    for (account_index, account) in app.data.accounts.iter().enumerate() {
        let provider = account.provider.clone();
        let quota_rows = quota_by_provider
            .get(&provider)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        let group_index = *group_indexes.entry(provider.clone()).or_insert_with(|| {
            let index = groups.len();
            groups.push(OverviewAccountUsageGroup {
                provider: provider.clone(),
                accounts: Vec::new(),
            });
            index
        });
        let account_key = usage_account_filter_key(account);
        groups[group_index].accounts.push(OverviewAccountUsageRow {
            provider,
            account_key,
            account_index,
            account_label: account_display(account),
            limit_label: String::new(),
            label: account_display(account),
            detail_text: None,
            percent: overview_account_percent(account, quota_rows),
            remaining_percent: String::new(),
            reset: String::new(),
            requests: account.requests,
            enabled: account.enabled,
            cooldown: account.cooldown,
            attention: account_has_attention_marker(account),
            priority: account.priority,
        });
    }

    groups
}

fn filter_overview_account_usage_groups(
    app: &TuiApp,
    groups: Vec<OverviewAccountUsageGroup>,
) -> Vec<OverviewAccountUsageGroup> {
    groups
        .into_iter()
        .filter_map(|mut group| {
            if app.hidden_usage_providers.contains(&group.provider) {
                return None;
            }
            group.accounts.retain(|row| {
                !app.hidden_usage_accounts
                    .contains(&(row.provider.clone(), row.account_key.clone()))
            });
            if group.accounts.is_empty() {
                None
            } else {
                Some(group)
            }
        })
        .collect()
}

fn order_overview_account_usage_groups(
    app: &TuiApp,
    mut groups: Vec<OverviewAccountUsageGroup>,
) -> Vec<OverviewAccountUsageGroup> {
    for group in &mut groups {
        let Some(order) = app.account_orders.get(&group.provider) else {
            continue;
        };
        let ranks = order
            .iter()
            .enumerate()
            .map(|(index, key)| (key.as_str(), index))
            .collect::<HashMap<_, _>>();
        group.accounts.sort_by_key(|row| {
            ranks
                .get(row.account_key.as_str())
                .copied()
                .unwrap_or(usize::MAX)
        });
    }
    let provider_ranks = app
        .provider_order
        .iter()
        .enumerate()
        .map(|(index, provider)| (provider.as_str(), index))
        .collect::<HashMap<_, _>>();
    groups.sort_by_key(|group| {
        provider_ranks
            .get(group.provider.as_str())
            .copied()
            .unwrap_or(usize::MAX)
    });
    groups
}

fn provider_order_for_app(app: &TuiApp) -> Vec<String> {
    let known = app
        .data
        .accounts
        .iter()
        .map(|account| account.provider.clone())
        .collect::<Vec<_>>();
    let known_keys = known.iter().cloned().collect::<HashSet<_>>();
    let mut seen = HashSet::new();
    let mut order = app.provider_order.clone();
    order.retain(|provider| {
        !provider.is_empty() && known_keys.contains(provider) && seen.insert(provider.clone())
    });
    for provider in known {
        if seen.insert(provider.clone()) {
            order.push(provider);
        }
    }
    order
}

fn account_order_for_provider(app: &TuiApp, provider: &str) -> Vec<String> {
    let known = app
        .data
        .accounts
        .iter()
        .filter(|account| account.provider == provider)
        .map(usage_account_filter_key)
        .collect::<Vec<_>>();
    let known_keys = known.iter().cloned().collect::<HashSet<_>>();
    let mut seen = HashSet::new();
    let mut order = app
        .account_orders
        .get(provider)
        .cloned()
        .unwrap_or_default();
    order.retain(|key| !key.is_empty() && known_keys.contains(key) && seen.insert(key.clone()));
    for key in known {
        if seen.insert(key.clone()) {
            order.push(key);
        }
    }
    order
}

fn overview_account_usage_account_rows(app: &TuiApp) -> Vec<OverviewAccountUsageRow> {
    let mut rows = Vec::new();
    let mut seen = HashSet::new();
    for group in overview_account_usage_groups(app) {
        for row in group.accounts {
            if seen.insert(row.account_index) {
                rows.push(row);
            }
        }
    }
    rows
}

fn overview_account_usage_account_count(app: &TuiApp) -> usize {
    overview_account_usage_account_rows(app).len()
}

fn selected_overview_account_index(app: &TuiApp) -> Option<usize> {
    selected_overview_usage_row(app).map(|row| row.account_index)
}

fn selected_overview_usage_row(app: &TuiApp) -> Option<OverviewAccountUsageRow> {
    overview_account_usage_account_rows(app)
        .into_iter()
        .nth(app.overview_account_selected)
}

fn overview_selection_for_account(app: &TuiApp, account_index: usize) -> Option<usize> {
    overview_account_usage_account_rows(app)
        .into_iter()
        .position(|row| row.account_index == account_index)
}

fn overview_account_usage_limit_groups(app: &TuiApp) -> Vec<OverviewAccountUsageGroup> {
    let mut groups = Vec::new();
    let mut group_indexes = HashMap::<String, usize>::new();

    for (account_index, account) in app.data.accounts.iter().enumerate() {
        let provider = account.provider.clone();
        let group_index = *group_indexes.entry(provider.clone()).or_insert_with(|| {
            let index = groups.len();
            groups.push(OverviewAccountUsageGroup {
                provider: provider.clone(),
                accounts: Vec::new(),
            });
            index
        });

        let mut matches = app
            .data
            .quotas
            .iter()
            .filter(|quota| {
                quota.overview
                    && quota.provider == provider
                    && quota_matches_account(quota, account)
            })
            .peekable();
        if matches.peek().is_none() {
            let account_key = usage_account_filter_key(account);
            groups[group_index].accounts.push(OverviewAccountUsageRow {
                provider,
                account_key,
                account_index,
                account_label: account_display(account),
                limit_label: String::new(),
                label: account_display(account),
                detail_text: None,
                percent: None,
                remaining_percent: String::new(),
                reset: String::new(),
                requests: account.requests,
                enabled: account.enabled,
                cooldown: account.cooldown,
                attention: account_has_attention_marker(account),
                priority: account.priority,
            });
            continue;
        }

        let account_key = usage_account_filter_key(account);
        for quota in matches {
            groups[group_index].accounts.push(OverviewAccountUsageRow {
                provider: provider.clone(),
                account_key: account_key.clone(),
                account_index,
                account_label: account_display(account),
                limit_label: quota.limit_label.clone(),
                label: account_usage_limit_label(account, quota),
                detail_text: Some(quota.detail_text.clone()),
                percent: quota.used_percent_value,
                remaining_percent: quota.remaining_percent.clone(),
                reset: quota.reset.clone(),
                requests: account.requests,
                enabled: account.enabled,
                cooldown: account.cooldown,
                attention: account_has_attention_marker(account),
                priority: account.priority,
            });
        }
    }

    groups
}

fn account_usage_limit_label(account: &AccountRow, quota: &QuotaRow) -> String {
    let account_label = account_display(account);
    if quota.limit_label.trim().is_empty() {
        account_label
    } else {
        format!("{account_label} / {}", quota.limit_label)
    }
}

fn quota_matches_account(quota: &QuotaRow, account: &AccountRow) -> bool {
    let account_keys = normalized_match_keys(&account_row_identity_keys(account));
    let quota_keys = normalized_match_keys(&[
        quota.account_key.clone(),
        quota.account_label.clone(),
        quota.label.clone(),
    ]);

    if quota_keys.iter().any(|key| account_keys.contains(key)) {
        return true;
    }

    if quota_keys.iter().any(|quota_key| {
        account_keys
            .iter()
            .any(|account_key| fuzzy_key_match(quota_key, account_key))
    }) {
        return true;
    }

    normalize_match_key(&quota.account_label) == normalize_match_key(&account_display(account))
}

fn overview_account_percent(
    account: &AccountRow,
    quota_rows: &[AccountQuotaUsageRow],
) -> Option<f64> {
    let account_keys = normalized_match_keys(&account_row_identity_keys(account));
    let exact = quota_rows.iter().find(|quota| {
        let quota_keys = normalized_match_keys(&quota.keys);
        quota_keys.iter().any(|key| account_keys.contains(key))
    });
    if let Some(quota) = exact {
        return quota.percent;
    }

    let fuzzy = quota_rows.iter().find(|quota| {
        let quota_keys = normalized_match_keys(&quota.keys);
        quota_keys.iter().any(|quota_key| {
            account_keys
                .iter()
                .any(|account_key| fuzzy_key_match(quota_key, account_key))
        })
    });
    if let Some(quota) = fuzzy {
        return quota.percent;
    }

    let account_label = normalize_match_key(&account_display(account));
    quota_rows
        .iter()
        .find(|quota| normalize_match_key(&quota.label) == account_label)
        .and_then(|quota| quota.percent)
}

fn account_row_identity_keys(account: &AccountRow) -> Vec<String> {
    unique_non_empty_strings([
        account.key.clone(),
        account.label.clone(),
        account.account_id.clone(),
        account.file_name.clone(),
        account_display(account),
    ])
}

fn account_quota_usage_groups(snapshot: &Value) -> Vec<AccountQuotaUsageGroup> {
    let mut groups = Vec::new();
    for spec in PROVIDERS {
        let provider = normalize_usage_provider(spec.key);
        let accounts = snapshot_accounts_for_provider(snapshot, &provider);
        let quota_sources = snapshot_quota_sources_for_provider(snapshot, &provider);
        if accounts.is_empty() && quota_sources.is_empty() {
            continue;
        }

        let mut used_quotas = HashSet::new();
        let mut rows = accounts
            .into_iter()
            .map(|account| {
                let keys = account_identity_keys(account);
                let quota_index = best_quota_source_index(&keys, &quota_sources, &used_quotas);
                if let Some(index) = quota_index {
                    used_quotas.insert(index);
                }
                let quota = quota_index.and_then(|index| quota_sources.get(index));
                let mut row_keys = keys;
                if let Some(quota) = quota {
                    row_keys.extend(quota.keys.clone());
                }
                AccountQuotaUsageRow {
                    label: account_display_name(account)
                        .or_else(|| quota.map(|quota| quota.label.clone()))
                        .unwrap_or_else(|| "Account".to_string()),
                    keys: unique_non_empty_strings(row_keys),
                    percent: quota.and_then(|quota| quota.percent),
                    requests: number_at(account, &["requests"]),
                }
            })
            .collect::<Vec<_>>();

        for (index, quota) in quota_sources.into_iter().enumerate() {
            if used_quotas.contains(&index) {
                continue;
            }
            rows.push(AccountQuotaUsageRow {
                label: quota.label,
                keys: quota.keys,
                percent: quota.percent,
                requests: 0,
            });
        }

        rows.sort_by(|left, right| {
            right
                .percent
                .partial_cmp(&left.percent)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| right.requests.cmp(&left.requests))
                .then_with(|| left.label.cmp(&right.label))
        });
        groups.push(AccountQuotaUsageGroup {
            provider,
            accounts: rows,
        });
    }
    groups
}

fn snapshot_accounts_for_provider<'a>(snapshot: &'a Value, provider: &str) -> Vec<&'a Value> {
    if provider == "codex" {
        return snapshot
            .get("dashboard")
            .and_then(|dashboard| dashboard.get("accounts"))
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .collect();
    }

    let Some(providers) = snapshot.get("providers").and_then(Value::as_object) else {
        return Vec::new();
    };
    for key in snapshot_provider_keys(provider) {
        if let Some(accounts) = providers
            .get(key)
            .and_then(|payload| payload.get("accounts"))
            .and_then(Value::as_array)
        {
            return accounts.iter().collect();
        }
    }
    Vec::new()
}

fn snapshot_quota_sources_for_provider(snapshot: &Value, provider: &str) -> Vec<QuotaUsageSource> {
    let Some(quotas) = snapshot.get("quotas").and_then(Value::as_object) else {
        return Vec::new();
    };
    for key in snapshot_provider_keys(provider) {
        if let Some(payload) = quotas.get(key) {
            return quota_account_values(payload)
                .into_iter()
                .map(|account| QuotaUsageSource {
                    label: account_display_name(account).unwrap_or_else(|| "Account".to_string()),
                    keys: account_identity_keys(account),
                    percent: quota_account_percent(provider, account),
                })
                .collect();
        }
    }
    Vec::new()
}

fn snapshot_provider_keys(provider: &str) -> Vec<&str> {
    if provider == "antigravity" {
        vec!["agw", "antigravity"]
    } else {
        vec![provider]
    }
}

fn quota_account_values(value: &Value) -> Vec<&Value> {
    if let Some(accounts) = value.get("accounts").and_then(Value::as_array) {
        return accounts.iter().collect();
    }
    if let Some(account) = value.get("account") {
        return vec![account];
    }
    value.as_array().into_iter().flatten().collect()
}

fn best_quota_source_index(
    keys: &[String],
    quotas: &[QuotaUsageSource],
    used: &HashSet<usize>,
) -> Option<usize> {
    let account_keys = normalized_match_keys(keys);
    let available = quotas
        .iter()
        .enumerate()
        .filter(|(index, _)| !used.contains(index))
        .collect::<Vec<_>>();
    let exact = available.iter().find(|(_, quota)| {
        let quota_keys = normalized_match_keys(&quota.keys);
        quota_keys.iter().any(|key| account_keys.contains(key))
    });
    if let Some((index, _)) = exact {
        return Some(*index);
    }

    let fuzzy = available.iter().find(|(_, quota)| {
        let quota_keys = normalized_match_keys(&quota.keys);
        quota_keys.iter().any(|quota_key| {
            account_keys
                .iter()
                .any(|account_key| fuzzy_key_match(quota_key, account_key))
        })
    });
    if let Some((index, _)) = fuzzy {
        return Some(*index);
    }

    if available.len() == 1 {
        available.first().map(|(index, _)| *index)
    } else {
        None
    }
}

fn account_identity_keys(value: &Value) -> Vec<String> {
    unique_non_empty_strings([
        string_at(value, &["key"]),
        string_at(value, &["file_name"]),
        string_at(value, &["label"]),
        string_at(value, &["email"]),
        string_at(value, &["login"]),
        string_at(value, &["organization_uuid"]),
        string_at(value, &["account_id"]),
        string_at(value, &["name"]),
    ])
}

fn account_display_name(value: &Value) -> Option<String> {
    [
        string_at(value, &["label"]),
        string_at(value, &["email"]),
        string_at(value, &["login"]),
        string_at(value, &["name"]),
        string_at(value, &["account_id"]),
        string_at(value, &["organization_uuid"]),
        string_at(value, &["file_name"]),
    ]
    .into_iter()
    .map(|value| value.trim().to_string())
    .find(|value| !value.is_empty())
}

fn unique_non_empty_strings(values: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut seen = HashSet::new();
    values
        .into_iter()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty() && seen.insert(value.clone()))
        .collect()
}

fn normalized_match_keys(values: &[String]) -> Vec<String> {
    unique_non_empty_strings(
        values
            .iter()
            .map(|value| normalize_match_key(value))
            .filter(|value| value.len() >= 3),
    )
}

fn normalize_match_key(value: &str) -> String {
    value
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn fuzzy_key_match(left: &str, right: &str) -> bool {
    left.len() >= 6 && right.len() >= 6 && (left.contains(right) || right.contains(left))
}

fn quota_account_percent(provider: &str, account: &Value) -> Option<f64> {
    let mut values = Vec::new();
    collect_quota_percent_values(provider, account, &mut values);
    values
        .into_iter()
        .filter(|value| value.is_finite())
        .max_by(|left, right| left.partial_cmp(right).unwrap_or(std::cmp::Ordering::Equal))
}

fn collect_quota_percent_values(provider: &str, quota: &Value, out: &mut Vec<f64>) {
    push_progress_pair(out, quota.get("code_generation"));
    push_progress_pair(out, quota.get("code_review"));

    for limit in array_values(quota.get("additional_rate_limits")) {
        push_progress_pair(out, Some(limit));
    }

    let hide_provider_models = provider == "antigravity" || provider == "gemini";
    if !hide_provider_models {
        for model in array_values(quota.get("models")) {
            if let Some(bucket) = model_quota_bucket(model) {
                push_quota_bucket_percent(out, bucket);
            }
        }
    }

    if provider == "gemini" {
        for model in array_values(quota.get("models")) {
            if let Some(bucket) = model_quota_bucket(model) {
                push_quota_bucket_percent(out, bucket);
            }
        }
    }

    for group in array_values(quota.get("groups")) {
        push_progress_pair(out, Some(group));
    }
    for limit in array_values(quota.get("limits")) {
        push_quota_bucket_percent(out, limit);
    }
    if let Some(current_window) = quota.get("current_window") {
        push_quota_bucket_percent(out, current_window);
    }
    if let Some(weekly) = quota.get("weekly") {
        push_quota_bucket_percent(out, weekly);
    }
    if provider == "grok" {
        collect_grok_quota_percent_values(quota, out);
    }
    for balance in array_values(quota.get("balances")) {
        if balance
            .get("total_balance")
            .is_some_and(|value| !value.is_null())
        {
            out.push(100.0);
        }
    }

    if out.is_empty() {
        collect_direct_quota_percent_values(quota, out);
    }
}

fn push_progress_pair(out: &mut Vec<f64>, value: Option<&Value>) {
    let Some(value) = value else {
        return;
    };
    if let Some(five_hour) = value.get("five_hour") {
        push_quota_bucket_percent(out, five_hour);
    }
    if let Some(weekly) = value.get("weekly") {
        push_quota_bucket_percent(out, weekly);
    }
}

fn push_quota_bucket_percent(out: &mut Vec<f64>, value: &Value) {
    if let Some(percent) = quota_percent_value(value) {
        out.push(percent);
    }
}

fn quota_percent_value(value: &Value) -> Option<f64> {
    let direct = number_field(value, "used_percent")
        .or_else(|| number_field(value, "usage_percent"))
        .or_else(|| number_field(value, "percent_used"))
        .or_else(|| number_field(value, "usedPercent"));
    if direct.is_some() {
        return direct;
    }

    if let Some(remaining_percent) = number_field(value, "remaining_percent") {
        return Some(100.0 - remaining_percent);
    }

    let limit = number_field(value, "limit");
    let remaining = number_field(value, "remaining");
    match (limit, remaining) {
        (Some(limit), Some(remaining)) if limit > 0.0 => {
            Some((100.0 * (limit - remaining)) / limit)
        }
        _ => None,
    }
}

fn number_field(value: &Value, key: &str) -> Option<f64> {
    let value = value.get(key)?;
    value
        .as_f64()
        .or_else(|| value.as_str().and_then(|text| text.trim().parse().ok()))
}

fn array_values(value: Option<&Value>) -> Vec<&Value> {
    value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .collect()
}

fn model_quota_bucket(model: &Value) -> Option<&Value> {
    model
        .get("current")
        .or_else(|| model.get("quota"))
        .or_else(|| model.get("limit"))
}

fn collect_grok_quota_percent_values(quota: &Value, out: &mut Vec<f64>) {
    let Some(kinds) = quota.get("kinds") else {
        return;
    };
    for (kind, include_tokens) in [
        ("DEFAULT_TEXT", true),
        ("DEFAULT_IMAGE", false),
        ("DEFAULT_VIDEO", false),
    ] {
        let Some(rate_limits) = kinds.get(kind).and_then(|kind| kind.get("rate_limits")) else {
            continue;
        };
        if let Some(requests) = rate_limits.get("requests") {
            push_quota_bucket_percent(out, requests);
        }
        if include_tokens {
            if let Some(tokens) = rate_limits.get("tokens") {
                push_quota_bucket_percent(out, tokens);
            }
        }
    }
}

fn collect_direct_quota_percent_values(value: &Value, out: &mut Vec<f64>) {
    match value {
        Value::Object(map) => {
            if let Some(percent) = quota_percent_value(value) {
                out.push(percent);
            }
            for child in map.values() {
                if matches!(child, Value::Object(_) | Value::Array(_)) {
                    collect_direct_quota_percent_values(child, out);
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_direct_quota_percent_values(item, out);
            }
        }
        _ => {}
    }
}

fn overview_chart_footer(buckets: &[UsageBucket]) -> String {
    let first = buckets
        .iter()
        .find(|bucket| bucket.total_tokens > 0 || bucket.request_count > 0)
        .or_else(|| buckets.first());
    let last = buckets
        .iter()
        .rev()
        .find(|bucket| bucket.total_tokens > 0 || bucket.request_count > 0)
        .or_else(|| buckets.last());
    match (first, last) {
        (Some(first), Some(last)) if !first.label.is_empty() && !last.label.is_empty() => {
            format!("{} -> {}", first.label, last.label)
        }
        _ => "no bucket labels".to_string(),
    }
}

fn percent_bar(percent: f64, width: usize) -> String {
    let width = width.max(1);
    let clamped = percent.clamp(0.0, 100.0);
    let filled = ((clamped / 100.0) * width as f64).round() as usize;
    format!("{}{}", "#".repeat(filled), ".".repeat(width - filled))
}

fn quota_usage_color(percent: f64) -> Color {
    if percent >= 90.0 {
        BTOP_USED_END
    } else if percent >= 70.0 {
        BTOP_CPU_MID
    } else {
        BTOP_CPU_START
    }
}

fn truncate_text(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_string();
    }
    if max_chars == 0 {
        return String::new();
    }
    if max_chars == 1 {
        return "~".to_string();
    }

    let mut text = value.chars().take(max_chars - 1).collect::<String>();
    text.push('~');
    text
}

fn error_rate(requests: u64, errors: u64) -> String {
    if requests == 0 {
        return "0.0%".to_string();
    }
    format!("{:.1}%", (errors as f64 / requests as f64) * 100.0)
}

fn format_count(value: u64) -> String {
    let raw = value.to_string();
    let mut out = String::new();
    for (index, ch) in raw.chars().rev().enumerate() {
        if index > 0 && index % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out.chars().rev().collect()
}

fn format_short(value: u64) -> String {
    const UNITS: &[(u64, &str)] = &[
        (1_000_000_000_000, "T"),
        (1_000_000_000, "B"),
        (1_000_000, "M"),
        (1_000, "K"),
    ];
    for (scale, suffix) in UNITS {
        if value >= *scale {
            let scaled = value as f64 / *scale as f64;
            if scaled >= 10.0 {
                return format!("{scaled:.0}{suffix}");
            }
            return format!("{scaled:.1}{suffix}");
        }
    }
    value.to_string()
}

fn find_account(
    accounts: &[AccountRow],
    target: &str,
    provider: Option<&str>,
) -> Result<AccountRow, String> {
    let target = target.trim();
    let provider = provider.map(normalize_usage_provider);
    let matches = accounts
        .iter()
        .filter(|account| {
            provider
                .as_ref()
                .map(|provider| &account.provider == provider)
                .unwrap_or(true)
                && [
                    account.file_name.as_str(),
                    account.key.as_str(),
                    account.label.as_str(),
                    account.account_id.as_str(),
                ]
                .iter()
                .any(|value| value.eq_ignore_ascii_case(target))
        })
        .cloned()
        .collect::<Vec<_>>();
    match matches.len() {
        0 => Err(format!("no account matched '{target}'")),
        1 => Ok(matches[0].clone()),
        _ => Err(format!(
            "multiple accounts matched '{target}'; pass --provider or use the file name"
        )),
    }
}

fn ensure_success(response: &ApiResponse) -> Result<(), String> {
    if response.status.is_success() {
        return Ok(());
    }
    Err(format!(
        "HTTP {}: {}",
        response.status.as_u16(),
        message_from(&response.body)
    ))
}

fn print_response(json_output: bool, value: Value) -> Result<(), String> {
    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&value).map_err(|err| err.to_string())?
        );
    } else {
        match &value {
            Value::Object(map) if map.contains_key("message") => {
                println!("{}", message_from(&value))
            }
            Value::String(text) => println!("{text}"),
            _ => println!(
                "{}",
                serde_json::to_string_pretty(&value).map_err(|err| err.to_string())?
            ),
        }
    }
    Ok(())
}

fn print_accounts(accounts: &[AccountRow]) {
    print_table(
        &[
            "Provider", "State", "Priority", "Label", "File", "Requests", "Errors", "Key",
        ],
        accounts.iter().map(|account| {
            vec![
                account.provider_label.clone(),
                if account_has_attention_marker(account) {
                    "●!".to_string()
                } else {
                    "●".to_string()
                },
                if account.priority { "first" } else { "" }.to_string(),
                account.label.clone(),
                account.file_name.clone(),
                account.requests.to_string(),
                account.errors.to_string(),
                account.key.clone(),
            ]
        }),
    );
}

fn quota_rows_for_mode(rows: &[QuotaRow], one_limit: bool) -> Vec<QuotaRow> {
    if one_limit {
        compact_quota_rows(rows)
    } else {
        rows.to_vec()
    }
}

fn compact_quota_rows(rows: &[QuotaRow]) -> Vec<QuotaRow> {
    let mut compact = Vec::<QuotaRow>::new();
    let mut indexes = HashMap::<(String, String), usize>::new();

    for row in rows {
        let key = (row.provider.clone(), row.account_key.clone());
        if let Some(index) = indexes.get(&key).copied() {
            if quota_row_is_more_constrained(row, &compact[index]) {
                compact[index] = row.clone();
            }
        } else {
            indexes.insert(key, compact.len());
            compact.push(row.clone());
        }
    }

    compact
}

fn quota_row_is_more_constrained(candidate: &QuotaRow, current: &QuotaRow) -> bool {
    match (candidate.used_percent_value, current.used_percent_value) {
        (Some(candidate), Some(current)) => candidate > current,
        (Some(_), None) => true,
        (None, Some(_)) => false,
        (None, None) => candidate.label < current.label,
    }
}

fn print_quota(rows: &[QuotaRow]) {
    print_table(
        &["Provider", "Account/limit", "Used", "Remaining", "Reset"],
        rows.iter().map(|row| {
            vec![
                provider_label(&row.provider).to_string(),
                row.label.clone(),
                row.used_percent.clone(),
                row.remaining_percent.clone(),
                row.reset.clone(),
            ]
        }),
    );
}

fn print_keys(keys: &[KeyRow]) {
    print_table(
        &[
            "Label",
            "Prefix",
            "State",
            "Source",
            "Access",
            "Created",
            "Last used",
            "Id",
        ],
        keys.iter().map(|key| {
            vec![
                key.label.clone(),
                key.prefix.clone(),
                if key.revoked { "revoked" } else { "active" }.to_string(),
                key.source.clone(),
                key.access.clone(),
                key.created_at.clone(),
                key.last_used_at.clone(),
                key.id.clone(),
            ]
        }),
    );
}

fn print_models(models: &[ModelRow]) {
    print_table(
        &["State", "Alias", "Name", "Routes", "Targets", "Id"],
        models.iter().map(|model| {
            vec![
                if model.enabled { "on" } else { "off" }.to_string(),
                model.alias.clone(),
                model.display_name.clone(),
                model.routes.to_string(),
                model.targets.to_string(),
                model.id.clone(),
            ]
        }),
    );
}

fn print_usage_summary(value: &Value) {
    let totals = value.get("totals").unwrap_or(&Value::Null);
    println!("Requests:      {}", number_at(totals, &["requests"]));
    println!("Errors:        {}", number_at(totals, &["errors"]));
    println!("Input tokens:  {}", number_at(totals, &["input_tokens"]));
    println!("Output tokens: {}", number_at(totals, &["output_tokens"]));
    println!("Total tokens:  {}", number_at(totals, &["total_tokens"]));
    println!("Cache tokens:  {}", number_at(totals, &["cache_tokens"]));
    println!(
        "Reasoning:     {}",
        number_at(totals, &["reasoning_tokens"])
    );
    println!(
        "First seen:    {}",
        string_at(totals, &["first_recorded_at"])
    );
    println!(
        "Last seen:     {}",
        string_at(totals, &["last_recorded_at"])
    );
}

fn print_history(events: &[Value]) {
    print_table(
        &["Time", "Provider", "Model", "Result", "Tokens", "Message"],
        events.iter().map(|event| {
            vec![
                string_at(event, &["recorded_at"]),
                string_at(event, &["provider"]),
                string_at(event, &["model"]),
                if bool_at(event, &["success"]) {
                    "ok"
                } else {
                    "err"
                }
                .to_string(),
                number_at(event, &["total_tokens"]).to_string(),
                short_message(&string_at(event, &["error_message"])),
            ]
        }),
    );
}

fn print_table<I>(headers: &[&str], rows: I)
where
    I: IntoIterator<Item = Vec<String>>,
{
    let rows = rows.into_iter().collect::<Vec<_>>();
    let mut widths = headers
        .iter()
        .map(|header| header.len())
        .collect::<Vec<_>>();
    for row in &rows {
        for (index, cell) in row.iter().enumerate() {
            if let Some(width) = widths.get_mut(index) {
                *width = (*width).max(cell.chars().count().min(42));
            }
        }
    }
    for (index, header) in headers.iter().enumerate() {
        print!("{:width$}  ", header, width = widths[index]);
    }
    println!();
    for width in &widths {
        print!("{:-<width$}--", "", width = *width);
    }
    println!();
    for row in rows {
        for (index, cell) in row.iter().enumerate() {
            let value = truncate(cell, widths[index]);
            print!("{:width$}  ", value, width = widths[index]);
        }
        println!();
    }
}

fn accounts_json(accounts: &[AccountRow]) -> Value {
    Value::Array(
        accounts
            .iter()
            .map(|account| {
                json!({
                    "provider": account.provider,
                    "provider_label": account.provider_label,
                    "key": account.key,
                    "label": account.label,
                    "account_id": account.account_id,
                    "file_name": account.file_name,
                    "enabled": account.enabled,
                    "priority": account.priority,
                    "requests": account.requests,
                    "errors": account.errors,
                    "total_tokens": account.total_tokens,
                    "last_success_at": account.last_success_at,
                    "last_error_at": account.last_error_at,
                    "last_error_message": account.last_error_message
                })
            })
            .collect(),
    )
}

fn quota_json(rows: &[QuotaRow]) -> Value {
    Value::Array(
        rows.iter()
            .map(|row| {
                json!({
                    "provider": row.provider,
                    "account_key": row.account_key,
                    "account_label": row.account_label,
                    "limit_label": row.limit_label,
                    "label": row.label,
                    "detail_text": row.detail_text,
                    "overview": row.overview,
                    "used_percent_value": row.used_percent_value,
                    "used_percent": row.used_percent,
                    "remaining_percent": row.remaining_percent,
                    "reset": row.reset
                })
            })
            .collect(),
    )
}

fn parse_json_arg(raw: &str) -> Result<Value, String> {
    let source = if let Some(path) = raw.strip_prefix('@') {
        fs::read_to_string(path).map_err(|err| err.to_string())?
    } else if Path::new(raw).exists() {
        fs::read_to_string(raw).map_err(|err| err.to_string())?
    } else {
        raw.to_string()
    };
    serde_json::from_str(&source).map_err(|err| err.to_string())
}

fn prompt_line(prompt: &str) -> io::Result<String> {
    print!("{prompt}");
    io::stdout().flush()?;
    let mut value = String::new();
    io::stdin().read_line(&mut value)?;
    Ok(value.trim().to_string())
}

fn session_path(base_url: &str) -> Result<PathBuf, String> {
    gateway_config_path(base_url, "session")
}

fn preferences_path(base_url: &str) -> Result<PathBuf, String> {
    gateway_config_path(base_url, "preferences.json")
}

fn gateway_config_path(base_url: &str, extension: &str) -> Result<PathBuf, String> {
    let dir = config_dir().join("iogw");
    let mut hasher = Sha256::new();
    hasher.update(base_url.as_bytes());
    let hash = hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    Ok(dir.join(format!("{hash}.{extension}")))
}

fn load_tui_preferences(path: &Path) -> TuiPreferenceState {
    let Ok(data) = fs::read_to_string(path) else {
        return TuiPreferenceState::default();
    };
    let Ok(preferences) = serde_json::from_str::<TuiPreferences>(&data) else {
        return TuiPreferenceState::default();
    };

    let account_orders = preferences
        .account_orders
        .into_iter()
        .filter_map(|entry| {
            let provider = normalize_usage_provider(&entry.provider);
            let mut seen = HashSet::new();
            let account_keys = entry
                .account_keys
                .into_iter()
                .filter(|key| !key.is_empty() && seen.insert(key.clone()))
                .collect::<Vec<_>>();
            if provider.is_empty() || account_keys.is_empty() {
                None
            } else {
                Some((provider, account_keys))
            }
        })
        .collect();
    let mut seen_providers = HashSet::new();
    let provider_order = preferences
        .provider_order
        .into_iter()
        .map(|provider| normalize_usage_provider(&provider))
        .filter(|provider| !provider.is_empty() && seen_providers.insert(provider.clone()))
        .collect();

    TuiPreferenceState {
        limit_display_mode: preferences
            .limit_display_mode
            .as_deref()
            .map(LimitDisplayMode::from_preference)
            .unwrap_or(LimitDisplayMode::OnePerAccount),
        hidden_usage_providers: preferences
            .hidden_usage_providers
            .into_iter()
            .map(|provider| normalize_usage_provider(&provider))
            .filter(|provider| !provider.is_empty())
            .collect(),
        hidden_usage_accounts: preferences
            .hidden_usage_accounts
            .into_iter()
            .map(|account| {
                (
                    normalize_usage_provider(&account.provider),
                    account.account_key,
                )
            })
            .filter(|(provider, account_key)| !provider.is_empty() && !account_key.is_empty())
            .collect(),
        account_orders,
        provider_order,
    }
}

fn save_tui_preferences(path: &Path, app: &TuiApp) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|err| err.to_string())?;
    }

    let mut hidden_usage_providers = app
        .hidden_usage_providers
        .iter()
        .cloned()
        .collect::<Vec<_>>();
    hidden_usage_providers.sort();

    let mut hidden_usage_accounts = app
        .hidden_usage_accounts
        .iter()
        .map(|(provider, account_key)| HiddenUsageAccountPreference {
            provider: provider.clone(),
            account_key: account_key.clone(),
        })
        .collect::<Vec<_>>();
    hidden_usage_accounts.sort_by(|left, right| {
        left.provider
            .cmp(&right.provider)
            .then_with(|| left.account_key.cmp(&right.account_key))
    });

    let mut account_orders = app
        .account_orders
        .iter()
        .map(|(provider, account_keys)| {
            let mut seen = HashSet::new();
            let account_keys = account_keys
                .iter()
                .filter(|key| !key.is_empty() && seen.insert((*key).clone()))
                .cloned()
                .collect::<Vec<_>>();
            AccountOrderPreference {
                provider: provider.clone(),
                account_keys,
            }
        })
        .filter(|entry| !entry.account_keys.is_empty())
        .collect::<Vec<_>>();
    account_orders.sort_by(|left, right| left.provider.cmp(&right.provider));

    let mut seen_providers = HashSet::new();
    let provider_order = app
        .provider_order
        .iter()
        .filter(|provider| !provider.is_empty() && seen_providers.insert((*provider).clone()))
        .cloned()
        .collect::<Vec<_>>();

    let preferences = TuiPreferences {
        limit_display_mode: Some(app.limit_display_mode.preference_value().to_string()),
        hidden_usage_providers,
        hidden_usage_accounts,
        account_orders,
        provider_order,
    };
    let data = serde_json::to_vec_pretty(&preferences).map_err(|err| err.to_string())?;
    fs::write(path, data).map_err(|err| err.to_string())
}

#[cfg(unix)]
fn write_session_cookie(path: &Path, cookie: &str) -> Result<(), String> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    let mut file = fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(path)
        .map_err(|err| err.to_string())?;
    file.write_all(cookie.as_bytes())
        .map_err(|err| err.to_string())?;
    let mut permissions = file
        .metadata()
        .map_err(|err| err.to_string())?
        .permissions();
    permissions.set_mode(0o600);
    fs::set_permissions(path, permissions).map_err(|err| err.to_string())
}

#[cfg(not(unix))]
fn write_session_cookie(path: &Path, cookie: &str) -> Result<(), String> {
    fs::write(path, cookie).map_err(|err| err.to_string())
}

fn config_dir() -> PathBuf {
    if let Some(value) = std::env::var_os("XDG_CONFIG_HOME") {
        return PathBuf::from(value);
    }
    if let Some(value) = std::env::var_os("HOME") {
        return PathBuf::from(value).join(".config");
    }
    PathBuf::from(".")
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum GatewayConfigPlatform {
    Linux,
    Macos,
    Windows,
    Other,
}

#[derive(Default)]
struct GatewayConfigLocations {
    config_path: Option<PathBuf>,
    config_dir: Option<PathBuf>,
    xdg_config_home: Option<PathBuf>,
    home: Option<PathBuf>,
    app_data: Option<PathBuf>,
    user_profile: Option<PathBuf>,
}

fn resolve_base_url(explicit_base_url: Option<&str>) -> String {
    let candidates = local_gateway_config_candidates();
    resolve_base_url_from_config_candidates(explicit_base_url, &candidates)
}

fn resolve_base_url_from_config_candidates(
    explicit_base_url: Option<&str>,
    candidates: &[PathBuf],
) -> String {
    if let Some(base_url) = explicit_base_url {
        return normalize_base_url(base_url);
    }

    candidates
        .iter()
        .find_map(|path| base_url_from_gateway_config(path))
        .unwrap_or_else(|| DEFAULT_BASE_URL.to_string())
}

fn local_gateway_config_candidates() -> Vec<PathBuf> {
    let home = environment_path("HOME").or_else(|| environment_path("USERPROFILE"));
    let user_profile = environment_path("USERPROFILE").or_else(|| home.clone());
    let locations = GatewayConfigLocations {
        config_path: environment_path("IO_GATEWAY_CONFIG"),
        config_dir: environment_path("IO_GATEWAY_CONFIG_DIR"),
        xdg_config_home: environment_path("XDG_CONFIG_HOME"),
        home,
        app_data: environment_path("APPDATA"),
        user_profile,
    };
    gateway_config_candidates(current_gateway_config_platform(), &locations)
}

fn environment_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn current_gateway_config_platform() -> GatewayConfigPlatform {
    if cfg!(target_os = "linux") {
        GatewayConfigPlatform::Linux
    } else if cfg!(target_os = "macos") {
        GatewayConfigPlatform::Macos
    } else if cfg!(target_os = "windows") {
        GatewayConfigPlatform::Windows
    } else {
        GatewayConfigPlatform::Other
    }
}

fn gateway_config_candidates(
    platform: GatewayConfigPlatform,
    locations: &GatewayConfigLocations,
) -> Vec<PathBuf> {
    let mut candidates = Vec::new();

    if let Some(path) = locations.config_path.as_ref() {
        push_config_candidate(&mut candidates, path.clone());
    }
    if let Some(dir) = locations.config_dir.as_ref() {
        push_config_candidate(&mut candidates, dir.join("config.json"));
    }

    match platform {
        GatewayConfigPlatform::Linux | GatewayConfigPlatform::Other => {
            if let Some(dir) = locations.xdg_config_home.as_ref() {
                push_config_candidate(&mut candidates, dir.join("io-gateway/config.json"));
            }
            if let Some(home) = locations.home.as_ref() {
                push_config_candidate(&mut candidates, home.join(".config/io-gateway/config.json"));
            }
        }
        GatewayConfigPlatform::Macos => {
            if let Some(dir) = locations.xdg_config_home.as_ref() {
                push_config_candidate(&mut candidates, dir.join("io-gateway/config.json"));
            }
            if let Some(home) = locations.home.as_ref() {
                push_config_candidate(
                    &mut candidates,
                    home.join("Library/Application Support/io-gateway/config.json"),
                );
            }
        }
        GatewayConfigPlatform::Windows => {
            if let Some(app_data) = locations.app_data.as_ref() {
                push_config_candidate(&mut candidates, app_data.join("io-gateway/config.json"));
            }
            if let Some(user_profile) = locations.user_profile.as_ref() {
                push_config_candidate(
                    &mut candidates,
                    user_profile.join("AppData/Roaming/io-gateway/config.json"),
                );
            }
        }
    }

    candidates
}

fn push_config_candidate(candidates: &mut Vec<PathBuf>, candidate: PathBuf) {
    if !candidates.iter().any(|path| path == &candidate) {
        candidates.push(candidate);
    }
}

fn base_url_from_gateway_config(path: &Path) -> Option<String> {
    let config = fs::read_to_string(path).ok()?;
    let value = serde_json::from_str::<Value>(&config).ok()?;
    let listen = value.get("listen")?.as_str()?.trim();
    let address = listen.parse::<SocketAddr>().ok()?;
    let port = address.port();
    if port == 0 {
        return None;
    }

    let host = if address.is_ipv6() && (address.ip().is_loopback() || address.ip().is_unspecified())
    {
        "[::1]"
    } else {
        "127.0.0.1"
    };
    Some(format!("http://{host}:{port}"))
}

fn normalize_base_url(raw: &str) -> String {
    raw.trim().trim_end_matches('/').to_string()
}

fn provider_spec(provider: &str) -> Option<&'static ProviderSpec> {
    let normalized = normalize_usage_provider(provider);
    PROVIDERS.iter().find(|spec| spec.key == normalized)
}

fn provider_label(provider: &str) -> &'static str {
    provider_spec(provider)
        .map(|spec| spec.label)
        .unwrap_or("Unknown")
}

fn normalize_usage_provider(provider: &str) -> String {
    match provider.trim().to_ascii_lowercase().as_str() {
        "cod" | "codex" => "codex",
        "agw" | "antigravity" | "anti-gravity" => "antigravity",
        "gem" | "gemini" => "gemini",
        "qwn" | "qwen" => "qwen",
        "dsk" | "deepseek" | "deep-seek" => "deepseek",
        "grk" | "grok" | "xai" | "x-ai" => "grok",
        "min" | "minimax" | "mini-max" => "minimax",
        "cop" | "copilot" | "github-copilot" => "copilot",
        "cld" | "claude" | "anthropic" => "claude",
        "glm" | "zai" | "z-ai" => "glm",
        "cmp" | "compatible" => "compatible",
        other => other,
    }
    .to_string()
}

fn normalize_refresh_provider(provider: &str) -> Option<&'static str> {
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
        "cmp" | "compatible" => Some("cmp"),
        _ => None,
    }
}

fn push_query(path: &mut String, key: &str, value: Option<&str>) {
    let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
        return;
    };
    let sep = if path.contains('?') { '&' } else { '?' };
    path.push(sep);
    path.push_str(key);
    path.push('=');
    path.push_str(&url_encode(value));
}

fn url_encode(value: &str) -> String {
    value
        .bytes()
        .flat_map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                vec![byte as char]
            }
            _ => format!("%{byte:02X}").chars().collect(),
        })
        .collect()
}

fn message_from(value: &Value) -> String {
    if let Some(message) = value.get("message").and_then(Value::as_str) {
        return message.to_string();
    }
    if let Some(message) = value
        .get("error")
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
    {
        return message.to_string();
    }
    if let Some(text) = value.get("text").and_then(Value::as_str) {
        return text.to_string();
    }
    if let Some(text) = value.as_str() {
        return text.to_string();
    }
    serde_json::to_string(value).unwrap_or_default()
}

fn string_at(value: &Value, path: &[&str]) -> String {
    let mut current = value;
    for key in path {
        current = match current.get(*key) {
            Some(value) => value,
            None => return String::new(),
        };
    }
    match current {
        Value::String(text) => text.clone(),
        Value::Number(number) => number.to_string(),
        Value::Bool(value) => value.to_string(),
        _ => String::new(),
    }
}

fn number_at(value: &Value, path: &[&str]) -> u64 {
    let mut current = value;
    for key in path {
        current = match current.get(*key) {
            Some(value) => value,
            None => return 0,
        };
    }
    current
        .as_u64()
        .or_else(|| current.as_i64().map(|value| value.max(0) as u64))
        .or_else(|| current.as_f64().map(|value| value.max(0.0) as u64))
        .unwrap_or(0)
}

fn bool_at(value: &Value, path: &[&str]) -> bool {
    let mut current = value;
    for key in path {
        current = match current.get(*key) {
            Some(value) => value,
            None => return false,
        };
    }
    current.as_bool().unwrap_or(false)
}

fn first_string_recursive(value: &Value, keys: &[&str]) -> Option<String> {
    match value {
        Value::Object(map) => {
            for key in keys {
                if let Some(value) = map.get(*key).and_then(Value::as_str) {
                    let value = value.trim();
                    if !value.is_empty() {
                        return Some(value.to_string());
                    }
                }
            }
            for child in map.values() {
                if let Some(value) = first_string_recursive(child, keys) {
                    return Some(value);
                }
            }
            None
        }
        Value::Array(items) => items
            .iter()
            .find_map(|item| first_string_recursive(item, keys)),
        _ => None,
    }
}

fn first_number_recursive(value: &Value, keys: &[&str]) -> Option<f64> {
    match value {
        Value::Object(map) => {
            for key in keys {
                if let Some(value) = map.get(*key).and_then(Value::as_f64) {
                    return Some(value);
                }
            }
            for child in map.values() {
                if let Some(value) = first_number_recursive(child, keys) {
                    return Some(value);
                }
            }
            None
        }
        Value::Array(items) => items
            .iter()
            .find_map(|item| first_number_recursive(item, keys)),
        _ => None,
    }
}

fn access_summary(value: &Value) -> String {
    let request_limit = value
        .get("max_estimated_input_tokens_per_request")
        .or_else(|| value.get("prompt_token_limit"))
        .and_then(Value::as_u64);
    let budget = value.get("input_token_budget").and_then(|budget| {
        let limit = budget.get("limit").and_then(Value::as_u64)?;
        let period = budget
            .get("period")
            .and_then(Value::as_str)
            .unwrap_or("lifetime")
            .replace('_', " ");
        Some(format!("budget={limit}/{period}"))
    });
    let whole_key_limits = [
        request_limit.map(|limit| format!("max={limit}/request")),
        budget,
        value
            .get("quota")
            .and_then(|quota| quota.get("rules"))
            .and_then(Value::as_array)
            .map(|rules| {
                rules
                    .iter()
                    .filter_map(|rule| {
                        Some(format!(
                            "{}={}/{}",
                            rule.get("metric")?.as_str()?,
                            rule.get("limit")?.as_u64()?,
                            rule.get("period")?.as_str()?
                        ))
                    })
                    .collect::<Vec<_>>()
                    .join(", ")
            }),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>();
    let suffix = if whole_key_limits.is_empty() {
        String::new()
    } else {
        format!(" ({})", whole_key_limits.join(", "))
    };
    if bool_at(value, &["all"]) {
        return format!("all{suffix}");
    }
    let providers = value
        .get("providers")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|provider| {
            let name = provider.get("provider").and_then(Value::as_str)?;
            let accounts = provider
                .get("accounts")
                .and_then(Value::as_array)
                .map_or(0, Vec::len);
            let scope = provider
                .get("account_scope")
                .and_then(Value::as_str)
                .filter(|scope| matches!(*scope, "all" | "selected"))
                // Legacy empty account lists meant all accounts. New records
                // carry the explicit scope shown above.
                .unwrap_or(if accounts == 0 { "all" } else { "selected" });
            let provider_limit = provider
                .get("max_estimated_input_tokens_per_request")
                .or_else(|| provider.get("prompt_token_limit"))
                .and_then(Value::as_u64)
                .map(|limit| format!(" <= {limit}/request"))
                .unwrap_or_default();
            let accounts = if scope == "all" {
                "all accounts".to_string()
            } else {
                format!("{accounts} account{}", if accounts == 1 { "" } else { "s" })
            };
            Some(format!("{name} ({accounts}{provider_limit})"))
        })
        .collect::<Vec<_>>();
    if providers.is_empty() {
        format!("restricted{suffix}")
    } else {
        format!("{}{}", providers.join(", "), suffix)
    }
}

fn percent(value: Option<f64>) -> String {
    value
        .map(|value| format!("{value:.0}%"))
        .unwrap_or_default()
}

fn truncate(value: &str, width: usize) -> String {
    if value.chars().count() <= width {
        return value.to_string();
    }
    if width <= 3 {
        return ".".repeat(width);
    }
    let mut out = value.chars().take(width - 3).collect::<String>();
    out.push_str("...");
    out
}

fn short_message(value: &str) -> String {
    truncate(value.replace('\n', " ").trim(), 72)
}

trait EmptyStringExt {
    fn if_empty(self, fallback: String) -> String;
}

impl EmptyStringExt for String {
    fn if_empty(self, fallback: String) -> String {
        if self.trim().is_empty() {
            fallback
        } else {
            self
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_runtime_snapshot_marks_cooldown_without_overriding_disabled() {
        let mut accounts = vec![
            AccountRow {
                provider: "codex".to_string(),
                key: "one.json".to_string(),
                file_name: "one.json".to_string(),
                enabled: true,
                ..AccountRow::default()
            },
            AccountRow {
                provider: "codex".to_string(),
                key: "two.json".to_string(),
                file_name: "two.json".to_string(),
                enabled: false,
                ..AccountRow::default()
            },
        ];
        let snapshot = json!({
            "dashboard": {"accounts": [
                {"file_name": "one.json", "runtime": {
                    "status": "cooldown",
                    "cooldown_remaining_seconds": 42
                }},
                {"file_name": "two.json", "runtime": {"status": "disabled"}}
            ]}
        });

        apply_account_runtime_status(&mut accounts, &snapshot);

        assert!(accounts[0].cooldown);
        assert_eq!(accounts[0].cooldown_remaining_seconds, Some(42));
        assert!(!accounts[1].cooldown);
        assert_eq!(
            account_status(accounts[0].enabled, accounts[0].cooldown),
            AccountStatus::Cooldown
        );
        assert_eq!(
            account_status(accounts[1].enabled, accounts[1].cooldown),
            AccountStatus::Disabled
        );
    }

    #[test]
    fn account_details_collects_reset_credits_and_models_from_quota_snapshot() {
        let account = AccountRow {
            provider: "codex".to_string(),
            key: "account@example.com".to_string(),
            account_id: "account@example.com".to_string(),
            enabled: true,
            ..AccountRow::default()
        };
        let snapshot = json!({
            "quotas": {"codex": {"accounts": [{
                "account_id": "account@example.com",
                "rate_limit_reset_credits": {
                    "available_count": 2,
                    "credits": [{"title": "Weekly reset", "expires_at": "tomorrow"}]
                },
                "models": [{"model_id": "gpt-test", "display_name": "Test model"}]
            }]}}
        });
        let values = account_snapshot_values(&snapshot, &account);

        assert_eq!(reset_credit_details(&values).unwrap().0, 2);
        assert_eq!(
            reset_credit_details(&values).unwrap().1,
            vec!["Weekly reset; expires tomorrow"]
        );
        assert_eq!(snapshot_model_names(&values), vec!["Test model"]);
    }

    #[test]
    fn account_summary_works_with_new_and_old_gateways_and_never_crosses_accounts() {
        let account = AccountRow {
            provider: "deepseek".into(),
            file_name: "one.json".into(),
            key: "one.json".into(),
            ..Default::default()
        };
        let quota = json!({"accounts":[
            {"file_name":"one.json","balances":[{"currency":"USD","total_balance":"4.50"}],"api_key":"SECRET"},
            {"file_name":"two.json","balances":[{"currency":"USD","total_balance":"999"}]}
        ]});
        let old = json!({"quotas":{"deepseek":quota}});
        let new = json!({"quotas":{"deepseek":provider_quota_summary::with_summaries(quota)}});
        let fields = account_quota_summary_fields(&old, &account);
        assert_eq!(fields, account_quota_summary_fields(&new, &account));
        let text = serde_json::to_string(&fields).unwrap();
        assert!(text.contains("USD") && text.contains("4.50"));
        assert!(!text.contains("999") && !text.contains("SECRET"));
        assert!(account_quota_summary_fields(&json!({"quotas":{}}), &account).is_empty());
    }

    #[test]
    fn detail_wrapping_preserves_unicode_styles_and_makes_final_rows_reachable() {
        let content = "wide 日本語 email@example.com end";
        let lines = wrap_detail_lines(
            vec![Line::from(Span::styled(content, btop_header_style()))],
            12,
        );
        assert!(lines.len() > 2);
        assert!(lines.iter().all(|line| line.width() <= 12));
        let joined = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert_eq!(joined, content);
        assert!(lines
            .iter()
            .flat_map(|line| &line.spans)
            .all(|span| span.style == btop_header_style()));
        assert!(wrap_detail_lines(vec![Line::from("unavailable")], 0).is_empty());
    }

    #[test]
    fn account_attention_matches_web_thresholds_and_expiry() {
        let mut accounts = vec![AccountRow {
            provider: "codex".to_string(),
            key: "one.json".to_string(),
            file_name: "one.json".to_string(),
            enabled: true,
            errors: 2,
            last_error_at: "2026-01-01T00:00:00Z".to_string(),
            last_success_at: "2026-02-01T00:00:00Z".to_string(),
            ..AccountRow::default()
        }];
        let snapshot = json!({
            "dashboard": {"accounts": [{
                "file_name": "one.json",
                "expired_at": "2000-01-01T00:00:00Z"
            }]}
        });
        let quotas = vec![QuotaRow {
            provider: "codex".to_string(),
            account_key: "one.json".to_string(),
            account_label: "one.json".to_string(),
            used_percent_value: Some(80.0),
            ..QuotaRow::default()
        }];

        apply_account_attention(&mut accounts, &snapshot, &quotas);

        assert_eq!(
            accounts[0].attention_reasons,
            vec!["expired tokens", "near quota"]
        );
        assert!(!accounts[0]
            .attention_reasons
            .iter()
            .any(|reason| reason == "errors"));
    }

    #[test]
    fn notification_rows_mirror_web_settings_and_keep_watch_state() {
        let settings = json!({
            "ok": true,
            "settings": {
                "enabled": true,
                "channel": "google_chat",
                "telegram": {"bot_token_configured": false, "chat_id": ""},
                "google_chat": {"webhook_configured": true},
                "watched_accounts": ["gemini:two"],
                "accounts": [
                    {"provider":"gemini", "provider_label":"Gemini", "key":"gemini:two", "label":"Two", "enabled":false},
                    {"provider":"codex", "provider_label":"Codex", "key":"codex:one", "label":"One", "account_id":"one@example.com", "enabled":true}
                ]
            }
        });

        let rows = notification_account_rows(&settings);
        assert_eq!(
            rows.iter()
                .map(|row| row.provider.as_str())
                .collect::<Vec<_>>(),
            vec!["codex", "gemini"]
        );
        assert!(!rows[0].watched);
        assert!(rows[1].watched);
        assert!(!rows[1].enabled);
        assert_eq!(
            notification_channel(notification_settings_value(&settings)),
            "google_chat"
        );
        assert_eq!(
            notification_settings_update_body(
                true,
                "telegram".to_string(),
                vec!["codex:one".to_string()]
            ),
            json!({"enabled":true,"channel":"telegram","watched_accounts":["codex:one"]})
        );
    }

    #[test]
    fn command_modal_keymaps_follow_the_active_section() {
        let mut app =
            TuiApp::new(GatewayClient::new("http://127.0.0.1:1".to_string()).unwrap()).unwrap();

        let keys = |app: &TuiApp| {
            command_modal_rows(app)
                .into_iter()
                .map(|(key, _)| key)
                .filter(|key| !key.is_empty())
                .collect::<Vec<_>>()
        };

        app.overview_section = OverviewSection::AccountUsage;
        let account_usage = keys(&app);
        assert!(account_usage.iter().any(|key| key == "Ctrl+j/k/Enter"));
        assert!(account_usage.iter().any(|key| key == "x"));
        assert!(account_usage.iter().any(|key| key == "n"));

        app.overview_section = OverviewSection::CustomModels;
        let models = keys(&app);
        assert!(models.iter().any(|key| key == "e"));
        assert!(models.iter().any(|key| key == "x"));
        assert!(!models.iter().any(|key| key == "Ctrl+j/k/Enter"));

        app.tab = Tab::Keys;
        let api_keys = keys(&app);
        assert!(api_keys.iter().any(|key| key == "n"));
        assert!(api_keys.iter().any(|key| key == "Enter"));
        assert!(!api_keys.iter().any(|key| key == "c"));

        app.tab = Tab::Notifications;
        let notifications = keys(&app);
        assert!(notifications.iter().any(|key| key == "a/u"));
        assert!(notifications.iter().any(|key| key == "s"));
        assert!(!notifications.iter().any(|key| key == "n"));
    }

    #[test]
    fn quota_rows_keep_web_labels_text_and_duplicate_limits_distinct() {
        let codex = quota_rows_from_provider(
            "codex",
            &json!({"accounts": [{
                "label": "Ada",
                "code_generation": {
                    "five_hour": {"used_percent": 12.5, "reset_label": "2h"},
                    "weekly": {"used_percent": 31.0, "reset_label": "4d"}
                },
                "code_review": {"five_hour": {"used_percent": 4.0}}
            }]}),
        );
        let labels = codex
            .iter()
            .map(|row| row.limit_label.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            labels,
            vec!["Code Gen 5h", "Code Gen Weekly", "Code Review 5h"]
        );
        assert_eq!(codex[0].detail_text, "12.5% 2h");

        let qwen = quota_rows_from_provider(
            "qwen",
            &json!({"accounts": [{
                "label": "Qwen",
                "limits": [
                    {"label": "Requests", "used_text": "10", "limit_text": "100", "reset_label": "in 1h"},
                    {"label": "Requests", "used_text": "20", "limit_text": "200", "reset_label": "in 2h"}
                ]
            }]}),
        );
        assert_eq!(qwen[0].detail_text, "10/100 in 1h");
        assert_eq!(
            qwen.iter()
                .map(|row| row.limit_label.as_str())
                .collect::<Vec<_>>(),
            vec!["Requests", "Requests (2)"]
        );

        let gemini = quota_rows_from_provider(
            "gemini",
            &json!({"accounts": [{
                "label": "Gemini",
                "models": [
                    {"model_id": "gemini-2.5-flash", "display_name": "gemini-2.5-flash", "current": {"used_percent": 55.0}},
                    {"model_id": "gemini-2.5-pro", "display_name": "gemini-2.5-pro", "current": {"used_percent": 25.0}}
                ]
            }]}),
        );
        assert!(gemini.iter().any(|row| row.limit_label == "Gemini Flash"));
        assert!(gemini
            .iter()
            .any(|row| { row.limit_label == "gemini-2.5-flash" && !row.overview }));
    }

    #[test]
    fn grok_quota_rows_include_each_kind_from_the_live_snapshot() {
        let rows = quota_rows_from_provider(
            "grok",
            &json!({
                "account": {"label": "Grok", "file_name": "grok.json"},
                "kinds": {
                    "DEFAULT_TEXT": {"rate_limits": {
                        "requests": {"remaining": 8, "limit": 10},
                        "tokens": {"remaining": 900, "limit": 1000}
                    }},
                    "DEFAULT_IMAGE": {"rate_limits": {
                        "requests": {"remaining": 2, "limit": 4}
                    }}
                }
            }),
        );
        assert_eq!(
            rows.iter()
                .map(|row| row.limit_label.as_str())
                .collect::<Vec<_>>(),
            vec!["Text requests", "Text tokens", "Image requests"]
        );
        assert_eq!(rows[0].detail_text, "8/10 (no reset info)");
    }

    #[test]
    fn account_reorder_keymap_accepts_control_key_variants() {
        let control = KeyModifiers::CONTROL;
        assert_eq!(
            account_reorder_delta(KeyEvent::new(KeyCode::Up, control)),
            None
        );
        assert_eq!(
            account_reorder_delta(KeyEvent::new(KeyCode::Down, control)),
            None
        );
        assert_eq!(
            account_reorder_delta(KeyEvent::new(KeyCode::Char('k'), control)),
            Some(-1)
        );
        assert_eq!(
            account_reorder_delta(KeyEvent::new(KeyCode::Char('K'), control)),
            Some(-1)
        );
        assert_eq!(
            account_reorder_delta(KeyEvent::new(KeyCode::Char('j'), control)),
            Some(1)
        );
        assert_eq!(
            account_reorder_delta(KeyEvent::new(KeyCode::Char('J'), control)),
            Some(1)
        );
        assert_eq!(
            account_reorder_delta(KeyEvent::new(KeyCode::Enter, control)),
            Some(1)
        );
        assert_eq!(
            account_reorder_delta(KeyEvent::new(KeyCode::Char('\u{0b}'), KeyModifiers::NONE)),
            Some(-1)
        );
        assert_eq!(
            account_reorder_delta(KeyEvent::new(KeyCode::Char('\u{0a}'), KeyModifiers::NONE)),
            Some(1)
        );
        assert_eq!(
            account_reorder_delta(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE)),
            None
        );
        assert_eq!(
            account_reorder_delta(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)),
            None
        );
        assert_eq!(
            provider_reorder_delta(KeyEvent::new(KeyCode::Up, control)),
            Some(-1)
        );
        assert_eq!(
            provider_reorder_delta(KeyEvent::new(KeyCode::Down, control)),
            Some(1)
        );
        assert_eq!(
            provider_reorder_delta(KeyEvent::new(KeyCode::Char('j'), control)),
            None
        );
        assert_eq!(
            provider_reorder_delta(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
            None
        );
    }

    #[test]
    fn overview_account_usage_navigation_moves_by_account_in_all_limits_mode() {
        let mut app =
            TuiApp::new(GatewayClient::new("http://127.0.0.1:1".to_string()).unwrap()).unwrap();
        app.overview_section = OverviewSection::AccountUsage;
        app.limit_display_mode = LimitDisplayMode::AllPerAccount;
        app.data.accounts = vec![
            AccountRow {
                provider: "codex".to_string(),
                key: "one".to_string(),
                label: "One".to_string(),
                enabled: true,
                ..AccountRow::default()
            },
            AccountRow {
                provider: "codex".to_string(),
                key: "two".to_string(),
                label: "Two".to_string(),
                enabled: true,
                ..AccountRow::default()
            },
        ];
        app.data.quotas = vec![
            QuotaRow {
                provider: "codex".to_string(),
                account_key: "one".to_string(),
                limit_label: "5h".to_string(),
                used_percent_value: Some(20.0),
                ..QuotaRow::default()
            },
            QuotaRow {
                provider: "codex".to_string(),
                account_key: "one".to_string(),
                limit_label: "weekly".to_string(),
                used_percent_value: Some(30.0),
                ..QuotaRow::default()
            },
            QuotaRow {
                provider: "codex".to_string(),
                account_key: "two".to_string(),
                limit_label: "5h".to_string(),
                used_percent_value: Some(40.0),
                ..QuotaRow::default()
            },
        ];

        assert_eq!(
            overview_account_usage_groups(&app)
                .iter()
                .map(|group| group.accounts.len())
                .sum::<usize>(),
            3
        );
        assert_eq!(overview_account_usage_account_count(&app), 2);
        app.move_selection(1);
        assert_eq!(app.overview_account_selected, 1);
        assert_eq!(
            selected_overview_usage_row(&app).unwrap().account_label,
            "Two"
        );
        app.move_selection(-1);
        assert_eq!(app.overview_account_selected, 0);
        assert_eq!(
            selected_overview_usage_row(&app).unwrap().account_label,
            "One"
        );
        app.move_selected_usage_account(1);
        assert_eq!(app.overview_account_selected, 1);
        assert_eq!(
            overview_account_usage_account_rows(&app)
                .into_iter()
                .map(|row| row.account_label)
                .collect::<Vec<_>>(),
            vec!["Two", "One"]
        );
        assert_eq!(
            selected_overview_usage_row(&app).unwrap().account_label,
            "One"
        );
        app.move_selected_usage_account(-1);
        assert_eq!(app.overview_account_selected, 0);
        assert_eq!(
            overview_account_usage_account_rows(&app)
                .into_iter()
                .map(|row| row.account_label)
                .collect::<Vec<_>>(),
            vec!["One", "Two"]
        );
    }

    #[test]
    fn overview_account_usage_provider_reordering_preserves_account_selection() {
        let mut app =
            TuiApp::new(GatewayClient::new("http://127.0.0.1:1".to_string()).unwrap()).unwrap();
        app.overview_section = OverviewSection::AccountUsage;
        app.account_orders.clear();
        app.provider_order.clear();
        app.data.accounts = vec![
            AccountRow {
                provider: "codex".to_string(),
                key: "codex-account".to_string(),
                label: "Codex account".to_string(),
                enabled: true,
                ..AccountRow::default()
            },
            AccountRow {
                provider: "claude".to_string(),
                key: "claude-account".to_string(),
                label: "Claude account".to_string(),
                enabled: true,
                ..AccountRow::default()
            },
            AccountRow {
                provider: "gemini".to_string(),
                key: "gemini-account".to_string(),
                label: "Gemini account".to_string(),
                enabled: true,
                ..AccountRow::default()
            },
        ];

        app.move_selected_usage_provider(1);
        assert_eq!(
            overview_account_usage_groups(&app)
                .into_iter()
                .map(|group| group.provider)
                .collect::<Vec<_>>(),
            vec!["claude", "codex", "gemini"]
        );
        assert_eq!(
            selected_overview_usage_row(&app).unwrap().account_label,
            "Codex account"
        );
        assert_eq!(app.overview_account_selected, 1);

        app.move_selected_usage_provider(1);
        assert_eq!(
            overview_account_usage_groups(&app)
                .into_iter()
                .map(|group| group.provider)
                .collect::<Vec<_>>(),
            vec!["claude", "gemini", "codex"]
        );
        app.move_selected_usage_provider(-1);
        assert_eq!(
            overview_account_usage_groups(&app)
                .into_iter()
                .map(|group| group.provider)
                .collect::<Vec<_>>(),
            vec!["claude", "codex", "gemini"]
        );
    }

    #[test]
    fn renewable_quota_cli_accepts_create_update_and_balance_commands() {
        let quota =
            r#"{"timezone":"UTC","rules":[{"metric":"requests","period":"weekly","limit":10}]}"#;
        let parsed =
            Cli::try_parse_from(["iogw", "keys", "create", "--quota-json", quota]).unwrap();
        let Some(Command::Keys(KeysCommand {
            action: Some(KeysAction::Create(args)),
        })) = parsed.command
        else {
            panic!("expected key create");
        };
        assert_eq!(args.quota_json.as_deref(), Some(quota));
        let parsed = Cli::try_parse_from([
            "iogw",
            "keys",
            "update",
            "key-one",
            "--access-json",
            "@policy.json",
        ])
        .unwrap();
        let Some(Command::Keys(KeysCommand {
            action: Some(KeysAction::Update(args)),
        })) = parsed.command
        else {
            panic!("expected key update");
        };
        assert_eq!(args.id, "key-one");
        assert_eq!(args.access_json, "@policy.json");
        let parsed = Cli::try_parse_from(["iogw", "keys", "quotas", "--id", "key-one"]).unwrap();
        assert!(matches!(parsed.command, Some(Command::Keys(KeysCommand {
            action: Some(KeysAction::Quotas { id: Some(ref id) })
        })) if id == "key-one"));
        assert!(Cli::try_parse_from(["iogw", "keys", "update", "key-one"]).is_err());
    }

    #[test]
    fn renewable_quota_cli_summary_keeps_scope_caps_and_each_rule() {
        let summary = access_summary(&json!({
            "all": false, "max_estimated_input_tokens_per_request": 400,
            "providers": [{"provider":"claude", "account_scope":"selected", "accounts":["claude:one"]}],
            "quota": {"timezone":"UTC", "rules":[
                {"metric":"requests","period":"daily","limit":5},
                {"metric":"output_tokens","period":"weekly","limit":1000}
            ]}
        }));
        assert!(summary.contains("claude (1 account)"));
        assert!(summary.contains("max=400/request"));
        assert!(summary.contains("requests=5/daily"));
        assert!(summary.contains("output_tokens=1000/weekly"));
    }

    #[test]
    fn key_create_explicit_cap_overrides_json_without_losing_scope_or_quotas() {
        let access = json!({
            "all": false,
            "prompt_token_limit": 999,
            "max_estimated_input_tokens_per_request": 500,
            "providers": [{"provider":"claude", "account_scope":"selected", "accounts":["claude:one"]}],
            "quota": {"rules":[{"metric":"requests", "period":"weekly", "limit":10}]}
        });
        let args = KeyCreateArgs {
            label: "test".to_string(),
            access_json: Some(access.to_string()),
            quota_json: None,
            max_estimated_input_tokens_per_request: Some(1),
        };
        let payload = key_create_access(&args).unwrap();
        assert_eq!(payload["max_estimated_input_tokens_per_request"], 1);
        assert!(payload.get("prompt_token_limit").is_none());
        assert_eq!(payload["providers"], access["providers"]);
        assert_eq!(payload["quota"], access["quota"]);
        assert_eq!(payload["all"], false);
    }

    #[test]
    fn key_create_zero_cap_is_rejected_with_or_without_access_json() {
        for access_json in [None, Some(r#"{"all":true}"#.to_string())] {
            let args = KeyCreateArgs {
                label: "test".to_string(),
                access_json,
                quota_json: None,
                max_estimated_input_tokens_per_request: Some(0),
            };
            assert!(key_create_access(&args)
                .unwrap_err()
                .contains("greater than zero"));
        }
    }

    fn test_dir(name: &str) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("io-gateway-iogw-{name}-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&path).expect("create test directory");
        path
    }

    #[test]
    fn explicit_base_url_wins_over_local_config() {
        let dir = test_dir("explicit-base-url");
        let config = dir.join("config.json");
        fs::write(&config, r#"{"listen":"127.0.0.1:9123"}"#).expect("write config");

        assert_eq!(
            resolve_base_url_from_config_candidates(
                Some("https://example.test/gateway/"),
                &[config]
            ),
            "https://example.test/gateway"
        );

        fs::remove_dir_all(dir).expect("remove test directory");
    }

    #[test]
    fn discovers_the_selected_local_port_and_skips_invalid_configs() {
        let dir = test_dir("selected-port");
        let invalid = dir.join("invalid.json");
        let valid = dir.join("valid.json");
        fs::write(&invalid, "not json").expect("write invalid config");
        fs::write(&valid, r#"{"listen":"0.0.0.0:9123"}"#).expect("write config");

        assert_eq!(
            resolve_base_url_from_config_candidates(None, &[invalid, valid]),
            "http://127.0.0.1:9123"
        );

        fs::remove_dir_all(dir).expect("remove test directory");
    }

    #[test]
    fn preserves_a_loopback_ipv6_listener() {
        let dir = test_dir("ipv6-listener");
        let config = dir.join("config.json");
        fs::write(&config, r#"{"listen":"[::1]:9124"}"#).expect("write config");

        assert_eq!(
            resolve_base_url_from_config_candidates(None, &[config]),
            "http://[::1]:9124"
        );

        fs::remove_dir_all(dir).expect("remove test directory");
    }

    #[test]
    fn falls_back_to_the_historical_default_without_a_valid_local_config() {
        assert_eq!(
            resolve_base_url_from_config_candidates(None, &[]),
            DEFAULT_BASE_URL
        );
    }

    #[test]
    fn searches_the_platform_specific_installer_config_locations() {
        let locations = GatewayConfigLocations {
            xdg_config_home: Some(PathBuf::from("/xdg")),
            home: Some(PathBuf::from("/home/alice")),
            app_data: Some(PathBuf::from("C:/Users/Alice/AppData/Roaming")),
            user_profile: Some(PathBuf::from("C:/Users/Alice")),
            ..GatewayConfigLocations::default()
        };

        assert_eq!(
            gateway_config_candidates(GatewayConfigPlatform::Linux, &locations),
            vec![
                PathBuf::from("/xdg/io-gateway/config.json"),
                PathBuf::from("/home/alice/.config/io-gateway/config.json"),
            ]
        );
        assert_eq!(
            gateway_config_candidates(GatewayConfigPlatform::Macos, &locations),
            vec![
                PathBuf::from("/xdg/io-gateway/config.json"),
                PathBuf::from("/home/alice/Library/Application Support/io-gateway/config.json"),
            ]
        );
        assert_eq!(
            gateway_config_candidates(GatewayConfigPlatform::Windows, &locations),
            vec![PathBuf::from(
                "C:/Users/Alice/AppData/Roaming/io-gateway/config.json"
            )]
        );
    }
}
