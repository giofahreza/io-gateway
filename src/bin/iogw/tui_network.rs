//! Bounded background requests. The terminal thread only schedules work and
//! applies completed results; it never waits for the gateway.

use super::{bool_at, ensure_success, message_from, GatewayClient, Tab};
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
            Self::Summary | Self::Routing => matches!(tab, Tab::Overview | Tab::Accounts),
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
}

pub(super) enum NetworkEvent {
    Section {
        section: Section,
        generation: u64,
        result: Result<Value, RequestError>,
    },
    Command(Result<CommandOutcome, RequestError>),
}

pub(super) enum TuiCommand {
    Login {
        otp: String,
        api_key: Option<String>,
    },
    ToggleAccount {
        file_name: String,
        enabled: bool,
    },
    SetPriority {
        provider: String,
        account: String,
        priority: bool,
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
    TestNotification,
}

impl TuiCommand {
    async fn execute(self, mut client: GatewayClient) -> Result<CommandOutcome, RequestError> {
        let login = matches!(self, Self::Login { .. });
        let refresh = !matches!(self, Self::TestNotification);
        let response = match self {
            Self::Login { otp, api_key } => client.login(&otp, api_key.as_deref()).await,
            Self::ToggleAccount { file_name, enabled } => {
                client
                    .post_form(
                        "/credentials/toggle",
                        &[("file_name", file_name), ("enabled", enabled.to_string())],
                    )
                    .await
            }
            Self::SetPriority {
                provider,
                account,
                priority,
            } => client
                .post_json(
                    "/admin/account-routing/priority",
                    Some(json!({ "provider": provider, "account": account, "priority": priority })),
                )
                .await,
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
            Self::TestNotification => client.post_json("/notifications/test", None).await,
        }?;
        ensure_success(&response).map_err(|message| RequestError {
            message,
            unauthorized: matches!(response.status.as_u16(), 401 | 403),
        })?;
        Ok(CommandOutcome {
            message: if login {
                "logged in".to_string()
            } else {
                message_from(&response.body)
            },
            client: login.then_some(client),
            refresh,
        })
    }
}

#[derive(Default)]
struct FetchState {
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
}

impl TuiNetwork {
    pub(super) fn new() -> Self {
        // At most eight section results and one action can be outstanding.
        let (tx, rx) = mpsc::channel(16);
        Self {
            sections: std::array::from_fn(|_| FetchState::default()),
            tx,
            rx,
            command: None,
            logging_in: false,
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
            if let Some(task) = state.task.take() {
                task.abort();
                state.generation += 1;
                state.attempted_at = None;
            }
        }
        let now = Instant::now();
        let mut active = self
            .sections
            .iter()
            .filter(|state| state.task.is_some())
            .count();
        for section in Section::ALL {
            if active >= MAX_FETCHES {
                break;
            }
            if !section.visible(tab) || (section != Section::Session && !authenticated) {
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

    pub(super) fn invalidate(&mut self, include_session: bool) {
        for section in Section::ALL {
            if section == Section::Session && !include_session {
                continue;
            }
            let state = &mut self.sections[section as usize];
            if let Some(task) = state.task.take() {
                task.abort();
            }
            state.generation += 1;
            state.attempted_at = None;
            state.retry_at = None;
        }
    }

    pub(super) fn forget_data(&mut self) {
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
        if self.command.is_some() {
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
    }
}

#[cfg(test)]
mod tests {
    use super::super::{ConfirmationAction, OverviewSection, TuiApp};
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
        app.tab = Tab::Accounts;
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
