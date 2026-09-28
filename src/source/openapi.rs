#![allow(dead_code)]

use axum::{
    extract::Path,
    http::StatusCode,
    response::{IntoResponse, Redirect, Response},
    Json,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use utoipa::openapi::security::{HttpAuthScheme, HttpBuilder, SecurityScheme};
use utoipa::{Modify, OpenApi, ToSchema};
use utoipa_swagger_ui::Config;

#[derive(OpenApi)]
#[openapi(
    paths(
        crate::health,
        crate::dashboard_root,
        crate::dashboard,
        crate::dashboard_json,
        crate::quota_json_route,
        crate::login_start_route,
        crate::login_submit_route,
        crate::delete_credential_route,
        crate::toggle_credential_route,
        codex_reset_credit_automation_get_doc,
        codex_reset_credit_automation_post_doc,
        codex_reset_credit_consume_doc,
        swagger_ui_redirect,
        openapi_json,
        crate::source::v1::models_doc,
        crate::source::v1::model_doc,
        crate::source::v1::responses_create_doc,
        crate::source::v1::responses_get_doc,
        crate::source::v1::responses_delete_doc,
        crate::source::codex::models_doc,
        crate::source::codex::responses_doc,
        crate::source::claude::models_doc,
        crate::source::claude::models_v1_doc,
        crate::source::claude::messages_doc,
        crate::source::claude::responses_doc
    ),
    components(schemas(
        DashboardJsonResponse,
        DashboardAccount,
        QuotaResponse,
        QuotaAccount,
        QuotaRateSummary,
        QuotaWindowSummary,
        LoginStartResponse,
        ActionResponse,
        LoginSubmitRequest,
        DeleteCredentialRequest,
        ToggleCredentialRequest,
        OpenAiModelListResponse,
        OpenAiModel,
        UpstreamModelListResponse,
        UpstreamModel,
        V1ResponsesCreateRequest,
        CodexResponsesCreateRequest,
        CodexInputMessage,
        CodexInputContentPart,
        ToolDescriptor,
        ClaudeMessagesCreateRequest,
        ClaudeMessage,
        ClaudeContentPart,
        ResponseSummary,
        OpenAiErrorResponse,
        OpenAiErrorBody,
        CodexResetCreditAutomationPolicyRequest,
        CodexResetCreditAutomationResponse,
        CodexResetCreditAutomationDefaults,
        CodexResetCreditAutomationAccount,
        CodexResetCreditAutomationPolicy,
        CodexResetCreditAutomationAction,
        CodexResetCreditConsumeRequest
    )),
    modifiers(&SecurityAddon)
)]
pub struct ApiDoc;

struct SecurityAddon;

impl Modify for SecurityAddon {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        openapi
            .components
            .get_or_insert_with(Default::default)
            .add_security_scheme(
                "bearer_auth",
                SecurityScheme::Http(
                    HttpBuilder::new()
                        .scheme(HttpAuthScheme::Bearer)
                        .bearer_format("API key")
                        .build(),
                ),
            );
    }
}

/// Serves the generated OpenAPI document consumed by Swagger UI.
#[utoipa::path(
    get,
    path = "/api-docs/openapi.json",
    responses((status = 200, description = "OpenAPI JSON", body = String))
)]
pub async fn openapi_json() -> impl IntoResponse {
    Json(ApiDoc::openapi())
}

/// Redirects the Swagger entrypoint to the UI root.
#[utoipa::path(
    get,
    path = "/docs",
    responses((status = 303, description = "Redirects to /docs/"))
)]
pub async fn swagger_ui_redirect() -> impl IntoResponse {
    Redirect::to("/docs/")
}

pub async fn swagger_ui_root() -> impl IntoResponse {
    swagger_ui_file("")
}

pub async fn swagger_ui_asset(Path(rest): Path<String>) -> impl IntoResponse {
    swagger_ui_file(&rest)
}

fn swagger_ui_file(path: &str) -> Response {
    let config = Arc::new(Config::new(["/api-docs/openapi.json"]).try_it_out_enabled(true));
    match utoipa_swagger_ui::serve(path, config) {
        Ok(Some(file)) => (
            StatusCode::OK,
            [("Content-Type", file.content_type)],
            file.bytes.into_owned(),
        )
            .into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(error) => (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()).into_response(),
    }
}

#[derive(Serialize, ToSchema)]
pub(crate) struct DashboardJsonResponse {
    total_requests: u64,
    total_errors: u64,
    accounts: Vec<DashboardAccount>,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct DashboardAccount {
    label: String,
    account_id: String,
    requests: u64,
    errors: u64,
    file_name: Option<String>,
    enabled: bool,
    expired_at: Option<String>,
    last_success_at: Option<String>,
    last_error_at: Option<String>,
    last_error_message: Option<String>,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct QuotaResponse {
    accounts: Vec<QuotaAccount>,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct QuotaAccount {
    label: String,
    account_id: String,
    file_name: String,
    plan_type: Option<String>,
    code_generation: Option<QuotaRateSummary>,
    code_review: Option<QuotaRateSummary>,
    error: Option<String>,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct QuotaRateSummary {
    five_hour: Option<QuotaWindowSummary>,
    weekly: Option<QuotaWindowSummary>,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct QuotaWindowSummary {
    used_percent: Option<f64>,
    reset_label: String,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct LoginStartResponse {
    url: String,
    state: String,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct ActionResponse {
    ok: bool,
    message: String,
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct LoginSubmitRequest {
    redirect_url: String,
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct DeleteCredentialRequest {
    file_name: String,
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct ToggleCredentialRequest {
    file_name: String,
    enabled: String,
}

/// Reads the administrator-visible, privacy-safe status of automatic Codex
/// reset-credit redemption. The runtime route requires an administrator
/// session when dashboard authentication is configured.
#[utoipa::path(
    get,
    path = "/admin/codex/reset-credit-automation",
    responses(
        (status = 200, description = "Automation policies and safe action status", body = CodexResetCreditAutomationResponse),
        (status = 401, description = "Administrator login required", body = ActionResponse),
        (status = 503, description = "Durable policy state is unavailable", body = ActionResponse)
    )
)]
#[allow(dead_code)]
pub(crate) fn codex_reset_credit_automation_get_doc() {}

/// Creates, edits, or disables one stable Codex-account automation policy.
#[utoipa::path(
    post,
    path = "/admin/codex/reset-credit-automation",
    request_body(content = CodexResetCreditAutomationPolicyRequest, content_type = "application/json"),
    responses(
        (status = 200, description = "Saved automation policies and safe action status", body = CodexResetCreditAutomationResponse),
        (status = 400, description = "Invalid policy or unavailable enabled account", body = ActionResponse),
        (status = 401, description = "Administrator login required", body = ActionResponse),
        (status = 503, description = "Durable policy state is unavailable", body = ActionResponse)
    )
)]
#[allow(dead_code)]
pub(crate) fn codex_reset_credit_automation_post_doc() {}

/// Manually redeems one concrete Codex reset credit through the same durable
/// action ledger as the automatic worker. The server owns the idempotency key.
#[utoipa::path(
    post,
    path = "/codex/rate-limit-reset-credit/consume",
    request_body(content = CodexResetCreditConsumeRequest, content_type = "application/x-www-form-urlencoded"),
    responses(
        (status = 200, description = "Coordinated redemption result", body = ActionResponse),
        (status = 401, description = "Administrator login required", body = ActionResponse)
    )
)]
#[allow(dead_code)]
pub(crate) fn codex_reset_credit_consume_doc() {}

#[derive(Deserialize, ToSchema)]
pub(crate) struct CodexResetCreditAutomationPolicyRequest {
    /// Must be a stable `codex:account_id:<id>` identity.
    account_key: String,
    enabled: bool,
    /// 1–1440; default 30.
    scan_interval_minutes: Option<u64>,
    /// 1–10080; default 60.
    expiry_window_minutes: Option<u64>,
    /// Must be smaller than `expiry_window_minutes`; default 5.
    final_attempt_minutes: Option<u64>,
    /// 0–10080; default 10.
    min_natural_reset_remaining_minutes: Option<u64>,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct CodexResetCreditAutomationResponse {
    ok: bool,
    defaults: CodexResetCreditAutomationDefaults,
    accounts: Vec<CodexResetCreditAutomationAccount>,
    policies: Vec<CodexResetCreditAutomationPolicy>,
    actions: Vec<CodexResetCreditAutomationAction>,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct CodexResetCreditAutomationDefaults {
    scan_interval_minutes: u64,
    expiry_window_minutes: u64,
    final_attempt_minutes: u64,
    min_natural_reset_remaining_minutes: u64,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct CodexResetCreditAutomationAccount {
    account_key: String,
    label: String,
    account_id: String,
    file_name: Option<String>,
    enabled: bool,
    eligible: bool,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct CodexResetCreditAutomationPolicy {
    account_key: String,
    enabled: bool,
    scan_interval_minutes: u64,
    expiry_window_minutes: u64,
    final_attempt_minutes: u64,
    min_natural_reset_remaining_minutes: u64,
    next_scan_at: Option<String>,
    updated_at: String,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct CodexResetCreditAutomationAction {
    id: String,
    account_key: String,
    credit_id: String,
    reset_type: String,
    credit_expires_at: Option<String>,
    state: String,
    trigger: String,
    next_attempt_at: String,
    attempt_count: u64,
    last_outcome: Option<String>,
    created_at: String,
    updated_at: String,
    submitted_at: Option<String>,
    verified_at: Option<String>,
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct CodexResetCreditConsumeRequest {
    file_name: Option<String>,
    label: Option<String>,
    account_id: Option<String>,
    /// Required: an opaque credit ID returned by the quota read.
    credit_id: String,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct OpenAiModelListResponse {
    object: String,
    data: Vec<OpenAiModel>,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct OpenAiModel {
    id: String,
    object: String,
    created: i64,
    owned_by: String,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct UpstreamModelListResponse {
    models: Vec<UpstreamModel>,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct UpstreamModel {
    id: Option<String>,
    slug: Option<String>,
    name: Option<String>,
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct V1ResponsesCreateRequest {
    model: String,
    input: String,
    instructions: Option<String>,
    stream: Option<bool>,
    tools: Option<Vec<ToolDescriptor>>,
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct CodexResponsesCreateRequest {
    model: String,
    input: Vec<CodexInputMessage>,
    instructions: Option<String>,
    stream: Option<bool>,
    tools: Option<Vec<ToolDescriptor>>,
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct CodexInputMessage {
    role: String,
    content: Vec<CodexInputContentPart>,
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct CodexInputContentPart {
    #[serde(rename = "type")]
    kind: String,
    text: String,
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct ToolDescriptor {
    #[serde(rename = "type")]
    kind: String,
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct ClaudeMessagesCreateRequest {
    model: String,
    messages: Vec<ClaudeMessage>,
    system: Option<String>,
    stream: Option<bool>,
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct ClaudeMessage {
    role: String,
    content: Vec<ClaudeContentPart>,
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct ClaudeContentPart {
    #[serde(rename = "type")]
    kind: String,
    text: String,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct ResponseSummary {
    id: Option<String>,
    object: String,
    model: Option<String>,
    status: Option<String>,
    output_text: Option<String>,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct OpenAiErrorResponse {
    error: OpenAiErrorBody,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct OpenAiErrorBody {
    message: String,
    #[serde(rename = "type")]
    kind: String,
    code: Option<String>,
}
