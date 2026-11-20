//! Bounded background requests. The terminal thread only schedules work and
//! applies completed results; it never waits for the gateway.

use super::{bool_at, ensure_success, message_from, ApiResponse, GatewayClient, Tab};
use serde_json::{json, Value};
use std::time::{Duration, Instant};
use tokio::{sync::mpsc, task::JoinHandle};

const MAX_FETCHES: usize = 3;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(usize)]
pub(super) enum Section {
    Session,
    Summary,
    Routing,
    Snapshot,
    Models,
    Chart,
    Keys,
    Notifications,
}

impl Section {
    pub(super) const ALL: [Self; 8] = [
        Self::Session,
        Self::Summary,
        Self::Routing,
        Self::Snapshot,
        Self::Models,
        Self::Chart,
        Self::Keys,
        Self::Notifications,
    ];

    pub(super) fn visible(self, tab: Tab) -> bool {
        match self {
            Self::Session => true,
            Self::Summary | Self::Routing => tab == Tab::Overview,
            Self::Snapshot | Self::Models | Self::Chart => tab == Tab::Overview,
            Self::Keys => tab == Tab::Keys,
            Self::Notifications => tab == Tab::Notifications,
        }
    }

    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Session => "session",
            Self::Summary => "usage",
            Self::Routing => "accounts",
            Self::Snapshot => "quota snapshot",
            Self::Models => "models",
            Self::Chart => "chart",
            Self::Keys => "keys",
            Self::Notifications => "notifications",
        }
    }

    fn path(self) -> &'static str {
        match self {
            Self::Session => "/admin/session",
            Self::Summary => "/usage/summary.json",
            Self::Routing => "/admin/account-routing",
            Self::Snapshot => "/dashboard/snapshot.json",
            Self::Models => "/custom-models.json",
            Self::Chart => "/usage/context-history.json?hours=24&bucket_minutes=30",
            Self::Keys => "/admin/api-keys",
            Self::Notifications => "/notifications/settings",
        }
    }

    fn interval(self) -> Duration {
        Duration::from_secs(match self {
            Self::Summary => 5,
            Self::Routing | Self::Snapshot => 15,
            Self::Chart | Self::Keys | Self::Notifications => 30,
            Self::Session | Self::Models => 60,
        })
    }

    fn valid_body(self, body: &Value) -> bool {
        match self {
            Self::Session => body.get("enabled").is_some_and(Value::is_boolean),
            Self::Summary => body.get("totals").is_some_and(Value::is_object),
            Self::Routing => body.get("accounts").is_some_and(Value::is_array),
            Self::Snapshot => body.get("quotas").is_some_and(Value::is_object),
            Self::Models => body.get("models").is_some_and(Value::is_array),
            Self::Chart => body.get("labels").is_some_and(Value::is_array),
            Self::Keys => body.get("keys").is_some_and(Value::is_array),
            Self::Notifications => body.is_object() && body.get("text").is_none(),
        }
    }
}

#[derive(Debug)]
pub(super) struct RequestError {
    pub message: String,
    pub unauthorized: bool,
}

impl From<String> for RequestError {
    fn from(message: String) -> Self {
        Self {
            message,
            unauthorized: false,
        }
    }
}

pub(super) struct CommandOutcome {
    pub message: String,
    pub client: Option<GatewayClient>,
    pub refresh: bool,
    pub reveal: Option<String>,
    pub account_login: Option<AccountLoginPrompt>,
}

pub(super) enum AccountLoginPrompt {
    QwenCloud {
        login_id: String,
        verification_url: String,
        interval: u64,
    },
    Browser {
        provider: String,
        url: String,
        state: Option<String>,
        label: Option<String>,
        organization_uuid: Option<String>,
        base_url: Option<String>,
    },
    CopilotDevice {
        device_code: String,
        verification_uri: String,
        user_code: String,
        label: Option<String>,
        account_type: String,
    },
}

pub(super) enum NetworkEvent {
    Section {
        section: Section,
        generation: u64,
        result: Result<Value, RequestError>,
    },
    Command(Result<CommandOutcome, RequestError>),
    QuotaRefresh {
        generation: u64,
        result: Result<Value, RequestError>,
    },
}

pub(super) enum TuiCommand {
    CheckQwenCloudLogin {
        login_id: String,
        verification_url: String,
        interval: u64,
    },
    CancelQwenCloudLogin {
        login_id: String,
    },
    Login {
        otp: String,
        api_key: Option<String>,
    },
    StartAccountLogin {
        provider: String,
        label: Option<String>,
        organization_uuid: Option<String>,
        base_url: Option<String>,
        account_type: Option<String>,
    },
    CompleteAccountLogin {
        provider: String,
        input: String,
        state: Option<String>,
        project_id: Option<String>,
        label: Option<String>,
        organization_uuid: Option<String>,
        base_url: Option<String>,
        account_type: Option<String>,
        verification_uri: Option<String>,
        user_code: Option<String>,
    },
    SaveAccountCredentials {
        provider: String,
        body: Value,
    },
    ToggleAccount {
        file_name: String,
        enabled: bool,
    },
    SaveClaudeResetPolicy {
        body: Value,
    },
    ConsumeReset {
        provider: String,
        file_name: String,
        credit_id: String,
        program: String,
    },
    SaveModel {
        body: Value,
    },
    DeleteAccount {
        file_name: String,
    },
    DeleteModel {
        alias: String,
    },
    CreateApiKey {
        label: String,
        access: Value,
    },
    UpdateApiKeyAccess {
        id: String,
        access: Value,
    },
    RevokeApiKey {
        id: String,
    },
    UpdateNotificationSettings {
        body: Value,
    },
    TestNotification,
}

impl TuiCommand {
    async fn execute(self, mut client: GatewayClient) -> Result<CommandOutcome, RequestError> {
        let login = matches!(self, Self::Login { .. });
        let refresh = !matches!(
            self,
            Self::TestNotification | Self::StartAccountLogin { .. }
        );
        let creates_api_key = matches!(self, Self::CreateApiKey { .. });
        let response = match self {
            Self::CheckQwenCloudLogin {
                login_id,
                verification_url,
                interval,
            } => {
                tokio::time::sleep(Duration::from_secs(interval.clamp(5, 60))).await;
                let response = client
                    .post_json(
                        "/compatible/qwen/status",
                        Some(json!({"login_id":login_id})),
                    )
                    .await?;
                ensure_success(&response)?;
                if response.body["status"] == "connected" {
                    let _ = client.get("/compatible/quota.json").await;
                    Ok(response)
                } else {
                    return Ok(CommandOutcome {
                        message: "Waiting for Qwen Cloud authorization. Optional to show quota."
                            .into(),
                        client: None,
                        refresh: false,
                        reveal: None,
                        account_login: Some(AccountLoginPrompt::QwenCloud {
                            login_id,
                            verification_url,
                            interval: response.body["interval"].as_u64().unwrap_or(5),
                        }),
                    });
                }
            }
            Self::CancelQwenCloudLogin { login_id } => {
                client
                    .post_json(
                        "/compatible/qwen/cancel",
                        Some(json!({"login_id":login_id})),
                    )
                    .await
            }
            Self::Login { otp, api_key } => client.login(&otp, api_key.as_deref()).await,
            Self::StartAccountLogin {
                provider,
                label,
                organization_uuid,
                base_url,
                account_type,
            } => {
                let pending = account_login_start(
                    &client,
                    &provider,
                    label,
                    organization_uuid,
                    base_url,
                    account_type,
                )
                .await?;
                return Ok(CommandOutcome {
                    message: "account login started".to_string(),
                    client: None,
                    refresh: false,
                    reveal: None,
                    account_login: pending,
                });
            }
            Self::CompleteAccountLogin {
                provider,
                input,
                state,
                project_id,
                label,
                organization_uuid,
                base_url,
                account_type,
                verification_uri,
                user_code,
            } => {
                let fields = account_login_submit_fields(
                    &provider,
                    input,
                    state,
                    project_id,
                    label,
                    organization_uuid,
                    base_url,
                    account_type,
                );
                let response = client
                    .post_form(account_login_submit_path(&provider)?, &fields)
                    .await?;
                if provider == "copilot"
                    && response.status.is_success()
                    && response.body.get("ok") == Some(&Value::Bool(false))
                    && is_copilot_pending(&message_from(&response.body))
                {
                    return Ok(CommandOutcome {
                        message: "GitHub authorization is still pending; approve the device code and press Enter to check again"
                            .to_string(),
                        client: None,
                        refresh: false,
                        reveal: None,
                        account_login: Some(AccountLoginPrompt::CopilotDevice {
                            device_code: fields
                                .iter()
                                .find(|(key, _)| *key == "device_code")
                                .map(|(_, value)| value.clone())
                                .unwrap_or_default(),
                            verification_uri: verification_uri.unwrap_or_default(),
                            user_code: user_code.unwrap_or_default(),
                            label: fields
                                .iter()
                                .find(|(key, _)| *key == "label")
                                .map(|(_, value)| value.clone()),
                            account_type: fields
                                .iter()
                                .find(|(key, _)| *key == "account_type")
                                .map(|(_, value)| value.clone())
                                .unwrap_or_else(|| "individual".to_string()),
                        }),
                    });
                }
                Ok(response)
            }
            Self::SaveAccountCredentials { provider, body } => {
                let quota_action = body["quota_action"].as_str().unwrap_or("").to_string();
                let path = account_credentials_path(&provider)?;
                let response = client.post_json(path, Some(body)).await?;
                if provider == "compatible" && response.status.is_success() {
                    ensure_success(&response)?;
                    let file_name = response.body["file_name"]
                        .as_str()
                        .ok_or_else(|| "No saved account was returned".to_string())?;
                    if quota_action == "connect" {
                        let login = client
                            .post_json(
                                "/compatible/qwen/connect",
                                Some(json!({"file_name":file_name})),
                            )
                            .await?;
                        ensure_success(&login)?;
                        let login_id = login.body["login_id"]
                            .as_str()
                            .ok_or_else(|| "No Qwen Cloud sign-in was returned".to_string())?
                            .to_string();
                        let verification_url = login.body["verification_url"]
                            .as_str()
                            .ok_or_else(|| "No Qwen Cloud sign-in URL was returned".to_string())?
                            .to_string();
                        let url = url::Url::parse(&verification_url)
                            .map_err(|_| "Invalid Qwen Cloud sign-in URL".to_string())?;
                        if url.scheme() != "https"
                            || !url.host_str().is_some_and(|host| {
                                host == "qwencloud.com" || host.ends_with(".qwencloud.com")
                            })
                        {
                            return Err("Invalid Qwen Cloud sign-in URL".to_string().into());
                        }
                        return Ok(CommandOutcome { message:"API account saved. Connect Qwen Cloud account — Optional to show quota.".into(),
                            client:None, refresh:true, reveal:None,
                            account_login:Some(AccountLoginPrompt::QwenCloud {login_id, verification_url,
                                interval:login.body["interval"].as_u64().unwrap_or(5)}) });
                    }
                    if quota_action == "disconnect" {
                        let disconnected = client
                            .post_json(
                                "/compatible/qwen/disconnect",
                                Some(json!({"file_name":file_name})),
                            )
                            .await?;
                        ensure_success(&disconnected)?;
                    }
                    // Populate the shared snapshot after adding/editing a Qwen
                    // account so its quota state appears immediately in the TUI.
                    let _ = client.get("/compatible/quota.json").await;
                }
                Ok(response)
            }
            Self::ToggleAccount { file_name, enabled } => {
                client
                    .post_form(
                        "/credentials/toggle",
                        &[("file_name", file_name), ("enabled", enabled.to_string())],
                    )
                    .await
            }
            Self::SaveClaudeResetPolicy { body } => {
                client
                    .post_json("/admin/claude/reset-credit-automation", Some(body))
                    .await
            }
            Self::ConsumeReset {
                provider,
                file_name,
                credit_id,
                program,
            } => {
                if !["codex", "claude"].contains(&provider.as_str()) {
                    return Err("Only Codex and Claude expose reset controls"
                        .to_string()
                        .into());
                }
                let mut fields = vec![("file_name", file_name), ("credit_id", credit_id)];
                if provider == "claude" {
                    fields.push(("program", program));
                }
                client
                    .post_form(
                        &format!("/{provider}/rate-limit-reset-credit/consume"),
                        &fields,
                    )
                    .await
            }
            Self::SaveModel { body } => client.post_json("/custom-models/save", Some(body)).await,
            Self::DeleteAccount { file_name } => {
                client
                    .post_form("/credentials/delete", &[("file_name", file_name)])
                    .await
            }
            Self::DeleteModel { alias } => {
                client
                    .post_json("/custom-models/delete", Some(json!({ "alias": alias })))
                    .await
            }
            Self::CreateApiKey { label, access } => {
                client
                    .post_json(
                        "/admin/api-keys/create",
                        Some(json!({ "label": label, "access": access })),
                    )
                    .await
            }
            Self::UpdateApiKeyAccess { id, access } => {
                client
                    .post_json(
                        "/admin/api-keys/access",
                        Some(json!({ "id": id, "access": access })),
                    )
                    .await
            }
            Self::RevokeApiKey { id } => {
                client
                    .post_json("/admin/api-keys/revoke", Some(json!({ "id": id })))
                    .await
            }
            Self::UpdateNotificationSettings { body } => {
                client
                    .post_json("/notifications/settings", Some(body))
                    .await
            }
            Self::TestNotification => client.post_json("/notifications/test", None).await,
        }?;
        ensure_success(&response).map_err(|message| RequestError {
            message,
            unauthorized: matches!(response.status.as_u16(), 401 | 403),
        })?;
        if response.body.get("ok") == Some(&Value::Bool(false)) {
            return Err(RequestError {
                message: message_from(&response.body),
                unauthorized: matches!(response.status.as_u16(), 401 | 403),
            });
        }
        Ok(CommandOutcome {
            message: if login {
                "logged in".to_string()
            } else if creates_api_key {
                "API key created; copy the revealed secret now".to_string()
            } else {
                message_from(&response.body)
            },
            client: login.then_some(client),
            refresh,
            reveal: creates_api_key
                .then(|| response.body.get("plain_text_key").and_then(Value::as_str))
                .flatten()
                .map(str::to_string),
            account_login: None,
        })
    }
}

async fn account_login_start(
    client: &GatewayClient,
    provider: &str,
    label: Option<String>,
    organization_uuid: Option<String>,
    base_url: Option<String>,
    account_type: Option<String>,
) -> Result<Option<AccountLoginPrompt>, RequestError> {
    let response = match provider {
        "codex" => client.get("/login/codex/start").await,
        "antigravity" => client.get("/login/antigravity/start").await,
        "gemini" => client.get("/login/gemini/start").await,
        "grok" => client.get("/login/grok/start").await,
        "claude" => {
            let path = query_path(
                "/login/claude/start",
                [
                    ("label", label.as_deref()),
                    ("organization_uuid", organization_uuid.as_deref()),
                    ("base_url", base_url.as_deref()),
                ],
            );
            client.get(&path).await
        }
        "copilot" => {
            let mut body = serde_json::Map::new();
            if let Some(label) = label.as_deref().filter(|value| !value.is_empty()) {
                body.insert("label".to_string(), Value::String(label.to_string()));
            }
            if let Some(account_type) = account_type.as_deref().filter(|value| !value.is_empty()) {
                body.insert(
                    "account_type".to_string(),
                    Value::String(account_type.to_string()),
                );
            }
            client
                .post_json("/login/copilot/start", Some(Value::Object(body)))
                .await
        }
        _ => return Err(format!("unsupported account provider: {provider}").into()),
    }
    .map_err(RequestError::from)?;
    ensure_action_success(&response)?;
    let pending = match provider {
        "copilot" => AccountLoginPrompt::CopilotDevice {
            device_code: string_field(&response.body, "device_code")?,
            verification_uri: string_field(&response.body, "verification_uri")?,
            user_code: string_field(&response.body, "user_code")?,
            label,
            account_type: account_type.unwrap_or_else(|| "individual".to_string()),
        },
        _ => AccountLoginPrompt::Browser {
            provider: provider.to_string(),
            url: string_field(&response.body, "url")?,
            state: response
                .body
                .get("state")
                .and_then(Value::as_str)
                .map(str::to_string),
            label,
            organization_uuid,
            base_url,
        },
    };
    Ok(Some(pending))
}

fn account_login_submit_path(provider: &str) -> Result<&'static str, RequestError> {
    match provider {
        "codex" => Ok("/login/codex/submit"),
        "antigravity" => Ok("/login/antigravity/submit"),
        "gemini" => Ok("/login/gemini/submit"),
        "grok" => Ok("/login/grok/submit"),
        "claude" => Ok("/login/claude/submit"),
        "copilot" => Ok("/login/copilot/submit"),
        _ => Err(format!("unsupported account provider: {provider}").into()),
    }
}

fn account_credentials_path(provider: &str) -> Result<&'static str, RequestError> {
    match provider {
        "qwen" => Ok("/login/qwen/start"),
        "deepseek" => Ok("/login/deepseek/start"),
        "minimax" => Ok("/login/minimax/start"),
        "glm" => Ok("/login/glm/start"),
        "compatible" => Ok("/compatible/save"),
        "copilot" => Ok("/login/copilot/start"),
        "claude" => Ok("/login/claude/start"),
        _ => Err(format!("unsupported account provider: {provider}").into()),
    }
}

fn account_login_submit_fields(
    provider: &str,
    input: String,
    state: Option<String>,
    project_id: Option<String>,
    label: Option<String>,
    organization_uuid: Option<String>,
    base_url: Option<String>,
    account_type: Option<String>,
) -> Vec<(&'static str, String)> {
    let mut fields = vec![(
        if provider == "copilot" {
            "device_code"
        } else {
            "redirect_url"
        },
        input,
    )];
    if let Some(value) = state.filter(|value| !value.is_empty()) {
        fields.push(("state", value));
    }
    if let Some(value) = project_id.filter(|value| !value.is_empty()) {
        fields.push(("project_id", value));
    }
    if let Some(value) = label.filter(|value| !value.is_empty()) {
        fields.push(("label", value));
    }
    if let Some(value) = organization_uuid.filter(|value| !value.is_empty()) {
        fields.push(("organization_uuid", value));
    }
    if let Some(value) = base_url.filter(|value| !value.is_empty()) {
        fields.push(("base_url", value));
    }
    if let Some(value) = account_type.filter(|value| !value.is_empty()) {
        fields.push(("account_type", value));
    }
    fields
}

fn query_path<const N: usize>(base: &str, fields: [(&str, Option<&str>); N]) -> String {
    let query = fields
        .into_iter()
        .filter_map(|(key, value)| {
            value
                .filter(|value| !value.is_empty())
                .map(|value| (key, value))
        })
        .map(|(key, value)| format!("{}={}", super::url_encode(key), super::url_encode(value)))
        .collect::<Vec<_>>()
        .join("&");
    if query.is_empty() {
        base.to_string()
    } else {
        format!("{base}?{query}")
    }
}

fn ensure_action_success(response: &ApiResponse) -> Result<(), RequestError> {
    ensure_success(response).map_err(|message| RequestError {
        message,
        unauthorized: matches!(response.status.as_u16(), 401 | 403),
    })?;
    if response.body.get("ok") == Some(&Value::Bool(false)) {
        return Err(RequestError {
            message: message_from(&response.body),
            unauthorized: matches!(response.status.as_u16(), 401 | 403),
        });
    }
    Ok(())
}

fn string_field(value: &Value, key: &str) -> Result<String, RequestError> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| format!("account login response did not include {key}").into())
}

fn is_copilot_pending(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    message.contains("pending")
        || message.contains("slow_down")
        || message.contains("waiting")
        || message.contains("authorization_pending")
}

#[derive(Default)]
struct FetchState {
    manual: bool,
    generation: u64,
    task: Option<JoinHandle<()>>,
    attempted_at: Option<Instant>,
    succeeded_at: Option<Instant>,
    error: Option<String>,
    failures: u32,
    retry_at: Option<Instant>,
}

pub(super) struct TuiNetwork {
    sections: [FetchState; 8],
    tx: mpsc::Sender<NetworkEvent>,
    rx: mpsc::Receiver<NetworkEvent>,
    command: Option<JoinHandle<()>>,
    logging_in: bool,
    refresh: Option<JoinHandle<()>>,
    refresh_generation: u64,
}

impl TuiNetwork {
    pub(super) fn new() -> Self {
        // Eight section results, one action and one quota-refresh result fit.
        let (tx, rx) = mpsc::channel(16);
        Self {
            sections: std::array::from_fn(|_| FetchState::default()),
            tx,
            rx,
            command: None,
            logging_in: false,
            refresh: None,
            refresh_generation: 0,
        }
    }

    pub(super) fn schedule(&mut self, client: &GatewayClient, tab: Tab, authenticated: bool) {
        if self.logging_in {
            return;
        }
        // A newly selected tab must not queue behind work for hidden panels.
        for section in Section::ALL
            .into_iter()
            .filter(|section| !section.visible(tab))
        {
            let state = &mut self.sections[section as usize];
            if !state.manual {
                if let Some(task) = state.task.take() {
                    task.abort();
                    state.generation += 1;
                    state.attempted_at = None;
                }
            }
        }
        let now = Instant::now();
        let mut active = self
            .sections
            .iter()
            .filter(|state| state.task.is_some())
            .count()
            + usize::from(self.refresh.is_some());
        for section in Section::ALL {
            if active >= MAX_FETCHES {
                break;
            }
            if (!section.visible(tab) && !self.sections[section as usize].manual)
                || (section != Section::Session && !authenticated)
                || (section == Section::Snapshot && self.refresh.is_some())
            {
                continue;
            }
            // Keep a slot available for counters even when metadata endpoints
            // are slow. Authentication still gates the initial data requests.
            if section != Section::Summary
                && self
                    .sections
                    .iter()
                    .enumerate()
                    .filter(|(index, state)| {
                        *index != Section::Summary as usize && state.task.is_some()
                    })
                    .count()
                    >= MAX_FETCHES - 1
            {
                continue;
            }
            let state = &mut self.sections[section as usize];
            let due = match state.retry_at {
                Some(at) => now >= at,
                None => state
                    .attempted_at
                    .is_none_or(|at| now.duration_since(at) >= section.interval()),
            };
            if state.task.is_some() || !due {
                continue;
            }
            state.generation += 1;
            let generation = state.generation;
            let client = client.clone();
            let tx = self.tx.clone();
            state.attempted_at = Some(now);
            state.task = Some(tokio::spawn(async move {
                let result = async {
                    let response = client.get(section.path()).await?;
                    ensure_success(&response).map_err(|message| RequestError {
                        message,
                        unauthorized: matches!(response.status.as_u16(), 401 | 403),
                    })?;
                    if !section.valid_body(&response.body) {
                        return Err(format!("invalid {} response", section.label()).into());
                    }
                    Ok(response.body)
                }
                .await;
                let _ = tx
                    .send(NetworkEvent::Section {
                        section,
                        generation,
                        result,
                    })
                    .await;
            }));
            active += 1;
        }
    }

    pub(super) fn refresh_visible(&mut self, tab: Tab) {
        for section in Section::ALL
            .into_iter()
            .filter(|section| section.visible(tab))
        {
            let state = &mut self.sections[section as usize];
            // An existing request already satisfies another refresh keypress.
            if state.task.is_none() {
                state.attempted_at = None;
                state.retry_at = None;
            }
        }
    }

    pub(super) fn refresh_all(&mut self) {
        for state in &mut self.sections {
            state.manual = true;
            if state.task.is_none() {
                state.attempted_at = None;
                state.retry_at = None;
            }
        }
    }

    fn invalidate_snapshot(&mut self) {
        let state = &mut self.sections[Section::Snapshot as usize];
        if let Some(task) = state.task.take() {
            task.abort();
        }
        state.generation += 1;
        state.attempted_at = None;
        state.retry_at = None;
    }

    pub(super) fn refresh_pending(&self) -> bool {
        self.refresh.is_some()
    }

    pub(super) fn reads_pending(&self) -> bool {
        self.sections
            .iter()
            .any(|state| state.manual || state.task.is_some())
    }

    pub(super) fn refresh_quotas(
        &mut self,
        client: &GatewayClient,
        target: Option<(String, String)>,
    ) -> bool {
        if self.refresh.is_some() || self.command.is_some() {
            return false;
        }
        self.invalidate_snapshot();
        self.refresh_generation += 1;
        let generation = self.refresh_generation;
        let client = client.clone();
        let tx = self.tx.clone();
        self.refresh = Some(tokio::spawn(async move {
            let result = async {
                let fields = target
                    .into_iter()
                    .flat_map(|(provider, file)| [("provider", provider), ("file_name", file)])
                    .collect::<Vec<_>>();
                let response = client.post_form("/models/refresh", &fields).await?;
                ensure_action_success(&response)?;
                let response = client.get(Section::Snapshot.path()).await?;
                ensure_action_success(&response)?;
                if !Section::Snapshot.valid_body(&response.body) {
                    return Err("invalid quota snapshot response".to_string().into());
                }
                Ok(response.body)
            }
            .await;
            let _ = tx
                .send(NetworkEvent::QuotaRefresh { generation, result })
                .await;
        }));
        true
    }

    pub(super) fn invalidate(&mut self, include_session: bool) {
        for section in Section::ALL {
            if section == Section::Session && !include_session {
                continue;
            }
            let state = &mut self.sections[section as usize];
            state.manual = false;
            if let Some(task) = state.task.take() {
                task.abort();
            }
            state.generation += 1;
            state.attempted_at = None;
            state.retry_at = None;
        }
    }

    pub(super) fn forget_data(&mut self) {
        if let Some(task) = self.refresh.take() {
            task.abort();
        }
        self.refresh_generation += 1;
        self.invalidate(false);
        for section in Section::ALL
            .into_iter()
            .filter(|section| *section != Section::Session)
        {
            let state = &mut self.sections[section as usize];
            state.succeeded_at = None;
            state.error = None;
            state.failures = 0;
        }
    }

    pub(super) fn start_command(&mut self, client: &GatewayClient, command: TuiCommand) -> bool {
        if self.command.is_some() || self.refresh.is_some() {
            return false;
        }
        self.logging_in = matches!(command, TuiCommand::Login { .. });
        if self.logging_in {
            self.invalidate(true);
        }
        let client = client.clone();
        let tx = self.tx.clone();
        self.command = Some(tokio::spawn(async move {
            let _ = tx
                .send(NetworkEvent::Command(command.execute(client).await))
                .await;
        }));
        true
    }

    pub(super) fn command_pending(&self) -> bool {
        self.command.is_some()
    }

    pub(super) fn next_event(&mut self) -> Option<NetworkEvent> {
        while let Ok(event) = self.rx.try_recv() {
            match &event {
                NetworkEvent::Section {
                    section,
                    generation,
                    result,
                } => {
                    let state = &mut self.sections[*section as usize];
                    if state.generation != *generation {
                        continue;
                    }
                    state.task = None;
                    state.manual = false;
                    match result {
                        Ok(_) => {
                            state.succeeded_at = Some(Instant::now());
                            state.error = None;
                            state.failures = 0;
                            state.retry_at = None;
                        }
                        Err(error) => {
                            state.error = Some(error.message.clone());
                            state.failures = state.failures.saturating_add(1);
                            let backoff =
                                Duration::from_secs((5u64 << (state.failures.min(5) - 1)).min(60));
                            state.retry_at = Some(Instant::now() + backoff);
                        }
                    }
                }
                NetworkEvent::Command(_) => {
                    self.command = None;
                    self.logging_in = false;
                }
                NetworkEvent::QuotaRefresh { generation, result } => {
                    if *generation != self.refresh_generation {
                        continue;
                    }
                    self.refresh = None;
                    // A read begun before the POST must never replace its result.
                    self.invalidate_snapshot();
                    let state = &mut self.sections[Section::Snapshot as usize];
                    state.manual = false;
                    state.attempted_at = Some(Instant::now());
                    if result.is_ok() {
                        state.succeeded_at = Some(Instant::now());
                        state.error = None;
                        state.failures = 0;
                    } else if let Err(error) = result {
                        state.error = Some(error.message.clone());
                    }
                }
            }
            return Some(event);
        }
        None
    }

    pub(super) fn has_data(&self, section: Section) -> bool {
        self.sections[section as usize].succeeded_at.is_some()
    }

    pub(super) fn age(&self, section: Section) -> Option<Duration> {
        self.sections[section as usize]
            .succeeded_at
            .map(|at| at.elapsed())
    }

    pub(super) fn status(&self, tab: Tab) -> String {
        let mut sections = Section::ALL
            .into_iter()
            .filter(|section| {
                section.visible(tab)
                    && (*section != Section::Session
                        || self.sections[*section as usize].error.is_some())
            })
            .collect::<Vec<_>>();
        // Keep failures visible even when the terminal clips a long footer.
        sections.sort_by_key(|section| self.sections[*section as usize].error.is_none());
        sections
            .into_iter()
            .map(|section| {
                let state = &self.sections[section as usize];
                let age = state
                    .succeeded_at
                    .map(|at| format!("{}s ago", at.elapsed().as_secs()));
                if let Some(error) = &state.error {
                    format!(
                        "{} {}: {}",
                        section.label(),
                        age.map(|age| format!("stale ({age})"))
                            .unwrap_or_else(|| "unavailable".into()),
                        super::short_message(error)
                    )
                } else {
                    format!(
                        "{} {}{}",
                        section.label(),
                        age.unwrap_or_else(|| "loading".into()),
                        if state.task.is_some() && state.succeeded_at.is_some() {
                            " (refreshing)"
                        } else {
                            ""
                        }
                    )
                }
            })
            .collect::<Vec<_>>()
            .join(" | ")
    }

    pub(super) fn connection_label(&self, tab: Tab, session: &Value) -> &'static str {
        if bool_at(session, &["enabled"]) && !bool_at(session, &["authenticated"]) {
            "AUTH"
        } else if !self.has_data(Section::Session) {
            "CONNECTING"
        } else if Section::ALL
            .into_iter()
            .any(|section| section.visible(tab) && self.sections[section as usize].error.is_some())
        {
            "STALE"
        } else {
            "CONNECTED"
        }
    }
}

impl Drop for TuiNetwork {
    fn drop(&mut self) {
        self.invalidate(true);
        if let Some(task) = self.command.take() {
            task.abort();
        }
        if let Some(task) = self.refresh.take() {
            task.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::{ConfirmationAction, KeyRow, OverviewSection, TuiApp};
    use super::*;
    use axum::{
        body::to_bytes,
        extract::{Request, State},
        http::StatusCode,
        response::{IntoResponse, Response},
        Router,
    };
    use std::sync::{Arc, Mutex};
    use tokio::sync::Semaphore;

    #[derive(Clone)]
    struct Reply {
        status: StatusCode,
        body: Value,
        gate: Option<Arc<Semaphore>>,
    }

    #[derive(Default)]
    struct MockState {
        replies: Mutex<std::collections::HashMap<String, Reply>>,
        requests: Mutex<Vec<(String, String, Vec<u8>, String)>>,
    }

    struct MockGateway {
        state: Arc<MockState>,
        server: JoinHandle<()>,
        base_url: String,
    }

    impl MockGateway {
        async fn new() -> Self {
            let state = Arc::new(MockState::default());
            let router = Router::new()
                .fallback(mock_request)
                .with_state(state.clone());
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base_url = format!("http://{}", listener.local_addr().unwrap());
            let server = tokio::spawn(async move {
                axum::serve(listener, router).await.unwrap();
            });
            let mock = Self {
                state,
                server,
                base_url,
            };
            for (section, body) in [
                (
                    Section::Session,
                    json!({"enabled":false,"authenticated":true}),
                ),
                (
                    Section::Summary,
                    json!({"totals":{"requests":111},"providers":{}}),
                ),
                (Section::Routing, json!({"settings":{},"accounts":[]})),
                (Section::Snapshot, json!({"quotas":{}})),
                (
                    Section::Models,
                    json!({"models":[{"alias":"original","enabled":true}]}),
                ),
                (
                    Section::Chart,
                    json!({"labels":["now"],"buckets":[{"input_tokens":7,"total_tokens":7}]}),
                ),
                (Section::Keys, json!({"keys":[]})),
                (Section::Notifications, json!({"enabled":true})),
            ] {
                mock.reply(section.path(), StatusCode::OK, body, None);
            }
            mock
        }

        fn reply(&self, path: &str, status: StatusCode, body: Value, gate: Option<Arc<Semaphore>>) {
            self.state
                .replies
                .lock()
                .unwrap()
                .insert(path.to_string(), Reply { status, body, gate });
        }

        fn count(&self, path: &str) -> usize {
            self.state
                .requests
                .lock()
                .unwrap()
                .iter()
                .filter(|(_, seen, _, _)| seen == path)
                .count()
        }

        fn app(&self) -> TuiApp {
            TuiApp::new(GatewayClient::new(self.base_url.clone()).unwrap()).unwrap()
        }
    }

    impl Drop for MockGateway {
        fn drop(&mut self) {
            self.server.abort();
        }
    }

    async fn mock_request(State(state): State<Arc<MockState>>, request: Request) -> Response {
        let path = request.uri().to_string();
        let method = request.method().to_string();
        let encoding = request
            .headers()
            .get("accept-encoding")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        let body = to_bytes(request.into_body(), 1024 * 1024).await.unwrap();
        state
            .requests
            .lock()
            .unwrap()
            .push((method, path.clone(), body.to_vec(), encoding));
        let reply = state.replies.lock().unwrap().get(&path).cloned();
        let Some(reply) = reply else {
            return StatusCode::NOT_FOUND.into_response();
        };
        if let Some(gate) = reply.gate {
            gate.acquire().await.unwrap().forget();
        }
        (reply.status, axum::Json(reply.body)).into_response()
    }

    async fn drive(app: &mut TuiApp, mut ready: impl FnMut(&TuiApp) -> bool) {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                app.poll_network();
                if ready(app) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("background requests made progress");
    }

    #[tokio::test]
    async fn focused_refresh_captures_target_coalesces_and_keeps_navigation_live() {
        let mock = MockGateway::new().await;
        let gate = Arc::new(Semaphore::new(0));
        mock.reply(
            "/models/refresh",
            StatusCode::OK,
            json!({"ok":true}),
            Some(gate.clone()),
        );
        mock.reply(
            Section::Routing.path(),
            StatusCode::OK,
            json!({"accounts":[
                {"provider":"codex","key":"one","credential_file":"one.json","enabled":true},
                {"provider":"gemini","key":"two","credential_file":"two.json","enabled":true}
            ]}),
            None,
        );
        let mut app = mock.app();
        drive(&mut app, |app| {
            app.data.accounts.len() == 2 && app.network.has_data(Section::Snapshot)
        })
        .await;
        app.overview_section = OverviewSection::AccountUsage;
        app.overview_account_selected = 0;
        app.sync_selected_from_overview_section();
        let expected = app.data.accounts[selected_overview_account_index_for_test(&app)]
            .file_name
            .clone();
        app.refresh_quota(false);
        app.move_selection(1);
        for _ in 0..20 {
            app.refresh_quota(false);
            app.poll_network();
        }
        drive(&mut app, |_| mock.count("/models/refresh") == 1).await;
        assert!(app.network.refresh_pending());
        assert!(!app.has_modal());
        let requests = mock.state.requests.lock().unwrap();
        let (_, _, body, _) = requests
            .iter()
            .find(|(method, path, _, _)| method == "POST" && path == "/models/refresh")
            .unwrap();
        let form = String::from_utf8(body.clone()).unwrap();
        assert!(form.contains(&format!("file_name={expected}")), "{form}");
        assert!(form.contains("provider=cod"), "{form}");
        drop(requests);
        mock.reply(
            Section::Snapshot.path(),
            StatusCode::OK,
            json!({"quotas":{"codex":{"accounts":[{"key":"one","used_percent":87}]}}}),
            None,
        );
        gate.add_permits(1);
        drive(&mut app, |app| {
            !app.network.refresh_pending()
                && app
                    .data
                    .quotas
                    .iter()
                    .any(|quota| quota.used_percent_value == Some(87.0))
        })
        .await;
        assert_eq!(mock.count("/models/refresh"), 1);
        assert!(app.network.has_data(Section::Snapshot));
    }

    fn selected_overview_account_index_for_test(app: &TuiApp) -> usize {
        super::super::selected_overview_account_index(app).unwrap()
    }

    #[tokio::test]
    async fn all_data_is_read_only_but_hard_reload_posts_unscoped_and_errors_recover() {
        let mock = MockGateway::new().await;
        let mut app = mock.app();
        drive(&mut app, |app| app.network.has_data(Section::Chart)).await;
        app.refresh();
        drive(&mut app, |app| {
            app.network.has_data(Section::Keys)
                && app.network.has_data(Section::Notifications)
                && !app.network.reads_pending()
        })
        .await;
        assert_eq!(mock.count("/models/refresh"), 0);
        mock.reply(
            "/models/refresh",
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({"message":"refresh failed"}),
            None,
        );
        app.refresh_quota(true);
        drive(&mut app, |app| !app.network.refresh_pending()).await;
        assert!(app.message.contains("refresh failed"));
        assert!(app.network.has_data(Section::Snapshot));
        mock.reply("/models/refresh", StatusCode::OK, json!({"ok":true}), None);
        app.refresh_quota(true);
        drive(&mut app, |app| {
            !app.network.refresh_pending() && app.message == "quota refresh complete"
        })
        .await;
        assert_eq!(mock.count("/models/refresh"), 2);
        assert!(mock
            .state
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|(method, path, _, _)| method == "POST" && path == "/models/refresh")
            .all(|(_, _, body, _)| body.is_empty()));
    }

    #[tokio::test]
    async fn missing_identity_and_unauthenticated_refresh_never_fan_out() {
        let mock = MockGateway::new().await;
        let mut app = mock.app();
        app.refresh_quota(true);
        assert!(app.message.contains("login required"));
        drive(&mut app, |app| app.network.has_data(Section::Snapshot)).await;
        app.refresh_quota(false);
        assert!(app.message.contains("no focused account"));
        assert_eq!(mock.count("/models/refresh"), 0);
        mock.reply(
            "/models/refresh",
            StatusCode::UNAUTHORIZED,
            json!({"message":"login required"}),
            None,
        );
        app.refresh_quota(true);
        drive(&mut app, |app| !app.network.refresh_pending()).await;
        assert!(app.needs_login());
        assert!(app.data.quotas.is_empty());
    }

    #[tokio::test]
    async fn counters_progress_with_two_blocked_sections_and_refreshes_coalesce() {
        let mock = MockGateway::new().await;
        let gate = Arc::new(Semaphore::new(0));
        for section in [Section::Routing, Section::Snapshot] {
            mock.reply(
                section.path(),
                StatusCode::OK,
                json!({}),
                Some(gate.clone()),
            );
        }
        let mut app = mock.app();
        drive(&mut app, |app| {
            app.data.summary["totals"]["requests"] == 111
                && mock.count(Section::Snapshot.path()) == 1
        })
        .await;
        mock.reply(
            Section::Summary.path(),
            StatusCode::OK,
            json!({"totals":{"requests":222}}),
            None,
        );
        for _ in 0..100 {
            app.refresh();
            app.poll_network();
        }
        drive(&mut app, |app| {
            app.data.summary["totals"]["requests"] == 222
        })
        .await;
        assert_eq!(mock.count(Section::Routing.path()), 1);
        assert_eq!(mock.count(Section::Snapshot.path()), 1);
        assert!(
            app.network
                .sections
                .iter()
                .filter(|state| state.task.is_some())
                .count()
                <= MAX_FETCHES
        );
        assert_eq!(mock.count(Section::Keys.path()), 0);
        assert_eq!(mock.count(Section::Notifications.path()), 0);
        let requests = mock.state.requests.lock().unwrap();
        assert!(requests.iter().all(|(_, path, _, encoding)| !path
            .starts_with("/usage/history.json")
            && encoding.contains("gzip")));
    }

    #[tokio::test]
    async fn a_failed_section_preserves_its_data_without_losing_fresh_counters_or_chart() {
        let mock = MockGateway::new().await;
        let mut app = mock.app();
        drive(&mut app, |app| {
            app.network.has_data(Section::Models) && app.network.has_data(Section::Chart)
        })
        .await;
        assert!(!app.data.usage_buckets.is_empty());
        mock.reply(
            Section::Models.path(),
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({"message":"model failure"}),
            None,
        );
        mock.reply(
            Section::Summary.path(),
            StatusCode::OK,
            json!({"totals":{"requests":222}}),
            None,
        );
        app.refresh();
        drive(&mut app, |app| {
            app.data.summary["totals"]["requests"] == 222
                && app.network.sections[Section::Models as usize]
                    .error
                    .is_some()
        })
        .await;
        assert_eq!(app.data.models[0].alias, "original");
        assert!(!app.data.usage_buckets.is_empty());
        assert!(app.network.status(Tab::Overview).contains("models stale"));
    }

    #[tokio::test]
    async fn changing_tabs_loads_keys_while_hidden_requests_are_blocked() {
        let mock = MockGateway::new().await;
        let gate = Arc::new(Semaphore::new(0));
        for section in [Section::Routing, Section::Snapshot] {
            mock.reply(
                section.path(),
                StatusCode::OK,
                json!({}),
                Some(gate.clone()),
            );
        }
        let mut app = mock.app();
        drive(&mut app, |_| mock.count(Section::Snapshot.path()) == 1).await;
        app.tab = Tab::Keys;
        drive(&mut app, |app| app.network.has_data(Section::Keys)).await;
        assert_eq!(mock.count(Section::Keys.path()), 1);
        assert!(app.network.sections[Section::Snapshot as usize]
            .task
            .is_none());
    }

    #[tokio::test]
    async fn invalidated_queued_responses_cannot_overwrite_newer_data() {
        let mock = MockGateway::new().await;
        let mut app = mock.app();
        app.data.summary = json!({"totals":{"requests":222}});
        let generation = app.network.sections[Section::Summary as usize].generation;
        app.network
            .tx
            .try_send(NetworkEvent::Section {
                section: Section::Summary,
                generation,
                result: Ok(json!({"totals":{"requests":111}})),
            })
            .unwrap_or_else(|_| panic!("queue result"));
        app.network.invalidate(true);
        app.poll_network();
        assert_eq!(app.data.summary["totals"]["requests"], 222);
    }

    #[tokio::test]
    async fn authentication_gates_fetches_and_expiry_clears_protected_data() {
        let mock = MockGateway::new().await;
        mock.reply(
            Section::Session.path(),
            StatusCode::OK,
            json!({"enabled":true,"authenticated":false}),
            None,
        );
        let mut app = mock.app();
        drive(&mut app, |app| app.needs_login()).await;
        assert_eq!(mock.count(Section::Summary.path()), 0);
        mock.reply(
            Section::Session.path(),
            StatusCode::OK,
            json!({"enabled":true,"authenticated":true}),
            None,
        );
        app.refresh();
        drive(&mut app, |app| app.network.has_data(Section::Models)).await;
        mock.reply(
            Section::Session.path(),
            StatusCode::OK,
            json!({"enabled":true,"authenticated":false}),
            None,
        );
        mock.reply(
            Section::Models.path(),
            StatusCode::UNAUTHORIZED,
            json!({"message":"expired"}),
            None,
        );
        app.refresh();
        drive(&mut app, |app| app.needs_login()).await;
        assert!(app.data.models.is_empty());
        assert!(app.data.summary.is_null());
        assert!(app.confirmation_modal.is_none());
    }

    #[tokio::test]
    async fn confirmations_keep_the_original_target_and_actions_do_not_overlap() {
        let mock = MockGateway::new().await;
        let gate = Arc::new(Semaphore::new(0));
        mock.reply(
            "/credentials/delete",
            StatusCode::OK,
            json!({"message":"deleted"}),
            Some(gate.clone()),
        );
        let mut app = mock.app();
        drive(&mut app, |app| app.network.has_data(Section::Session)).await;
        app.tab = Tab::Overview;
        app.overview_section = OverviewSection::AccountUsage;
        app.apply_section(
            Section::Routing,
            json!({"accounts":[
                {"provider":"codex","key":"one","label":"One","credential_file":"one.json"},
                {"provider":"codex","key":"two","label":"Two","credential_file":"two.json"}
            ]}),
        );
        app.request_delete_confirmation();
        assert!(
            matches!(app.confirmation_modal.as_ref().unwrap().action, ConfirmationAction::DeleteAccount { ref file_name } if file_name == "one.json")
        );
        app.selected = 1;
        app.confirm_selection();
        app.start_command(TuiCommand::TestNotification);
        drive(&mut app, |_| mock.count("/credentials/delete") == 1).await;
        assert_eq!(mock.count("/notifications/test"), 0);
        app.move_selection(-1);
        assert_eq!(app.selected, 0);
        let requests = mock.state.requests.lock().unwrap();
        let (_, _, body, _) = requests
            .iter()
            .find(|(_, path, _, _)| path == "/credentials/delete")
            .unwrap();
        let form: std::collections::HashMap<String, String> =
            serde_urlencoded::from_bytes(body).unwrap();
        assert_eq!(form["file_name"], "one.json");
        drop(requests);
        gate.add_permits(1);
        drive(&mut app, |app| !app.network.command_pending()).await;
    }

    #[tokio::test]
    async fn api_key_management_uses_dashboard_routes_and_reveals_new_secret() {
        let mock = MockGateway::new().await;
        mock.reply(
            "/admin/api-keys/create",
            StatusCode::OK,
            json!({
                "ok": true,
                "plain_text_key": "iogw-secret-once",
                "keys": [],
                "quota_summaries": {},
                "accounts": []
            }),
            None,
        );
        mock.reply(
            "/admin/api-keys/access",
            StatusCode::OK,
            json!({"ok":true,"keys":[],"quota_summaries":{},"accounts":[]}),
            None,
        );
        mock.reply(
            "/admin/api-keys/revoke",
            StatusCode::OK,
            json!({"ok":true,"keys":[],"quota_summaries":{},"accounts":[]}),
            None,
        );
        let mut app = mock.app();
        drive(&mut app, |app| app.network.has_data(Section::Session)).await;
        app.tab = Tab::Keys;
        app.start_command(TuiCommand::CreateApiKey {
            label: "Dashboard key".to_string(),
            access: json!({"all":true,"providers":[]}),
        });
        drive(&mut app, |app| app.api_key_reveal == "iogw-secret-once").await;
        let requests = mock.state.requests.lock().unwrap();
        let (_, _, body, _) = requests
            .iter()
            .find(|(_, path, _, _)| path == "/admin/api-keys/create")
            .unwrap();
        let body: Value = serde_json::from_slice(body).unwrap();
        assert_eq!(body["label"], "Dashboard key");
        assert_eq!(body["access"]["all"], true);
        drop(requests);

        app.data.keys = vec![KeyRow {
            id: "key-1".to_string(),
            label: "Dashboard key".to_string(),
            access_json: json!({"all":true,"providers":[]}),
            ..KeyRow::default()
        }];
        app.selected = 0;
        app.start_command(TuiCommand::UpdateApiKeyAccess {
            id: "key-1".to_string(),
            access: json!({"all":false,"providers":[]}),
        });
        drive(&mut app, |app| !app.network.command_pending()).await;
        app.data.keys = vec![KeyRow {
            id: "key-1".to_string(),
            label: "Dashboard key".to_string(),
            access_json: json!({"all":false,"providers":[]}),
            ..KeyRow::default()
        }];
        app.selected = 0;
        app.request_delete_confirmation();
        app.confirm_selection();
        drive(&mut app, |app| !app.network.command_pending()).await;
        assert_eq!(mock.count("/admin/api-keys/access"), 1);
        assert_eq!(mock.count("/admin/api-keys/revoke"), 1);
    }

    #[tokio::test]
    async fn account_add_uses_the_web_provider_route_and_json_payload() {
        let mock = MockGateway::new().await;
        mock.reply(
            "/login/deepseek/start",
            StatusCode::OK,
            json!({"ok":true,"message":"saved DeepSeek account"}),
            None,
        );
        let mut app = mock.app();
        drive(&mut app, |app| app.network.has_data(Section::Session)).await;
        app.start_command(TuiCommand::SaveAccountCredentials {
            provider: "deepseek".to_string(),
            body: json!({
                "api_key": "sk-test",
                "label": "work",
                "base_url": "https://api.deepseek.com"
            }),
        });
        drive(&mut app, |app| !app.network.command_pending()).await;

        let requests = mock.state.requests.lock().unwrap();
        let (_, path, body, _) = requests
            .iter()
            .find(|(_, path, _, _)| path == "/login/deepseek/start")
            .unwrap();
        assert_eq!(path, "/login/deepseek/start");
        let body: Value = serde_json::from_slice(body).unwrap();
        assert_eq!(body["api_key"], "sk-test");
        assert_eq!(body["label"], "work");
        assert_eq!(body["base_url"], "https://api.deepseek.com");
        assert_eq!(app.message, "saved DeepSeek account");
    }

    #[tokio::test]
    async fn qwen_cloud_connect_saves_before_login_and_disconnect_keeps_the_api_account() {
        let mock = MockGateway::new().await;
        mock.reply(
            "/compatible/save",
            StatusCode::OK,
            json!({"ok":true,"file_name":"plan.json"}),
            None,
        );
        mock.reply("/compatible/qwen/connect", StatusCode::OK,
            json!({"ok":true,"login_id":"login-1","verification_url":"https://account.qwencloud.com/device?code=synthetic","interval":5}), None);
        mock.reply(
            "/compatible/qwen/disconnect",
            StatusCode::OK,
            json!({"ok":true,"message":"Qwen Cloud disconnected. API access is still available."}),
            None,
        );
        mock.reply(
            "/compatible/quota.json",
            StatusCode::OK,
            json!({"accounts":[]}),
            None,
        );
        let mut app = mock.app();
        drive(&mut app, |app| app.network.has_data(Section::Session)).await;
        app.start_command(TuiCommand::SaveAccountCredentials { provider:"compatible".into(),
            body:json!({"service":"qwen_token_plan","api_key":"synthetic-key","quota_action":"connect"}) });
        drive(&mut app, |app| app.pending_account_login.is_some()).await;
        match app.pending_account_login.take().unwrap() {
            AccountLoginPrompt::QwenCloud {
                login_id,
                verification_url,
                interval,
            } => {
                assert_eq!(login_id, "login-1");
                assert!(verification_url.starts_with("https://account.qwencloud.com/"));
                assert_eq!(interval, 5);
            }
            _ => panic!("expected Qwen Cloud sign-in"),
        }
        assert_eq!(mock.count("/compatible/save"), 1);
        app.start_command(TuiCommand::SaveAccountCredentials {
            provider: "compatible".into(),
            body: json!({"file_name":"plan.json","quota_action":"disconnect"}),
        });
        drive(&mut app, |app| !app.network.command_pending()).await;
        assert_eq!(mock.count("/compatible/qwen/disconnect"), 1);
        assert_eq!(mock.count("/credentials/delete"), 0);
        let requests = mock.state.requests.lock().unwrap();
        let (_, _, body, _) = requests
            .iter()
            .find(|(_, path, _, _)| path == "/compatible/qwen/connect")
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(body).unwrap(),
            json!({"file_name":"plan.json"})
        );
    }

    #[tokio::test]
    async fn browser_account_add_keeps_provider_state_for_callback_submission() {
        let mock = MockGateway::new().await;
        mock.reply(
            "/login/claude/start?label=work&organization_uuid=org-1&base_url=https%3A%2F%2Fapi.anthropic.com",
            StatusCode::OK,
            json!({"ok":true,"url":"https://example.test/login","state":"state-1"}),
            None,
        );
        let mut app = mock.app();
        drive(&mut app, |app| app.network.has_data(Section::Session)).await;
        app.start_command(TuiCommand::StartAccountLogin {
            provider: "claude".to_string(),
            label: Some("work".to_string()),
            organization_uuid: Some("org-1".to_string()),
            base_url: Some("https://api.anthropic.com".to_string()),
            account_type: None,
        });
        drive(&mut app, |app| app.pending_account_login.is_some()).await;

        match app.pending_account_login.as_ref().unwrap() {
            AccountLoginPrompt::Browser {
                provider,
                url,
                state,
                label,
                organization_uuid,
                base_url,
            } => {
                assert_eq!(provider, "claude");
                assert_eq!(url, "https://example.test/login");
                assert_eq!(state.as_deref(), Some("state-1"));
                assert_eq!(label.as_deref(), Some("work"));
                assert_eq!(organization_uuid.as_deref(), Some("org-1"));
                assert_eq!(base_url.as_deref(), Some("https://api.anthropic.com"));
            }
            AccountLoginPrompt::CopilotDevice { .. } | AccountLoginPrompt::QwenCloud { .. } => {
                panic!("expected browser login")
            }
        }
    }

    #[tokio::test]
    async fn notification_management_uses_dashboard_settings_route() {
        let mock = MockGateway::new().await;
        mock.reply(
            "/notifications/settings",
            StatusCode::OK,
            json!({
                "ok": true,
                "settings": {
                    "enabled": false,
                    "channel": "telegram",
                    "telegram": {"bot_token_configured": true, "chat_id": "123"},
                    "google_chat": {"webhook_configured": false},
                    "watched_accounts": ["codex:one"],
                    "accounts": [{"provider":"codex", "provider_label":"Codex", "key":"codex:one", "label":"One", "enabled":true}]
                }
            }),
            None,
        );
        let mut app = mock.app();
        drive(&mut app, |app| app.network.has_data(Section::Session)).await;
        app.tab = Tab::Notifications;
        app.data.notifications = json!({
            "ok": true,
            "settings": {
                "enabled": true,
                "channel": "telegram",
                "watched_accounts": [],
                "accounts": [{"provider":"codex", "provider_label":"Codex", "key":"codex:one", "label":"One", "enabled":true}]
            }
        });
        app.toggle_notifications_enabled();
        drive(&mut app, |app| !app.network.command_pending()).await;

        let requests = mock.state.requests.lock().unwrap();
        let (_, _, body, _) = requests
            .iter()
            .find(|(_, path, _, _)| path == "/notifications/settings")
            .unwrap();
        let body: Value = serde_json::from_slice(body).unwrap();
        assert_eq!(body["enabled"], false);
        assert_eq!(body["channel"], "telegram");
        assert_eq!(body["watched_accounts"], json!([]));
    }

    #[tokio::test]
    async fn model_actions_keep_routes_and_confirmation_alias_across_updates() {
        let mock = MockGateway::new().await;
        mock.reply(
            "/custom-models/save",
            StatusCode::OK,
            json!({"message":"saved"}),
            None,
        );
        let mut app = mock.app();
        drive(&mut app, |app| app.network.has_data(Section::Session)).await;
        app.overview_section = OverviewSection::CustomModels;
        let model = json!({"alias":"chosen","enabled":true,"routes":[{"provider":"codex","model":"example"}]});
        app.apply_section(Section::Models, json!({"models":[model.clone()]}));
        app.request_delete_confirmation();
        app.apply_section(
            Section::Models,
            json!({"models":[{"alias":"another"},model.clone()]}),
        );
        assert_eq!(app.selected_model().unwrap().alias, "chosen");
        assert!(
            matches!(app.confirmation_modal.as_ref().unwrap().action, ConfirmationAction::DeleteCustomModel { ref alias } if alias == "chosen")
        );
        app.cancel_confirmation();
        app.toggle_selected_model(false);
        drive(&mut app, |_| mock.count("/custom-models/save") == 1).await;
        let requests = mock.state.requests.lock().unwrap();
        let (_, _, body, _) = requests
            .iter()
            .find(|(_, path, _, _)| path == "/custom-models/save")
            .unwrap();
        let body: Value = serde_json::from_slice(body).unwrap();
        assert_eq!(body["routes"], model["routes"]);
        assert_eq!(body["enabled"], false);
        assert_eq!(body["alias"], "chosen");
    }

    #[tokio::test]
    async fn malformed_success_does_not_replace_valid_section_data() {
        let mock = MockGateway::new().await;
        let mut app = mock.app();
        drive(&mut app, |app| app.network.has_data(Section::Summary)).await;
        mock.reply(
            Section::Summary.path(),
            StatusCode::OK,
            json!({"unexpected":"response"}),
            None,
        );
        app.refresh();
        drive(&mut app, |app| {
            app.network.sections[Section::Summary as usize]
                .error
                .is_some()
        })
        .await;
        assert_eq!(app.data.summary["totals"]["requests"], 111);
    }
}
