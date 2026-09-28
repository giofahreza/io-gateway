use clap::{Args, Parser, Subcommand};
use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
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
const BTOP_CPU_MID: Color = Color::Rgb(0xcb, 0xc0, 0x6c);
const BTOP_CACHED_MID: Color = Color::Rgb(0x74, 0xe6, 0xfc);
const BTOP_AVAILABLE_END: Color = Color::Rgb(0xff, 0xb8, 0x14);
const BTOP_USED_END: Color = Color::Rgb(0xff, 0x47, 0x69);

const PROVIDERS: &[ProviderSpec] = &[
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
    /// Revoke an API key by id.
    Revoke { id: String },
}

#[derive(Args, Debug)]
struct KeyCreateArgs {
    #[arg(short, long, default_value = "API key")]
    label: String,
    /// Optional JSON access object, or @path to a JSON file.
    #[arg(long)]
    access_json: Option<String>,
    /// Optional whole-key prompt token limit for unrestricted keys.
    #[arg(long)]
    prompt_token_limit: Option<u64>,
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
    priority: bool,
    requests: u64,
    errors: u64,
    total_tokens: u64,
    last_success_at: String,
    last_error_at: String,
    last_error_message: String,
}

#[derive(Clone, Default)]
struct QuotaRow {
    provider: String,
    account_key: String,
    account_label: String,
    limit_label: String,
    label: String,
    used_percent_value: Option<f64>,
    used_percent: String,
    remaining_percent: String,
    reset: String,
}

#[derive(Clone, Default)]
struct KeyRow {
    id: String,
    label: String,
    prefix: String,
    source: String,
    revoked: bool,
    created_at: String,
    last_used_at: String,
    access: String,
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
    context_history: Value,
    keys: Vec<KeyRow>,
    models: Vec<ModelRow>,
    accounts: Vec<AccountRow>,
    quotas: Vec<QuotaRow>,
    usage_buckets: Vec<UsageBucket>,
    history: Vec<Value>,
    notifications: Value,
    fetched_at: Option<Instant>,
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
    Accounts,
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
    DeleteAccount,
    DeleteCustomModel,
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
}

#[derive(Deserialize, Serialize)]
struct HiddenUsageAccountPreference {
    provider: String,
    account_key: String,
}

struct TuiPreferenceState {
    limit_display_mode: LimitDisplayMode,
    hidden_usage_providers: HashSet<String>,
    hidden_usage_accounts: HashSet<(String, String)>,
}

impl Default for TuiPreferenceState {
    fn default() -> Self {
        Self {
            limit_display_mode: LimitDisplayMode::OnePerAccount,
            hidden_usage_providers: HashSet::new(),
            hidden_usage_accounts: HashSet::new(),
        }
    }
}

impl Tab {
    fn all() -> &'static [Tab] {
        &[Tab::Overview, Tab::Accounts, Tab::Keys, Tab::Notifications]
    }

    fn title(self) -> &'static str {
        match self {
            Tab::Overview => "Overview",
            Tab::Accounts => "Accounts",
            Tab::Keys => "Keys",
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
    show_command_modal: bool,
    confirmation_modal: Option<ConfirmationModal>,
    message: String,
    last_auto_refresh: Instant,
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
    let mut client = GatewayClient::new(resolve_base_url(cli.base_url.as_deref()))?;
    match cli.command.unwrap_or(Command::Tui) {
        Command::Tui => run_tui(client).await,
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
            let data = fetch_gateway_data(client, false).await?;
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
            let data = fetch_gateway_data(client, false).await?;
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
            let data = fetch_gateway_data(client, false).await?;
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
    let data = fetch_gateway_data(client, false).await?;
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
            let access = if let Some(raw) = args.access_json {
                parse_json_arg(&raw)?
            } else {
                let mut access = json!({ "all": true, "providers": [] });
                if let Some(limit) = args.prompt_token_limit {
                    access["prompt_token_limit"] = json!(limit);
                }
                access
            };
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
        KeysAction::Revoke { id } => {
            let response = client
                .post_json("/admin/api-keys/revoke", Some(json!({ "id": id })))
                .await?;
            ensure_success(&response)?;
            print_response(json_output, response.body)
        }
    }
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

async fn run_tui_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    client: GatewayClient,
) -> Result<(), String> {
    let preferences_path = preferences_path(&client.base_url)?;
    let preferences = load_tui_preferences(&preferences_path);
    let mut app = TuiApp {
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
        show_command_modal: false,
        confirmation_modal: None,
        message: "loading".to_string(),
        last_auto_refresh: Instant::now(),
    };
    app.refresh(false).await;
    loop {
        terminal
            .draw(|frame| draw_tui(frame, &app))
            .map_err(|err| err.to_string())?;

        if app.last_auto_refresh.elapsed() >= Duration::from_secs(30) {
            app.refresh(false).await;
            continue;
        }

        if !event::poll(Duration::from_millis(250)).map_err(|err| err.to_string())? {
            continue;
        }
        let Event::Key(key) = event::read().map_err(|err| err.to_string())? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return Ok(());
        }
        if app.confirmation_modal.is_some() {
            match key.code {
                KeyCode::Char('y') | KeyCode::Enter => app.confirm_selection().await,
                KeyCode::Char('n') | KeyCode::Char('?') => app.cancel_confirmation(),
                _ => {}
            }
            continue;
        }
        if app.show_command_modal {
            match key.code {
                KeyCode::Char('?') => app.show_command_modal = false,
                _ => {}
            }
            continue;
        }
        match key.code {
            KeyCode::Char('?') => app.show_command_modal = true,
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
            KeyCode::Char('v') => app.hide_selected_usage_account(),
            KeyCode::Char('r') => app.refresh(true).await,
            KeyCode::Char('a') => {
                let selected_account_index = selected_overview_account_index(&app);
                app.limit_display_mode.toggle();
                if app.tab == Tab::Overview {
                    app.selected = selected_account_index
                        .and_then(|index| overview_selection_for_account(&app, index))
                        .unwrap_or(0);
                }
                app.clamp_selection();
                app.message = format!("quota view: {}", app.limit_display_mode.label());
                app.persist_preferences();
            }
            KeyCode::Char('u') => app.show_all_usage_filters(),
            KeyCode::Char('o') => {
                let otp = prompt_in_terminal(terminal, "OTP: ")?;
                let api_key_required = bool_at(&app.data.session, &["requires_api_key"])
                    || bool_at(&app.data.session, &["api_key_required"]);
                let api_key = if api_key_required {
                    Some(prompt_in_terminal(terminal, "Administrator API key: ")?)
                } else {
                    None
                };
                match app.client.login(&otp, api_key.as_deref()).await {
                    Ok(response) if response.status.is_success() => {
                        app.message = "logged in".to_string();
                        app.refresh(true).await;
                    }
                    Ok(response) => app.message = message_from(&response.body),
                    Err(err) => app.message = err,
                }
            }
            KeyCode::Char('e') => app.enable_selected().await,
            KeyCode::Char('d') => app.disable_selected().await,
            KeyCode::Char('p') => {
                if app.tab == Tab::Overview {
                    app.hide_selected_usage_provider();
                } else {
                    app.toggle_selected_priority().await;
                }
            }
            KeyCode::Char('x') => {
                app.request_delete_confirmation();
            }
            KeyCode::Char('t') => {
                if app.tab == Tab::Notifications {
                    match app.client.post_json("/notifications/test", None).await {
                        Ok(response) if response.status.is_success() => {
                            app.message = message_from(&response.body)
                        }
                        Ok(response) => app.message = message_from(&response.body),
                        Err(err) => app.message = err,
                    }
                }
            }
            _ => {}
        }
    }
}

impl TuiApp {
    async fn refresh(&mut self, explicit: bool) {
        match fetch_gateway_data(&self.client, true).await {
            Ok(data) => {
                self.data = data;
                self.message = if explicit {
                    "refreshed".to_string()
                } else if self.needs_login() {
                    "admin login required: press o".to_string()
                } else {
                    String::new()
                };
                self.last_auto_refresh = Instant::now();
                self.clamp_selection();
            }
            Err(err) => {
                self.message = err;
                self.last_auto_refresh = Instant::now();
            }
        }
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
                OverviewSection::AccountUsage => overview_account_usage_row_count(self),
                _ => 0,
            },
            Tab::Accounts => self.data.accounts.len(),
            Tab::Keys => self.data.keys.len(),
            _ => 0,
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
                    overview_account_usage_row_count(self),
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
            overview_account_usage_row_count(self),
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

    fn selected_model(&self) -> Option<ModelRow> {
        if self.tab == Tab::Overview && self.overview_section == OverviewSection::CustomModels {
            return self.data.models.get(self.overview_model_selected).cloned();
        }
        None
    }

    async fn enable_selected(&mut self) {
        if self.selected_model().is_some() {
            self.toggle_selected_model(true).await;
        } else {
            self.toggle_selected_account(true).await;
        }
    }

    async fn disable_selected(&mut self) {
        if self.selected_model().is_some() {
            self.toggle_selected_model(false).await;
        } else {
            self.toggle_selected_account(false).await;
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

    async fn toggle_selected_account(&mut self, enabled: bool) {
        if !matches!(self.tab, Tab::Overview | Tab::Accounts) {
            return;
        }
        let Some(account) = self.selected_account() else {
            return;
        };
        if account.file_name.is_empty() {
            self.message = "selected account has no credential file".to_string();
            return;
        }
        match self
            .client
            .post_form(
                "/credentials/toggle",
                &[
                    ("file_name", account.file_name),
                    ("enabled", enabled.to_string()),
                ],
            )
            .await
        {
            Ok(response) if response.status.is_success() => {
                self.message = message_from(&response.body);
                self.refresh(true).await;
            }
            Ok(response) => self.message = message_from(&response.body),
            Err(err) => self.message = err,
        }
    }

    async fn toggle_selected_priority(&mut self) {
        if !matches!(self.tab, Tab::Overview | Tab::Accounts) {
            return;
        }
        let Some(account) = self.selected_account() else {
            return;
        };
        if account.key.is_empty() {
            self.message = "selected account has no routing key".to_string();
            return;
        }
        match self
            .client
            .post_json(
                "/admin/account-routing/priority",
                Some(json!({
                    "provider": account.provider,
                    "account": account.key,
                    "priority": !account.priority
                })),
            )
            .await
        {
            Ok(response) if response.status.is_success() => {
                self.message = if account.priority {
                    "priority removed".to_string()
                } else {
                    "account will be used first".to_string()
                };
                self.refresh(true).await;
            }
            Ok(response) => self.message = message_from(&response.body),
            Err(err) => self.message = err,
        }
    }

    async fn toggle_selected_model(&mut self, enabled: bool) {
        let Some(model) = self.selected_model() else {
            return;
        };
        let mut body = model.raw.clone();
        body["enabled"] = json!(enabled);
        match self
            .client
            .post_json("/custom-models/save", Some(body))
            .await
        {
            Ok(response) if response.status.is_success() => {
                self.message = if enabled {
                    "custom model enabled".to_string()
                } else {
                    "custom model disabled".to_string()
                };
                self.refresh(true).await;
            }
            Ok(response) => self.message = message_from(&response.body),
            Err(err) => self.message = err,
        }
    }

    fn request_delete_confirmation(&mut self) {
        if self.tab == Tab::Accounts {
            let Some(account) = self.selected_account() else {
                return;
            };
            self.confirmation_modal = Some(ConfirmationModal {
                title: "Delete Account".to_string(),
                message: format!("Delete account credential {}?", account_display(&account)),
                action: ConfirmationAction::DeleteAccount,
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
                action: ConfirmationAction::DeleteCustomModel,
            });
        }
    }

    fn cancel_confirmation(&mut self) {
        self.confirmation_modal = None;
        self.message = "delete cancelled".to_string();
    }

    async fn confirm_selection(&mut self) {
        let Some(modal) = self.confirmation_modal.take() else {
            return;
        };
        match modal.action {
            ConfirmationAction::DeleteAccount => self.delete_selected_account().await,
            ConfirmationAction::DeleteCustomModel => self.delete_selected_model().await,
        }
    }

    async fn delete_selected_model(&mut self) {
        let Some(model) = self.selected_model() else {
            return;
        };
        match self
            .client
            .post_json(
                "/custom-models/delete",
                Some(json!({ "alias": model.alias })),
            )
            .await
        {
            Ok(response) if response.status.is_success() => {
                self.message = message_from(&response.body);
                self.refresh(true).await;
            }
            Ok(response) => self.message = message_from(&response.body),
            Err(err) => self.message = err,
        }
    }

    async fn delete_selected_account(&mut self) {
        let Some(account) = self.selected_account() else {
            return;
        };
        if account.file_name.is_empty() {
            self.message = "selected account has no credential file".to_string();
            return;
        }
        match self
            .client
            .post_form("/credentials/delete", &[("file_name", account.file_name)])
            .await
        {
            Ok(response) if response.status.is_success() => {
                self.message = message_from(&response.body);
                self.refresh(true).await;
            }
            Ok(response) => self.message = message_from(&response.body),
            Err(err) => self.message = err,
        }
    }
}

async fn fetch_gateway_data(
    client: &GatewayClient,
    include_history: bool,
) -> Result<GatewayData, String> {
    let session_response = client.get("/admin/session").await?;
    let session = session_response.body;
    let mut data = GatewayData {
        session: session.clone(),
        fetched_at: Some(Instant::now()),
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
    data.keys = key_rows_from_response(&keys.body);
    data.models = model_rows_from_response(&models.body);
    data.notifications = notifications.body;

    if include_history {
        let history = client.get("/usage/history.json?limit=30").await?;
        if history.status.is_success() {
            data.history = events_from_response(&history.body);
        }
        let context_history = client
            .get("/usage/context-history.json?hours=24&bucket_minutes=30")
            .await?;
        if context_history.status.is_success() {
            data.usage_buckets = usage_buckets_from_context(&context_history.body);
            data.context_history = context_history.body;
        }
    }

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

fn moved_index(current: usize, len: usize, delta: isize) -> usize {
    if len == 0 {
        return 0;
    }
    (current as isize + delta).clamp(0, len as isize - 1) as usize
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
        if app.needs_login() { "AUTH" } else { "LIVE" }
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
        Tab::Accounts => draw_accounts_table(frame, root[2], &app.data.accounts, app.selected),
        Tab::Keys => draw_keys_table(frame, root[2], &app.data.keys, app.selected),
        Tab::Notifications => draw_notifications(frame, root[2], app),
    }

    let footer = if app.message.trim().is_empty() {
        format!(
            " ? commands | tab switch | h/l section | j/k move | r refresh | a quota: {} | o login | e/d enable | v hide acct | x delete ",
            app.limit_display_mode.label()
        )
    } else {
        format!(" {}", app.message)
    };
    frame.render_widget(Paragraph::new(footer).style(btop_muted_style()), root[3]);

    if app.show_command_modal {
        draw_command_modal(frame, frame.area(), app);
    }
    if let Some(modal) = &app.confirmation_modal {
        draw_confirmation_modal(frame, frame.area(), modal);
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
            app.data
                .fetched_at
                .map(|_| "updated this session")
                .unwrap_or("waiting for data")
                .to_string(),
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
    let content_width = area.width.saturating_sub(2) as usize;
    let max_rows = area.height.saturating_sub(2).max(1) as usize;

    let mut all_lines = Vec::new();
    let mut usage_row_index = 0usize;
    let mut selected_line_index = None;
    for group in &groups {
        all_lines.push(account_usage_provider_header(group, app.limit_display_mode));
        let mut last_account_key: Option<&str> = None;

        for account in &group.accounts {
            if app.limit_display_mode == LimitDisplayMode::AllPerAccount
                && last_account_key != Some(account.account_key.as_str())
            {
                all_lines.push(account_usage_account_header_line(account, content_width));
                last_account_key = Some(account.account_key.as_str());
            }
            let selected = app.tab == Tab::Overview
                && app.overview_section == OverviewSection::AccountUsage
                && usage_row_index == app.overview_account_selected;
            if selected {
                selected_line_index = Some(all_lines.len());
            }
            all_lines.push(account_quota_usage_line(
                account,
                content_width,
                selected,
                app.limit_display_mode,
            ));
            usage_row_index += 1;
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
    let state = if account.enabled { "on" } else { "off" };
    let priority = if account.priority { "first" } else { "" };
    let selection = if selected { ">" } else { " " };

    if content_width < 38 {
        let compact = format!(
            "{selection} {state} {priority} {} {}",
            account.label, percent_text
        );
        let style = if selected {
            btop_selected_style()
        } else {
            btop_style()
        };
        return Line::from(Span::styled(truncate_text(&compact, content_width), style));
    }

    let marker_width = 2;
    let state_width = 4;
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
        Span::styled(
            format!("{state:<state_width$}"),
            selected_or(
                if account.enabled {
                    btop_fg(BTOP_CPU_START)
                } else {
                    btop_muted_style()
                },
                selected,
            ),
        ),
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
    ];

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
) -> Line<'static> {
    let state = if account.enabled { "on" } else { "off" };
    let priority = if account.priority { " first" } else { "" };
    let requests = if content_width >= 64 {
        format!("  {} req", format_short(account.requests))
    } else {
        String::new()
    };
    let label_width = content_width
        .saturating_sub(3 + state.len() + priority.len() + requests.len())
        .max(8);
    Line::from(vec![
        Span::styled("  ", btop_style()),
        Span::styled(
            format!("{state}"),
            if account.enabled {
                btop_fg(BTOP_CPU_START)
            } else {
                btop_muted_style()
            },
        ),
        Span::styled(priority, btop_muted_style()),
        Span::raw(" "),
        Span::styled(
            truncate_text(&account.account_label, label_width),
            btop_title_style(),
        ),
        Span::styled(requests, btop_muted_style()),
    ])
}

fn account_quota_usage_detail_line(
    account: &OverviewAccountUsageRow,
    content_width: usize,
    selected: bool,
) -> Line<'static> {
    let percent_text = account
        .percent
        .map(|percent| format!("{percent:.1}% used"))
        .unwrap_or_else(|| "No quota".to_string());
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

fn draw_accounts_table(
    frame: &mut Frame<'_>,
    area: Rect,
    accounts: &[AccountRow],
    selected: usize,
) {
    let rows = accounts.iter().enumerate().map(|(index, account)| {
        let is_selected = index == selected;
        Row::new(vec![
            Cell::from(if is_selected { ">" } else { "" }),
            Cell::from(account.provider_label.clone()),
            Cell::from(if account.enabled { "on" } else { "off" }),
            Cell::from(if account.priority { "first" } else { "" }),
            Cell::from(account.label.clone()),
            Cell::from(account.file_name.clone()),
            Cell::from(account.requests.to_string()),
            Cell::from(account.errors.to_string()),
            Cell::from(short_message(&account.last_error_message)),
        ])
        .style(if is_selected {
            btop_selected_style()
        } else {
            btop_style()
        })
    });
    let header = Row::new([
        "",
        "Provider",
        "State",
        "Priority",
        "Label",
        "File",
        "Req",
        "Err",
        "Last error",
    ])
    .style(btop_header_style());
    let widths = [
        Constraint::Length(1),
        Constraint::Length(12),
        Constraint::Length(5),
        Constraint::Length(8),
        Constraint::Length(22),
        Constraint::Length(28),
        Constraint::Length(8),
        Constraint::Length(6),
        Constraint::Min(12),
    ];
    frame.render_widget(
        Table::new(rows, widths)
            .header(header)
            .style(btop_style())
            .block(btop_block_with_border("Accounts", BTOP_PROC_BOX))
            .column_spacing(1),
        area,
    );
}

fn draw_keys_table(frame: &mut Frame<'_>, area: Rect, keys: &[KeyRow], selected: usize) {
    let rows = keys.iter().enumerate().map(|(index, key)| {
        let is_selected = index == selected;
        Row::new(vec![
            Cell::from(if is_selected { ">" } else { "" }),
            Cell::from(key.label.clone()),
            Cell::from(key.prefix.clone()),
            Cell::from(if key.revoked { "revoked" } else { "active" }),
            Cell::from(key.source.clone()),
            Cell::from(key.access.clone()),
            Cell::from(key.created_at.clone()),
            Cell::from(key.last_used_at.clone()),
            Cell::from(key.id.clone()),
        ])
        .style(if is_selected {
            btop_selected_style()
        } else {
            btop_style()
        })
    });
    let header = Row::new([
        "",
        "Label",
        "Prefix",
        "State",
        "Source",
        "Access",
        "Created",
        "Last used",
        "Id",
    ])
    .style(btop_header_style());
    frame.render_widget(
        Table::new(
            rows,
            [
                Constraint::Length(1),
                Constraint::Length(20),
                Constraint::Length(16),
                Constraint::Length(8),
                Constraint::Length(10),
                Constraint::Length(18),
                Constraint::Length(20),
                Constraint::Length(20),
                Constraint::Min(20),
            ],
        )
        .header(header)
        .style(btop_style())
        .block(btop_block_with_border("API Keys", BTOP_NET_BOX))
        .column_spacing(1),
        area,
    );
}

fn draw_notifications(frame: &mut Frame<'_>, area: Rect, app: &TuiApp) {
    let settings = app
        .data
        .notifications
        .get("settings")
        .unwrap_or(&app.data.notifications);
    let text = serde_json::to_string_pretty(settings).unwrap_or_else(|_| "{}".to_string());
    frame.render_widget(
        Paragraph::new(text)
            .style(btop_style())
            .block(btop_block_with_border("Notifications", BTOP_NET_BOX))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn draw_command_modal(frame: &mut Frame<'_>, area: Rect, app: &TuiApp) {
    let modal = centered_rect(area, 72, 18);
    let text = vec![
        Line::from(vec![
            Span::styled("?", btop_header_style()),
            Span::raw(" close commands"),
        ]),
        Line::from(vec![
            Span::styled("Tab", btop_header_style()),
            Span::raw(" next tab"),
            Span::styled("    Shift-Tab", btop_header_style()),
            Span::raw(" previous tab"),
        ]),
        Line::from(vec![
            Span::styled("j/k or Up/Down", btop_header_style()),
            Span::raw(" move selection"),
        ]),
        Line::from(vec![
            Span::styled("h/l", btop_header_style()),
            Span::raw(" previous/next Overview section"),
        ]),
        Line::from(vec![
            Span::styled("r", btop_header_style()),
            Span::raw(" refresh data"),
            Span::styled("    o", btop_header_style()),
            Span::raw(" login"),
        ]),
        Line::from(vec![
            Span::styled("a", btop_header_style()),
            Span::raw(format!(
                " toggle quota limit view ({})",
                app.limit_display_mode.label()
            )),
        ]),
        Line::from(vec![
            Span::styled("p", btop_header_style()),
            Span::raw(" hide selected provider from Account Usage"),
            Span::styled("    u", btop_header_style()),
            Span::raw(" show all hidden usage rows"),
        ]),
        Line::from(vec![
            Span::styled("v", btop_header_style()),
            Span::raw(" hide selected account from Account Usage"),
        ]),
        Line::from(""),
        Line::from("Overview Custom Models / Accounts"),
        Line::from(vec![
            Span::styled("e", btop_header_style()),
            Span::raw(" enable selected"),
            Span::styled("    d", btop_header_style()),
            Span::raw(" disable selected"),
            Span::styled("    x", btop_header_style()),
            Span::raw(" delete selected"),
        ]),
        Line::from("Accounts"),
        Line::from(vec![
            Span::styled("    p", btop_header_style()),
            Span::raw(" toggle use-first priority"),
        ]),
        Line::from(""),
        Line::from("Notifications"),
        Line::from(vec![
            Span::styled("t", btop_header_style()),
            Span::raw(" send test notification"),
        ]),
        Line::from(""),
        Line::from(
            "Direct commands: status, accounts, quota, keys, models, usage, notifications, raw",
        ),
    ];

    frame.render_widget(Clear, modal);
    frame.render_widget(
        Paragraph::new(text)
            .style(btop_style())
            .block(btop_block_with_border("Commands", BTOP_DIV_LINE))
            .wrap(Wrap { trim: false }),
        modal,
    );
}

fn draw_confirmation_modal(frame: &mut Frame<'_>, area: Rect, modal: &ConfirmationModal) {
    let area = centered_rect(area, 62, 7);
    let lines = vec![
        Line::from(modal.message.clone()),
        Line::from(""),
        Line::from(vec![
            Span::styled("y/Enter", btop_header_style()),
            Span::raw(" confirm"),
            Span::styled("    n", btop_header_style()),
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
    collect_quota_limit_rows(
        provider,
        &account_key,
        &base_label,
        account,
        None,
        &mut rows,
    );
    if rows.is_empty() {
        let used_percent_value =
            first_number_recursive(account, &["used_percent", "usage_percent", "percent_used"])
                .or_else(|| quota_percent_value(account));
        rows.push(QuotaRow {
            provider: provider.to_string(),
            account_key,
            account_label: base_label.clone(),
            limit_label: String::new(),
            label: base_label,
            used_percent_value,
            used_percent: percent(used_percent_value),
            remaining_percent: percent(first_number_recursive(account, &["remaining_percent"])),
            reset: first_string_recursive(account, &["reset_label", "reset_at", "window"])
                .unwrap_or_default(),
        });
    }
    rows
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
                out.push(QuotaRow {
                    provider: provider.to_string(),
                    account_key: account_key.to_string(),
                    account_label: parent_label.to_string(),
                    limit_label,
                    label,
                    used_percent_value,
                    used_percent: percent(used_percent_value),
                    remaining_percent: percent(first_number_recursive(
                        value,
                        &["remaining_percent"],
                    )),
                    reset: first_string_recursive(
                        value,
                        &["reset_label", "reset_at", "reset_time"],
                    )
                    .unwrap_or_default(),
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
        "five_hour" | "5h" => "5h".to_string(),
        "weekly" => "Weekly".to_string(),
        "current_window" => "5h window".to_string(),
        other => other.replace(['_', '-'], " "),
    }
}

fn quota_child_label_hint(parent_hint: Option<&str>, child_key: &str) -> String {
    let child = format_quota_label_hint(child_key);
    match parent_hint {
        Some("code_generation") | Some("Code Gen") => format!("Code Gen {child}"),
        Some("code_review") | Some("Code Review") => format!("Code Review {child}"),
        _ => child,
    }
}

fn key_rows_from_response(value: &Value) -> Vec<KeyRow> {
    let mut rows = value
        .get("keys")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|key| KeyRow {
            id: string_at(key, &["id"]),
            label: string_at(key, &["label"]),
            prefix: string_at(key, &["key_prefix"]),
            source: string_at(key, &["source"]),
            revoked: key.get("revoked_at").is_some_and(|value| !value.is_null()),
            created_at: string_at(key, &["created_at"]),
            last_used_at: string_at(key, &["last_used_at"]),
            access: access_summary(key.get("access").unwrap_or(&Value::Null)),
        })
        .collect::<Vec<_>>();
    rows.sort_by(|left, right| {
        left.label
            .cmp(&right.label)
            .then_with(|| left.id.cmp(&right.id))
    });
    rows
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
    percent: Option<f64>,
    remaining_percent: String,
    reset: String,
    requests: u64,
    enabled: bool,
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

    filter_overview_account_usage_groups(app, groups)
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
            percent: overview_account_percent(account, quota_rows),
            remaining_percent: String::new(),
            reset: String::new(),
            requests: account.requests,
            enabled: account.enabled,
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

fn overview_account_usage_row_count(app: &TuiApp) -> usize {
    overview_account_usage_groups(app)
        .iter()
        .map(|group| group.accounts.len())
        .sum()
}

fn selected_overview_account_index(app: &TuiApp) -> Option<usize> {
    selected_overview_usage_row(app).map(|row| row.account_index)
}

fn selected_overview_usage_row(app: &TuiApp) -> Option<OverviewAccountUsageRow> {
    overview_account_usage_groups(app)
        .into_iter()
        .flat_map(|group| group.accounts)
        .nth(app.overview_account_selected)
}

fn overview_selection_for_account(app: &TuiApp, account_index: usize) -> Option<usize> {
    overview_account_usage_groups(app)
        .into_iter()
        .flat_map(|group| group.accounts)
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
            .filter(|quota| quota.provider == provider && quota_matches_account(quota, account))
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
                percent: None,
                remaining_percent: String::new(),
                reset: String::new(),
                requests: account.requests,
                enabled: account.enabled,
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
                percent: quota.used_percent_value,
                remaining_percent: quota.remaining_percent.clone(),
                reset: quota.reset.clone(),
                requests: account.requests,
                enabled: account.enabled,
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
                if account.enabled { "on" } else { "off" }.to_string(),
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

    let preferences = TuiPreferences {
        limit_display_mode: Some(app.limit_display_mode.preference_value().to_string()),
        hidden_usage_providers,
        hidden_usage_accounts,
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
    if bool_at(value, &["all"]) {
        if let Some(limit) = value.get("prompt_token_limit").and_then(Value::as_u64) {
            return format!("all <= {limit}");
        }
        return "all".to_string();
    }
    let providers = value
        .get("providers")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|provider| provider.get("provider").and_then(Value::as_str))
        .collect::<Vec<_>>();
    if providers.is_empty() {
        "restricted".to_string()
    } else {
        providers.join(",")
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
