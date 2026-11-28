use crate::source::v1::multimodal::{classify_content, is_data_url, split_data_url, PartKind};
use axum::{
    body::Body,
    extract::State,
    http::{HeaderMap, HeaderValue, StatusCode},
    response::IntoResponse,
};
use bytes::Bytes;
use futures_util::StreamExt;
use serde_json::{json, Map, Value};
use std::{collections::BTreeMap, error::Error, future::Future, time::Duration};
use uuid::Uuid;

const REQUEST_TIMEOUT_SECS: u64 = 180;
const STREAM_REQUEST_TIMEOUT_SECS: u64 = 30 * 60;
const RESPONSES_HEARTBEAT_SECS: u64 = 10;
const DEFAULT_MAX_TOKENS: u64 = 4096;
const MODERN_DEFAULT_MAX_TOKENS: u64 = 32_000;
const CLAUDE_CODE_ATTRIBUTION_MARKER: &str =
    "x-anthropic-billing-header: cc_version=2.1.207.3a5; cc_entrypoint=sdk-cli; cch=00000;";
const CLAUDE_CODE_AGENT_PROMPT: &str =
    "You are a Claude agent, built on Anthropic's Claude Agent SDK.";

// candidate_accounts reserves its first account before any async policy or
// credential work. Cancellation during that work must release the reservation.
struct ClaudeReservationGuard {
    state: crate::AppState,
    account_key: Option<String>,
}

impl ClaudeReservationGuard {
    fn new(state: &crate::AppState, accounts: &[super::accounts::ClaudeAccount]) -> Self {
        Self {
            state: state.clone(),
            account_key: accounts.first().map(crate::claude_stats_key),
        }
    }

    fn started(&mut self) {
        self.account_key = None;
    }
}

impl Drop for ClaudeReservationGuard {
    fn drop(&mut self) {
        if let Some(key) = &self.account_key {
            crate::router_reservation_cancelled(&self.state, "claude", key);
        }
    }
}

pub async fn models(State(state): State<crate::AppState>, headers: HeaderMap) -> impl IntoResponse {
    if !crate::check_api_key(&state, &headers) {
        return openai_error(
            StatusCode::UNAUTHORIZED,
            "invalid_request_error",
            "Invalid proxy API key",
        );
    }

    let account = match super::accounts::first_enabled(&state) {
        Some(account) => account,
        None => {
            return (
                StatusCode::OK,
                [("Content-Type", "application/json")],
                serde_json::to_vec(&json!({
                    "object": "list",
                    "data": fallback_models()
                }))
                .unwrap_or_default(),
            )
                .into_response();
        }
    };
    let access_token = match super::auth::ensure_access_token(&state, &account).await {
        Ok(token) => token,
        Err(err) => {
            return (
                StatusCode::OK,
                [("Content-Type", "application/json")],
                serde_json::to_vec(&json!({
                    "object": "list",
                    "warning": err,
                    "data": models_to_openai_entries(&account.models)
                }))
                .unwrap_or_default(),
            )
                .into_response();
        }
    };
    let base_url = super::auth::api_base_url(account.api_base_url.as_deref());
    let models = super::auth::fetch_models(&state.client, &access_token, &base_url)
        .await
        .unwrap_or_else(|_| {
            if account.models.is_empty() {
                fallback_model_infos()
            } else {
                account.models.clone()
            }
        });
    (
        StatusCode::OK,
        [("Content-Type", "application/json")],
        serde_json::to_vec(&json!({
            "object": "list",
            "data": models_to_openai_entries(&models)
        }))
        .unwrap_or_default(),
    )
        .into_response()
}

pub async fn messages(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    if !crate::check_api_key(&state, &headers) {
        crate::release_api_key_budget_before_dispatch(&state);
        return anthropic_error(
            StatusCode::UNAUTHORIZED,
            "authentication_error",
            "Invalid proxy API key",
        );
    }
    let raw = match serde_json::from_slice::<Value>(&body) {
        Ok(value) => value,
        Err(_) => {
            crate::release_api_key_budget_before_dispatch(&state);
            return anthropic_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "Invalid request body",
            );
        }
    };
    let model = match raw.get("model").and_then(|value| value.as_str()) {
        Some(model) if !model.trim().is_empty() => model.trim().to_string(),
        _ => {
            crate::release_api_key_budget_before_dispatch(&state);
            return anthropic_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "model is required",
            );
        }
    };
    let accounts = super::accounts::candidate_accounts(&state);
    let mut reservation = ClaudeReservationGuard::new(&state, &accounts);
    if accounts.is_empty() {
        crate::release_api_key_budget_before_dispatch(&state);
        let reason = super::accounts::empty_accounts_reason(&state);
        return anthropic_error(StatusCode::SERVICE_UNAVAILABLE, "api_error", reason);
    }
    let wants_stream = crate::source::wants_stream(&headers, &body);
    // Policy and usage accounting must include the Claude Code system blocks
    // that are injected immediately before the upstream POST.
    let prompt_metrics = policy_prompt_metrics("anthropic/v1/messages", &body)
        .unwrap_or_else(|| crate::prompt_metrics_from_request_value(&raw));
    let prepared_policy_body = prepare_anthropic_body(body.clone()).0;
    let mut last_error: Option<(StatusCode, String)> = None;

    for (attempt_idx, account) in accounts.iter().enumerate() {
        let context = crate::claude_usage_context(
            account,
            Some(model.clone()),
            "/claude/v1/messages",
            prompt_metrics.clone(),
        );
        if let Err(response) = crate::reserve_api_key_budgets_for_prepared_dispatch(
            &state,
            context.provider_name,
            &context.key,
            &prepared_policy_body,
        )
        .await
        {
            return response;
        }
        crate::record_claude_request(&state, &context);
        reservation.started();
        let mut lifecycle = crate::StreamRequestGuard::new(&state, &context);
        let access_token = match super::auth::ensure_access_token(&state, account).await {
            Ok(token) => token,
            Err(err) => {
                lifecycle.finish();
                if attempt_idx + 1 < accounts.len() {
                    crate::record_request_pre_dispatch_retry_error(&state, &context, err.as_str());
                } else {
                    crate::record_request_pre_dispatch_error(&state, &context, err.as_str());
                }
                last_error = Some((StatusCode::UNAUTHORIZED, err));
                if attempt_idx + 1 < accounts.len() {
                    continue;
                }
                break;
            }
        };

        let resp = match post_anthropic_messages(
            &state.client,
            account,
            &access_token,
            &headers,
            body.clone(),
            wants_stream,
        )
        .await
        {
            Ok(resp) => resp,
            Err(err) => {
                lifecycle.finish();
                crate::record_claude_error(&state, &context, &err);
                last_error = Some((StatusCode::BAD_GATEWAY, err));
                if attempt_idx + 1 < accounts.len() {
                    continue;
                }
                break;
            }
        };

        let status = resp.status();
        super::quota::observe_response_headers(&state, account, resp.headers());
        let out_headers = response_headers(
            resp.headers(),
            if wants_stream {
                "text/event-stream"
            } else {
                "application/json"
            },
        );
        if wants_stream && status.is_success() {
            lifecycle.finish();
            return stream_anthropic_passthrough(state, context, resp, out_headers).await;
        }
        let bytes = match resp.bytes().await {
            Ok(bytes) => bytes,
            Err(err) => {
                lifecycle.finish();
                let message = claude_transport_error("body read", err);
                crate::record_claude_error(&state, &context, &message);
                last_error = Some((StatusCode::BAD_GATEWAY, message));
                if attempt_idx + 1 < accounts.len() {
                    continue;
                }
                break;
            }
        };
        if !status.is_success() {
            lifecycle.finish();
            let message = format!(
                "Claude returned {}: {}",
                status,
                String::from_utf8_lossy(&bytes)
            );
            crate::record_claude_error(&state, &context, &message);
            if attempt_idx + 1 < accounts.len()
                && crate::should_retry_account_error(status, &message)
            {
                last_error = Some((status, message));
                continue;
            }
            return (status, out_headers, bytes).into_response();
        }
        let usage = serde_json::from_slice::<Value>(&bytes)
            .map(|value| crate::usage_metrics_from_response_value(&value))
            .unwrap_or_default();
        crate::record_claude_success(&state, &context, &usage);
        lifecycle.finish();
        return (status, out_headers, bytes).into_response();
    }

    let (status, message) = last_error.unwrap_or_else(|| {
        (
            StatusCode::BAD_GATEWAY,
            "All Claude accounts failed".to_string(),
        )
    });
    anthropic_error(
        status,
        "api_error",
        &format!("All Claude accounts failed; last error: {}", message),
    )
}

pub async fn responses(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    if !crate::check_api_key(&state, &headers) {
        crate::release_api_key_budget_before_dispatch(&state);
        return openai_error(
            StatusCode::UNAUTHORIZED,
            "invalid_request_error",
            "Invalid proxy API key",
        );
    }
    let raw = match serde_json::from_slice::<Value>(&body) {
        Ok(value) => value,
        Err(_) => {
            crate::release_api_key_budget_before_dispatch(&state);
            return openai_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "Invalid request body",
            );
        }
    };
    let model = match raw.get("model").and_then(|value| value.as_str()) {
        Some(model) if !model.trim().is_empty() => model.trim().to_string(),
        _ => {
            crate::release_api_key_budget_before_dispatch(&state);
            return openai_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "model is required",
            );
        }
    };
    let wants_stream = crate::source::wants_stream(&headers, &body);
    let anthropic_payload = match responses_to_anthropic_messages(&raw, &model, true) {
        Ok(payload) => payload,
        Err(err) => {
            crate::release_api_key_budget_before_dispatch(&state);
            return openai_error(StatusCode::BAD_REQUEST, "invalid_request_error", &err);
        }
    };
    let anthropic_body = Bytes::from(serde_json::to_vec(&anthropic_payload).unwrap_or_default());
    let accounts = super::accounts::candidate_accounts(&state);
    let mut reservation = ClaudeReservationGuard::new(&state, &accounts);
    if accounts.is_empty() {
        crate::release_api_key_budget_before_dispatch(&state);
        let reason = super::accounts::empty_accounts_reason(&state);
        return openai_error(StatusCode::SERVICE_UNAVAILABLE, "server_error", reason);
    }
    // Keep per-account telemetry consistent with the proxy's final-payload
    // policy preflight, including translated and injected system content.
    let prompt_metrics = policy_prompt_metrics("responses", &body)
        .unwrap_or_else(|| crate::prompt_metrics_from_request_value(&raw));
    let prepared_policy_body = prepare_anthropic_body(anthropic_body.clone()).0;
    let mut last_error: Option<(StatusCode, String)> = None;

    for (attempt_idx, account) in accounts.iter().enumerate() {
        let context = crate::claude_usage_context(
            account,
            Some(model.clone()),
            "/claude/v1/responses",
            prompt_metrics.clone(),
        );
        if let Err(response) = crate::reserve_api_key_budgets_for_prepared_dispatch(
            &state,
            context.provider_name,
            &context.key,
            &prepared_policy_body,
        )
        .await
        {
            return response;
        }
        crate::record_claude_request(&state, &context);
        reservation.started();
        let mut lifecycle = crate::StreamRequestGuard::new(&state, &context);
        let access_token = match super::auth::ensure_access_token(&state, account).await {
            Ok(token) => token,
            Err(err) => {
                lifecycle.finish();
                if attempt_idx + 1 < accounts.len() {
                    crate::record_request_pre_dispatch_retry_error(&state, &context, err.as_str());
                } else {
                    crate::record_request_pre_dispatch_error(&state, &context, err.as_str());
                }
                last_error = Some((StatusCode::UNAUTHORIZED, err));
                if attempt_idx + 1 < accounts.len() {
                    continue;
                }
                break;
            }
        };
        let resp = match post_anthropic_messages(
            &state.client,
            account,
            &access_token,
            &headers,
            anthropic_body.clone(),
            true,
        )
        .await
        {
            Ok(resp) => resp,
            Err(err) => {
                lifecycle.finish();
                crate::record_claude_error(&state, &context, &err);
                last_error = Some((StatusCode::BAD_GATEWAY, err));
                if attempt_idx + 1 < accounts.len() {
                    continue;
                }
                break;
            }
        };
        let status = resp.status();
        super::quota::observe_response_headers(&state, account, resp.headers());
        if !status.is_success() {
            let text = match resp.text().await {
                Ok(text) => text,
                Err(err) => {
                    let message = claude_transport_error("body read", err);
                    crate::record_claude_error(&state, &context, &message);
                    lifecycle.finish();
                    last_error = Some((StatusCode::BAD_GATEWAY, message));
                    if attempt_idx + 1 < accounts.len() {
                        continue;
                    }
                    break;
                }
            };
            let message = format!("Claude returned {}: {}", status, text);
            crate::record_claude_error(&state, &context, &message);
            lifecycle.finish();
            if attempt_idx + 1 < accounts.len()
                && crate::should_retry_account_error(status, &message)
            {
                last_error = Some((status, message));
                continue;
            }
            return (
                status,
                [("Content-Type", "application/json")],
                openai_error_body_from_anthropic(&text),
            )
                .into_response();
        }
        let completion = finish_claude_responses(
            state.clone(),
            account.clone(),
            headers.clone(),
            context,
            lifecycle,
            resp,
            anthropic_payload.clone(),
            recovery_max_tokens(&raw, &model),
            model.clone(),
        );
        if wants_stream {
            return buffered_responses_stream(completion);
        }
        return match completion.await {
            Ok(response) => (
                StatusCode::OK,
                [("Content-Type", "application/json")],
                serde_json::to_vec(&response).unwrap_or_default(),
            )
                .into_response(),
            Err(response) => response,
        };
    }

    let (status, message) = last_error.unwrap_or_else(|| {
        (
            StatusCode::BAD_GATEWAY,
            "All Claude accounts failed".to_string(),
        )
    });
    openai_error(
        status,
        "server_error",
        &format!("All Claude accounts failed; last error: {}", message),
    )
}

fn claude_transport_error(stage: &str, err: reqwest::Error) -> String {
    // Display alone hides reqwest's timeout/connection cause. Keep the cause
    // chain actionable for diagnostics and the router's short timeout cooldown.
    // Strip the URL, which can contain credentials on custom upstreams.
    let category = if err.is_timeout() {
        "timeout"
    } else {
        "transport error"
    };
    let err = err.without_url();
    let mut message = format!("Claude {stage} failed ({category}): {err}");
    let mut source = err.source();
    while let Some(cause) = source {
        message.push_str(&format!(": {cause}"));
        source = cause.source();
    }
    message
}

fn recovery_max_tokens(raw: &Value, model: &str) -> Option<u64> {
    if raw.get("max_tokens").and_then(Value::as_u64).is_some()
        || raw
            .get("max_output_tokens")
            .and_then(Value::as_u64)
            .is_some()
    {
        return None;
    }
    // One regeneration from 32K to 64K, only for model families with a known
    // >=64K output limit. Opus 4/4.1 top out at 32K; unknown models stay unchanged.
    let (family, version) = model.strip_prefix("claude-")?.split_once('-')?;
    let mut components = version.split('-');
    let major = components.next()?.parse::<u32>().ok()?;
    let minor = components
        .next()
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(0);
    let supported = match family {
        "sonnet" => (4..=5).contains(&major),
        "opus" | "haiku" => major == 5 || (major == 4 && (5..100).contains(&minor)),
        "fable" | "mythos" => major == 5,
        _ => false,
    };
    supported.then_some(64_000)
}

#[allow(clippy::too_many_arguments)]
async fn finish_claude_responses(
    state: crate::AppState,
    account: super::accounts::ClaudeAccount,
    headers: HeaderMap,
    context: crate::UsageContext,
    mut lifecycle: crate::StreamRequestGuard,
    mut resp: reqwest::Response,
    mut payload: Value,
    mut recovery_budget: Option<u64>,
    model: String,
) -> Result<Value, axum::response::Response> {
    loop {
        let (message, usage) = match collect_anthropic_message(resp).await {
            Ok(result) => result,
            Err(message) => {
                crate::record_claude_error(&state, &context, &message);
                lifecycle.finish();
                return Err(openai_error(
                    StatusCode::BAD_GATEWAY,
                    "server_error",
                    &message,
                ));
            }
        };
        crate::record_claude_success(&state, &context, &usage);
        lifecycle.finish();
        let needs_recovery = message["stop_reason"] == "max_tokens";
        let Some(budget) = recovery_budget.take().filter(|_| needs_recovery) else {
            return Ok(anthropic_message_to_response(&message, &model));
        };
        // No item from this attempt has been sent to the client. Regenerate the
        // same input, never append unfinished arguments or replay executed tools.
        // Settle and reserve every inference attempt separately, including when
        // the API key has a renewable quota or a legacy input-token budget.
        payload["max_tokens"] = json!(budget);
        let body = Bytes::from(serde_json::to_vec(&payload).unwrap_or_default());
        let prepared = prepare_anthropic_body(body.clone()).0;
        crate::reserve_api_key_budgets_for_prepared_dispatch(
            &state,
            context.provider_name,
            &context.key,
            &prepared,
        )
        .await?;
        crate::record_claude_request(&state, &context);
        lifecycle = crate::StreamRequestGuard::new(&state, &context);
        let token = match super::auth::ensure_access_token(&state, &account).await {
            Ok(token) => token,
            Err(message) => {
                crate::record_request_pre_dispatch_error(&state, &context, &message);
                lifecycle.finish();
                return Err(openai_error(
                    StatusCode::UNAUTHORIZED,
                    "server_error",
                    &message,
                ));
            }
        };
        resp = match post_anthropic_messages(&state.client, &account, &token, &headers, body, true)
            .await
        {
            Ok(resp) => resp,
            Err(message) => {
                crate::record_claude_error(&state, &context, &message);
                lifecycle.finish();
                return Err(openai_error(
                    StatusCode::BAD_GATEWAY,
                    "server_error",
                    &message,
                ));
            }
        };
        super::quota::observe_response_headers(&state, &account, resp.headers());
        if !resp.status().is_success() {
            let status = resp.status();
            let message = match resp.text().await {
                Ok(text) => format!("Claude returned {status}: {text}"),
                Err(err) => claude_transport_error("body read", err),
            };
            crate::record_claude_error(&state, &context, &message);
            lifecycle.finish();
            return Err(openai_error(status, "server_error", &message));
        }
    }
}

fn buffered_responses_stream(
    completion: impl Future<Output = Result<Value, axum::response::Response>> + Send + 'static,
) -> axum::response::Response {
    let stream = async_stream::stream! {
        tokio::pin!(completion);
        let mut heartbeat = tokio::time::interval(Duration::from_secs(RESPONSES_HEARTBEAT_SECS));
        heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let result = loop {
            tokio::select! {
                result = &mut completion => break result,
                _ = heartbeat.tick() => {
                    // Send an SSE data event: comment-only keepalives do not
                    // reset Codex's eventsource-level idle timer.
                    yield Ok::<Bytes, std::io::Error>(Bytes::from_static(b"event: ping\ndata: {\"type\":\"ping\"}\n\n"));
                }
            }
        };
        let response = match result {
            Ok(response) => response,
            Err(response) => {
                let status = response.status();
                // Codex-source policy errors use plain-text HTTP bodies. Once
                // streaming has begun, retain their actionable status in the
                // Responses error code rather than labelling all failures 5xx.
                let fallback_code = match status {
                    StatusCode::TOO_MANY_REQUESTS => "rate_limit_exceeded",
                    StatusCode::UNAUTHORIZED => "invalid_api_key",
                    StatusCode::FORBIDDEN => "permission_denied",
                    StatusCode::BAD_REQUEST => "invalid_request_error",
                    _ => "server_error",
                };
                let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024).await.unwrap_or_default();
                let error: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
                let message = error.pointer("/error/message").and_then(Value::as_str)
                    .map(str::to_owned).unwrap_or_else(|| String::from_utf8_lossy(&bytes).into_owned());
                json!({
                    "id": format!("resp_{}", Uuid::new_v4().simple()), "object": "response",
                    "status": "failed", "output": [], "usage": null,
                    "error": {"code": error.pointer("/error/code").and_then(Value::as_str).unwrap_or(fallback_code),
                        "message": format!("Claude Responses request failed ({status}): {message}")}
                })
            }
        };
        let mut body = responses_sse(response).into_body().into_data_stream();
        while let Some(chunk) = body.next().await {
            yield chunk.map_err(std::io::Error::other);
        }
    };
    (
        StatusCode::OK,
        [
            ("Content-Type", "text/event-stream"),
            ("Cache-Control", "no-cache"),
            ("X-Accel-Buffering", "no"),
        ],
        Body::from_stream(stream),
    )
        .into_response()
}

#[derive(Default)]
struct AnthropicMessageAccumulator {
    buffer: Vec<u8>,
    message: Option<Value>,
    blocks: BTreeMap<u64, Value>,
    partial_inputs: BTreeMap<u64, String>,
    stopped_blocks: std::collections::BTreeSet<u64>,
    stopped: bool,
}

impl AnthropicMessageAccumulator {
    fn push(&mut self, bytes: &[u8]) -> Result<(), String> {
        self.buffer.extend_from_slice(bytes);
        while let Some((end, delimiter)) = find_sse_boundary(&self.buffer) {
            let event = self.buffer.drain(..end + delimiter).collect::<Vec<_>>();
            self.absorb(&event[..end])?;
        }
        Ok(())
    }

    fn absorb(&mut self, event: &[u8]) -> Result<(), String> {
        if let Some(error) = crate::sse_terminal_error_from_event(event) {
            return Err(format!("Claude stream error: {error}"));
        }
        let Some(data) = parse_sse_data(event) else {
            return Ok(());
        };
        let value: Value = serde_json::from_str(&data)
            .map_err(|err| format!("Claude stream JSON parse failed: {err}"))?;
        match value["type"].as_str() {
            Some("message_start") => {
                if self.message.is_some() || !value["message"].is_object() {
                    return Err("Claude stream has an invalid message_start".into());
                }
                self.message = Some(value["message"].clone());
            }
            Some("content_block_start") => {
                let index = value["index"]
                    .as_u64()
                    .ok_or("Claude stream is missing block index")?;
                if !value["content_block"].is_object()
                    || self
                        .blocks
                        .insert(index, value["content_block"].clone())
                        .is_some()
                {
                    return Err("Claude stream has an invalid content_block_start".into());
                }
            }
            Some("content_block_delta") => {
                let index = value["index"]
                    .as_u64()
                    .ok_or("Claude stream is missing block index")?;
                let block = self
                    .blocks
                    .get_mut(&index)
                    .ok_or("Claude stream delta has no content block")?;
                let delta = &value["delta"];
                let field = match delta["type"].as_str() {
                    Some("text_delta") => "text",
                    Some("thinking_delta") => "thinking",
                    Some("signature_delta") => "signature",
                    Some("input_json_delta") => {
                        self.partial_inputs
                            .entry(index)
                            .or_default()
                            .push_str(delta["partial_json"].as_str().unwrap_or(""));
                        return Ok(());
                    }
                    _ => return Ok(()),
                };
                let mut text = block[field].as_str().unwrap_or("").to_string();
                text.push_str(delta[field].as_str().unwrap_or(""));
                block[field] = json!(text);
            }
            Some("content_block_stop") => {
                let index = value["index"]
                    .as_u64()
                    .ok_or("Claude stream is missing block index")?;
                self.stopped_blocks.insert(index);
            }
            Some("message_delta") => {
                let message = self
                    .message
                    .as_mut()
                    .ok_or("Claude stream delta has no message")?;
                if let Some(delta) = value["delta"].as_object() {
                    message.as_object_mut().unwrap().extend(delta.clone());
                }
                if let Some(usage) = value["usage"].as_object() {
                    if !message["usage"].is_object() {
                        message["usage"] = json!({});
                    }
                    message["usage"]
                        .as_object_mut()
                        .unwrap()
                        .extend(usage.clone());
                }
            }
            Some("message_stop") => self.stopped = true,
            _ => {} // Includes upstream pings and future event types.
        }
        Ok(())
    }

    fn finish(mut self) -> Result<Value, String> {
        if !self.buffer.is_empty() {
            let remaining = std::mem::take(&mut self.buffer);
            self.absorb(&remaining)?;
        }
        if !self.stopped {
            return Err("Claude stream ended before message_stop".into());
        }
        let mut message = self
            .message
            .ok_or("Claude stream ended without message_start")?;
        let truncated = message["stop_reason"] == "max_tokens";
        if message["stop_reason"].as_str().is_none() {
            return Err("Claude stream ended without stop_reason".into());
        }
        let last_index = self.blocks.keys().last().copied();
        for (index, block) in &mut self.blocks {
            let unfinished = truncated && Some(*index) == last_index;
            if !self.stopped_blocks.contains(index) && !unfinished {
                return Err("Claude stream ended with an unfinished content block".into());
            }
            if block["type"] == "tool_use" {
                if let Some(input) = self
                    .partial_inputs
                    .get(index)
                    .filter(|input| !input.is_empty())
                {
                    match serde_json::from_str::<Value>(input) {
                        Ok(value) if value.is_object() => block["input"] = value,
                        _ if unfinished => block["__iogw_partial_json"] = json!(input),
                        _ => return Err("Claude stream returned invalid tool argument JSON".into()),
                    }
                }
                if !unfinished && !block["input"].is_object() {
                    return Err("Claude stream returned non-object tool arguments".into());
                }
            }
        }
        message["content"] = Value::Array(self.blocks.into_values().collect());
        Ok(message)
    }
}

async fn collect_anthropic_message(
    resp: reqwest::Response,
) -> Result<(Value, crate::UsageMetrics), String> {
    if !resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.contains("text/event-stream"))
    {
        // Custom Anthropic-compatible servers can return JSON despite stream:true.
        let message: Value = resp
            .json()
            .await
            .map_err(|err| claude_transport_error("body read", err))?;
        let mut usage = crate::usage_metrics_from_response_value(&message);
        crate::quota_usage::preserve_native_usage(&mut usage, &message, "anthropic");
        return Ok((message, usage));
    }
    let mut upstream = resp.bytes_stream();
    let mut message = AnthropicMessageAccumulator::default();
    let mut usage = AnthropicSseUsageTracker::default();
    while let Some(chunk) = upstream.next().await {
        let bytes = chunk.map_err(|err| claude_transport_error("stream read", err))?;
        usage.push(&bytes);
        message.push(&bytes)?;
        if message.stopped {
            break;
        }
    }
    Ok((message.finish()?, usage.finish()?.unwrap_or_default()))
}

async fn post_anthropic_messages(
    client: &reqwest::Client,
    account: &super::accounts::ClaudeAccount,
    access_token: &str,
    incoming_headers: &HeaderMap,
    body: Bytes,
    stream: bool,
) -> Result<reqwest::Response, String> {
    let base_url = super::auth::api_base_url(account.api_base_url.as_deref());
    let (body, body_beta) = prepare_anthropic_body(body);
    let merged_beta = merge_beta_headers(
        incoming_headers
            .get("anthropic-beta")
            .and_then(|value| value.to_str().ok()),
        body_beta.as_deref(),
    );
    let beta = super::auth::merged_beta_header(merged_beta.as_deref());
    let anthropic_version = incoming_headers
        .get("anthropic-version")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("2023-06-01");
    let mut request = client
        .post(anthropic_messages_url(&base_url))
        .header("Authorization", format!("Bearer {}", access_token.trim()))
        .header("Content-Type", "application/json")
        .header(
            "Accept",
            if stream {
                "text/event-stream"
            } else {
                "application/json"
            },
        )
        .header("anthropic-version", anthropic_version)
        .header("User-Agent", super::CLAUDE_CODE_CLI_USER_AGENT)
        .header("x-app", "cli")
        .header("Accept-Encoding", "identity")
        .timeout(Duration::from_secs(if stream {
            STREAM_REQUEST_TIMEOUT_SECS
        } else {
            REQUEST_TIMEOUT_SECS
        }))
        .body(body);
    request = request.header("anthropic-beta", beta);
    for (key, value) in incoming_headers.iter() {
        let lower = key.as_str().to_ascii_lowercase();
        if should_drop_anthropic_incoming_header(&lower) {
            continue;
        }
        if matches!(
            lower.as_str(),
            "anthropic-version" | "anthropic-beta" | "accept" | "content-type"
        ) {
            continue;
        }
        request = request.header(key, value);
    }
    request
        .send()
        .await
        .map_err(|err| claude_transport_error("request", err))
}

fn anthropic_messages_url(base_url: &str) -> String {
    let base = base_url.trim_end_matches('/');
    if base.ends_with("/v1/messages") {
        return format!("{}?beta=true", base);
    }
    if base.ends_with("/v1") {
        return format!("{}/messages?beta=true", base);
    }
    format!("{}/v1/messages?beta=true", base)
}

fn prepare_anthropic_body(body: Bytes) -> (Bytes, Option<String>) {
    let Ok(mut value) = serde_json::from_slice::<Value>(&body) else {
        return (body, None);
    };
    let Some(object) = value.as_object_mut() else {
        return (body, None);
    };
    let beta = object
        .remove("betas")
        .and_then(normalize_beta_value)
        .filter(|value| !value.is_empty());
    inject_claude_code_system(object);
    let rebuilt = serde_json::to_vec(&value).unwrap_or_else(|_| body.to_vec());
    (Bytes::from(rebuilt), beta)
}

/// Calculate the conservative input footprint of the exact Anthropic JSON
/// payload this adapter will dispatch.  This is used by the outer API-key
/// preflight as well as account telemetry, so injected Claude Code text and
/// Responses-to-Anthropic conversion cannot evade a key cap or budget.
pub(crate) fn policy_prompt_metrics(
    upstream_path: &str,
    body: &Bytes,
) -> Option<crate::PromptMetrics> {
    let raw = serde_json::from_slice::<Value>(body).ok()?;
    let prepared = if upstream_path == "anthropic/v1/messages" {
        prepare_anthropic_body(body.clone()).0
    } else {
        let model = raw.get("model")?.as_str()?.trim();
        if model.is_empty() {
            return None;
        }
        let anthropic = responses_to_anthropic_messages(&raw, model, false).ok()?;
        let body = Bytes::from(serde_json::to_vec(&anthropic).ok()?);
        prepare_anthropic_body(body).0
    };
    serde_json::from_slice::<Value>(&prepared)
        .ok()
        .map(|value| crate::prompt_metrics_from_request_value(&value))
}

fn inject_claude_code_system(object: &mut Map<String, Value>) {
    let missing_parts = missing_claude_code_system_parts(object.get("system"));
    if missing_parts.is_empty() {
        return;
    }

    let mut blocks = missing_parts
        .into_iter()
        .map(|text| json!({ "type": "text", "text": text }))
        .collect::<Vec<_>>();
    match object.remove("system") {
        None | Some(Value::Null) => {}
        Some(Value::Array(mut items)) => blocks.append(&mut items),
        Some(Value::String(text)) => {
            let text = text.trim();
            if !text.is_empty() {
                blocks.push(json!({ "type": "text", "text": text }));
            }
        }
        Some(Value::Object(item)) => {
            if is_text_block(&item) {
                blocks.push(Value::Object(item));
            } else {
                let existing = Value::Object(item);
                if let Some(text) = extract_text_value(Some(&existing)) {
                    blocks.push(json!({ "type": "text", "text": text }));
                }
            }
        }
        Some(existing) => {
            if let Some(text) = extract_text_value(Some(&existing)) {
                blocks.push(json!({ "type": "text", "text": text }));
            }
        }
    }

    object.insert("system".to_string(), Value::Array(blocks));
}

fn missing_claude_code_system_parts(system: Option<&Value>) -> Vec<&'static str> {
    let existing = extract_text_value(system);
    let existing = existing.as_deref().unwrap_or_default();
    let mut missing = Vec::new();
    if !existing.contains(CLAUDE_CODE_ATTRIBUTION_MARKER) {
        missing.push(CLAUDE_CODE_ATTRIBUTION_MARKER);
    }
    if !existing.contains(CLAUDE_CODE_AGENT_PROMPT) {
        missing.push(CLAUDE_CODE_AGENT_PROMPT);
    }
    missing
}

fn is_text_block(value: &serde_json::Map<String, Value>) -> bool {
    matches!(
        value.get("type").and_then(|value| value.as_str()),
        Some("text")
    ) && value
        .get("text")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .is_some()
}

fn normalize_beta_value(value: Value) -> Option<String> {
    match value {
        Value::String(text) => {
            let trimmed = text.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_string())
        }
        Value::Array(values) => {
            let betas = values
                .iter()
                .filter_map(|item| item.as_str())
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .collect::<Vec<_>>();
            (!betas.is_empty()).then(|| betas.join(","))
        }
        _ => None,
    }
}

fn merge_beta_headers(incoming: Option<&str>, extra: Option<&str>) -> Option<String> {
    let mut values = Vec::new();
    for source in [incoming, extra] {
        if let Some(source) = source {
            for beta in source.split(',') {
                let beta = beta.trim();
                if !beta.is_empty() && !values.iter().any(|existing| existing == beta) {
                    values.push(beta.to_string());
                }
            }
        }
    }
    (!values.is_empty()).then(|| values.join(","))
}

pub(crate) fn responses_to_anthropic_messages(
    raw: &Value,
    model: &str,
    stream: bool,
) -> Result<Value, String> {
    let direct_messages = raw.get("messages").and_then(|value| value.as_array());
    let chat_messages = if let Some(messages) = direct_messages {
        normalize_chat_messages(messages)?
    } else {
        build_messages_from_input(raw)?
    };

    let mut object = Map::new();
    object.insert("model".to_string(), Value::String(model.to_string()));
    object.insert(
        "max_tokens".to_string(),
        raw.get("max_tokens")
            .and_then(|value| value.as_u64())
            .or_else(|| raw.get("max_output_tokens").and_then(Value::as_u64))
            .unwrap_or_else(|| default_max_tokens(model))
            .into(),
    );
    object.insert("stream".to_string(), Value::Bool(stream));

    let mut system_parts = Vec::new();
    if let Some(system) = extract_text_value(raw.get("system")) {
        system_parts.push(system);
    }
    if let Some(instructions) = raw
        .get("instructions")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        system_parts.push(instructions.to_string());
    }

    if let Some(temperature) = raw.get("temperature").and_then(|value| value.as_f64()) {
        object.insert("temperature".to_string(), json!(temperature));
    }
    if let Some(top_p) = raw.get("top_p").and_then(|value| value.as_f64()) {
        object.insert("top_p".to_string(), json!(top_p));
    }
    if let Some(stop) = raw.get("stop_sequences").or_else(|| raw.get("stop")) {
        object.insert("stop_sequences".to_string(), normalize_stop_sequences(stop));
    }
    let messages = chat_messages_to_anthropic(chat_messages, &mut system_parts);
    if messages.is_empty() {
        return Err("input or messages is required".to_string());
    }
    object.insert("messages".to_string(), Value::Array(messages));
    if !system_parts.is_empty() {
        object.insert(
            "system".to_string(),
            Value::Array(
                system_parts
                    .into_iter()
                    .map(|text| json!({ "type": "text", "text": text }))
                    .collect(),
            ),
        );
    }

    if let Some(tools) = normalize_tools(raw.get("tools")) {
        object.insert("tools".to_string(), Value::Array(tools));
    }
    if let Some(tool_choice) = normalize_tool_choice(raw.get("tool_choice")) {
        object.insert("tool_choice".to_string(), tool_choice);
    }
    for field in [
        "metadata",
        "thinking",
        "context_management",
        "output_config",
        "speed",
    ] {
        if let Some(value) = raw.get(field).filter(|value| !value.is_null()) {
            object.insert(field.to_string(), value.clone());
        }
    }

    Ok(Value::Object(object))
}

fn default_max_tokens(model: &str) -> u64 {
    // Current Claude models include adaptive thinking in max_tokens. A 4096
    // default can be exhausted entirely on thinking, causing Codex to replay
    // the same failed request without ever reaching a tool or final answer.
    // Keep the legacy default for older/unknown compatible models, whose
    // output limits may be lower, and always respect an explicit caller cap.
    let Some((family, version)) = model
        .strip_prefix("claude-")
        .and_then(|model| model.split_once('-'))
    else {
        return DEFAULT_MAX_TOKENS;
    };
    let major = version
        .split('-')
        .next()
        .and_then(|version| version.parse::<u32>().ok());
    if matches!(family, "sonnet" | "opus" | "haiku" | "fable" | "mythos")
        && major.is_some_and(|major| major >= 4)
    {
        MODERN_DEFAULT_MAX_TOKENS
    } else {
        DEFAULT_MAX_TOKENS
    }
}

fn chat_messages_to_anthropic(messages: Vec<Value>, system_parts: &mut Vec<String>) -> Vec<Value> {
    let mut out = Vec::new();
    let mut index = 0;
    while index < messages.len() {
        let message = &messages[index];
        match message_role(message).unwrap_or("user") {
            "system" | "developer" => {
                if let Some(text) = extract_text_value(message.get("content")) {
                    system_parts.push(text);
                }
                index += 1;
            }
            "assistant" => {
                if let Some(tool_calls) =
                    message.get("tool_calls").and_then(|value| value.as_array())
                {
                    let mut next = index + 1;
                    let mut tool_messages = Vec::new();
                    while next < messages.len() && message_role(&messages[next]) == Some("tool") {
                        tool_messages.push(messages[next].clone());
                        next += 1;
                    }

                    let available_ids = tool_messages
                        .iter()
                        .filter_map(|tool| {
                            tool.get("tool_call_id")
                                .and_then(|value| value.as_str())
                                .map(str::to_string)
                        })
                        .collect::<Vec<_>>();
                    let mut content = Vec::new();
                    if let Some(reasoning) = assistant_reasoning_text(message) {
                        content.push(json!({ "type": "thinking", "thinking": reasoning }));
                    }
                    content.extend(anthropic_content_blocks(&message["content"]));
                    let mut retained_ids = Vec::new();
                    for tool_call in tool_calls {
                        let Some(id) = tool_call_id(tool_call) else {
                            continue;
                        };
                        if !available_ids.iter().any(|available| available == &id) {
                            continue;
                        }
                        if let Some(block) = tool_call_to_anthropic(tool_call, &id) {
                            retained_ids.push(id);
                            content.push(block);
                        }
                    }
                    if !content.is_empty() {
                        out.push(json!({
                            "role": "assistant",
                            "content": content
                        }));
                    }

                    let results = tool_messages
                        .iter()
                        .filter_map(|tool| {
                            let id = tool.get("tool_call_id").and_then(|value| value.as_str())?;
                            if retained_ids.iter().any(|retained| retained == id) {
                                tool_message_to_anthropic_result(tool)
                            } else {
                                None
                            }
                        })
                        .collect::<Vec<_>>();
                    if !results.is_empty() {
                        out.push(json!({
                            "role": "user",
                            "content": results
                        }));
                    }

                    index = next;
                } else {
                    let mut content = Vec::new();
                    if let Some(reasoning) = assistant_reasoning_text(message) {
                        content.push(json!({ "type": "thinking", "thinking": reasoning }));
                    }
                    content.extend(anthropic_content_blocks(&message["content"]));
                    if !content.is_empty() {
                        out.push(json!({
                            "role": "assistant",
                            "content": content
                        }));
                    }
                    index += 1;
                }
            }
            "tool" => {
                if let Some(text) = tool_result_context_text(message) {
                    out.push(json!({
                        "role": "user",
                        "content": [{ "type": "text", "text": text }]
                    }));
                }
                index += 1;
            }
            _ => {
                let blocks = anthropic_content_blocks(&message["content"]);
                if !blocks.is_empty() {
                    out.push(json!({
                        "role": "user",
                        "content": blocks
                    }));
                }
                index += 1;
            }
        }
    }
    out
}

fn normalize_chat_messages(messages: &[Value]) -> Result<Vec<Value>, String> {
    let mut out = Vec::new();
    for message in messages {
        let role = normalize_message_role(message.get("role").and_then(|value| value.as_str()));
        let original_content = message
            .get("content")
            .cloned()
            .unwrap_or(Value::String(String::new()));
        let mut normalized = json!({
            "role": role,
            "content": original_content
        });

        if role == "assistant" {
            if let Some(reasoning_content) = message
                .get("reasoning_content")
                .and_then(|value| value.as_str())
                .map(str::trim)
                .filter(|value| !value.is_empty())
            {
                normalized["reasoning_content"] = json!(reasoning_content);
            }
            if let Some(tool_calls) = message.get("tool_calls").and_then(|value| value.as_array()) {
                normalized["tool_calls"] = Value::Array(normalize_direct_tool_calls(tool_calls)?);
            }
        } else if role == "tool" {
            let tool_call_id = message
                .get("tool_call_id")
                .and_then(|value| value.as_str())
                .ok_or_else(|| "tool messages require tool_call_id".to_string())?;
            normalized["tool_call_id"] = json!(tool_call_id);
        }

        let has_content = match normalized.get("content") {
            Some(Value::String(text)) => !text.trim().is_empty(),
            Some(Value::Array(items)) => !items.is_empty(),
            _ => false,
        };
        if role != "assistant" && role != "tool" && !has_content {
            continue;
        }
        out.push(normalized);
    }
    Ok(out)
}

fn normalize_message_role(role: Option<&str>) -> &'static str {
    match role
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("user")
        .to_ascii_lowercase()
        .as_str()
    {
        "system" | "developer" => "system",
        "assistant" => "assistant",
        "tool" => "tool",
        _ => "user",
    }
}

fn build_messages_from_input(request_value: &Value) -> Result<Vec<Value>, String> {
    let mut messages = Vec::new();
    let input = request_value
        .get("input")
        .ok_or_else(|| "input is required when messages are not provided".to_string())?;

    if let Some(prompt) = input
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        messages.push(json!({
            "role": "user",
            "content": prompt
        }));
        return Ok(messages);
    }

    let Some(items) = input.as_array() else {
        return Err("only string or array input is supported for Claude responses".to_string());
    };

    let mut pending = PendingAssistantTurn::default();

    for item in items {
        let item_type = item
            .get("type")
            .and_then(|value| value.as_str())
            .unwrap_or("");
        match item_type {
            "reasoning" => {
                if let Some(text) = extract_reasoning_text(item) {
                    pending.push_reasoning(text);
                }
            }
            "function_call" => {
                let call_id = item
                    .get("call_id")
                    .or_else(|| item.get("id"))
                    .and_then(|value| value.as_str())
                    .ok_or_else(|| "function_call items require call_id".to_string())?;
                let name = item
                    .get("name")
                    .and_then(|value| value.as_str())
                    .ok_or_else(|| "function_call items require name".to_string())?;
                let arguments = item
                    .get("arguments")
                    .and_then(|value| value.as_str())
                    .unwrap_or("{}");
                if let Some(reasoning_content) = item
                    .get("reasoning_content")
                    .and_then(|value| value.as_str())
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                {
                    pending.push_reasoning(reasoning_content.to_string());
                }
                pending.tool_calls.push(json!({
                    "id": call_id,
                    "type": "function",
                    "function": {
                        "name": name,
                        "arguments": arguments
                    }
                }));
            }
            "function_call_output" => {
                flush_pending_assistant_turn(&mut messages, &mut pending);
                let call_id = item
                    .get("call_id")
                    .and_then(|value| value.as_str())
                    .ok_or_else(|| "function_call_output items require call_id".to_string())?;
                messages.push(json!({
                    "role": "tool",
                    "tool_call_id": call_id,
                    "content": content_value_to_text(item.get("output").or_else(|| item.get("content")).unwrap_or(&Value::Null))
                }));
            }
            _ => {
                let role =
                    normalize_message_role(item.get("role").and_then(|value| value.as_str()));
                let blocks = anthropic_content_blocks(item);
                let direct_tool_calls = item.get("tool_calls").and_then(|value| value.as_array());

                if role == "assistant" && direct_tool_calls.is_some() {
                    flush_pending_assistant_turn(&mut messages, &mut pending);
                    let mut assistant = json!({
                        "role": "assistant",
                        "content": if blocks.is_empty() { json!("") } else { json!(blocks) },
                        "tool_calls": Value::Array(normalize_direct_tool_calls(direct_tool_calls.unwrap())?)
                    });
                    if let Some(reasoning_content) = item
                        .get("reasoning_content")
                        .and_then(|value| value.as_str())
                        .map(str::trim)
                        .filter(|value| !value.is_empty())
                    {
                        assistant["reasoning_content"] = json!(reasoning_content);
                    }
                    messages.push(assistant);
                    continue;
                }

                if role == "assistant" {
                    let text = blocks
                        .iter()
                        .filter_map(|block| {
                            if block.get("type").and_then(|value| value.as_str()) == Some("text") {
                                block
                                    .get("text")
                                    .and_then(|value| value.as_str())
                                    .map(str::to_string)
                            } else {
                                None
                            }
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    if !text.is_empty() {
                        pending.push_content(text);
                    }
                } else if role == "tool" {
                    flush_pending_assistant_turn(&mut messages, &mut pending);
                    let Some(tool_call_id) =
                        item.get("tool_call_id").and_then(|value| value.as_str())
                    else {
                        return Err("tool items require tool_call_id".to_string());
                    };
                    let tool_text = blocks
                        .iter()
                        .filter_map(|block| {
                            if block.get("type").and_then(|value| value.as_str()) == Some("text") {
                                block
                                    .get("text")
                                    .and_then(|value| value.as_str())
                                    .map(str::to_string)
                            } else {
                                None
                            }
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    messages.push(json!({
                        "role": "tool",
                        "tool_call_id": tool_call_id,
                        "content": tool_text
                    }));
                } else if !blocks.is_empty() {
                    flush_pending_assistant_turn(&mut messages, &mut pending);
                    messages.push(json!({
                        "role": role,
                        "content": blocks
                    }));
                }
            }
        }
    }

    flush_pending_assistant_turn(&mut messages, &mut pending);

    if messages.is_empty() {
        return Err("input did not contain any text or tool content".to_string());
    }

    Ok(messages)
}

#[derive(Default)]
struct PendingAssistantTurn {
    content: Option<String>,
    reasoning_content: Option<String>,
    tool_calls: Vec<Value>,
}

impl PendingAssistantTurn {
    fn push_content(&mut self, text: String) {
        if text.trim().is_empty() {
            return;
        }
        match &mut self.content {
            Some(existing) => {
                if !existing.is_empty() {
                    existing.push('\n');
                }
                existing.push_str(&text);
            }
            None => self.content = Some(text),
        }
    }

    fn push_reasoning(&mut self, text: String) {
        if text.trim().is_empty() {
            return;
        }
        match &mut self.reasoning_content {
            Some(existing) => {
                if !existing.is_empty() {
                    existing.push('\n');
                }
                existing.push_str(&text);
            }
            None => self.reasoning_content = Some(text),
        }
    }
}

fn flush_pending_assistant_turn(messages: &mut Vec<Value>, pending: &mut PendingAssistantTurn) {
    if pending.tool_calls.is_empty() && pending.content.is_none() {
        pending.reasoning_content = None;
        return;
    }

    let mut assistant = json!({
        "role": "assistant",
        "content": pending.content.take().unwrap_or_default()
    });
    if !pending.tool_calls.is_empty() {
        assistant["tool_calls"] = Value::Array(std::mem::take(&mut pending.tool_calls));
    }
    if let Some(reasoning_content) = pending.reasoning_content.take() {
        assistant["reasoning_content"] = json!(reasoning_content);
    }
    messages.push(assistant);
}

fn extract_text_value(value: Option<&Value>) -> Option<String> {
    let value = value?;
    if let Some(content) = value.as_str() {
        let content = content.trim();
        if !content.is_empty() {
            return Some(content.to_string());
        }
    }

    if let Some(content) = value.get("content").and_then(|item| item.as_str()) {
        let content = content.trim();
        if !content.is_empty() {
            return Some(content.to_string());
        }
    }

    let parts = value
        .get("content")
        .and_then(|item| item.as_array())
        .or_else(|| value.as_array())?;
    let mut out = String::new();
    for part in parts {
        let part_type = part
            .get("type")
            .and_then(|item| item.as_str())
            .unwrap_or("");
        let text = part
            .get("text")
            .and_then(|item| item.as_str())
            .or_else(|| part.get("input_text").and_then(|item| item.as_str()))
            .or_else(|| part.get("output_text").and_then(|item| item.as_str()))
            .or_else(|| part.get("content").and_then(|item| item.as_str()));
        if matches!(
            part_type,
            "" | "text" | "input_text" | "output_text" | "summary_text"
        ) {
            if let Some(text) = text.map(str::trim).filter(|text| !text.is_empty()) {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(text);
            }
        }
    }

    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

fn anthropic_content_blocks(value: &Value) -> Vec<Value> {
    let target = if value.is_object() && value.get("content").is_some() {
        value.get("content").unwrap()
    } else {
        value
    };

    if let Some(arr) = target.as_array() {
        let all_blocks = arr.iter().all(|item| {
            item.is_object()
                && matches!(
                    item.get("type").and_then(|value| value.as_str()),
                    Some("text" | "image" | "tool_use" | "tool_result" | "thinking")
                )
        });
        if all_blocks {
            return arr.clone();
        }
    }

    let parts = classify_content(Some(target));
    let mut blocks = Vec::new();
    for part in parts {
        match part {
            PartKind::Text(text) => blocks.push(json!({ "type": "text", "text": text })),
            PartKind::Image(url) => {
                if let Some(block) = anthropic_image_block(&url) {
                    blocks.push(block);
                }
            }
            PartKind::Other(_) => {}
        }
    }
    blocks
}

fn anthropic_image_block(url: &str) -> Option<Value> {
    if is_data_url(url) {
        let (mime_type, payload) = split_data_url(url)?;
        return Some(json!({
            "type": "image",
            "source": {
                "type": "base64",
                "media_type": mime_type,
                "data": payload
            }
        }));
    }
    if url.starts_with("http://") || url.starts_with("https://") {
        return Some(json!({
            "type": "image",
            "source": {
                "type": "url",
                "url": url
            }
        }));
    }
    None
}

fn message_role(message: &Value) -> Option<&str> {
    message.get("role").and_then(|value| value.as_str())
}

fn assistant_reasoning_text(message: &Value) -> Option<String> {
    message
        .get("reasoning_content")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn tool_call_id(tool_call: &Value) -> Option<String> {
    tool_call
        .get("id")
        .and_then(|value| value.as_str())
        .map(str::to_string)
}

fn normalize_direct_tool_calls(tool_calls: &[Value]) -> Result<Vec<Value>, String> {
    let mut normalized = Vec::new();
    for tool_call in tool_calls {
        let tool_type = tool_call
            .get("type")
            .and_then(|value| value.as_str())
            .unwrap_or("function");
        let function = tool_call
            .get("function")
            .ok_or_else(|| "assistant tool_calls require function payloads".to_string())?;
        let name = function
            .get("name")
            .and_then(|value| value.as_str())
            .ok_or_else(|| "assistant tool_calls require function.name".to_string())?;
        let arguments = function
            .get("arguments")
            .and_then(|value| value.as_str())
            .unwrap_or("{}");
        let tool_call_id = tool_call
            .get("id")
            .and_then(|value| value.as_str())
            .map(str::to_string)
            .unwrap_or_else(|| format!("call_{}", Uuid::new_v4().simple()));
        normalized.push(json!({
            "id": tool_call_id,
            "type": tool_type,
            "function": {
                "name": name,
                "arguments": arguments
            }
        }));
    }
    Ok(normalized)
}

fn tool_call_to_anthropic(tool_call: &Value, id: &str) -> Option<Value> {
    let function = tool_call.get("function")?;
    let name = function.get("name").and_then(|value| value.as_str())?;
    let input = function
        .get("arguments")
        .and_then(|value| value.as_str())
        .and_then(|arguments| serde_json::from_str::<Value>(arguments).ok())
        .or_else(|| function.get("arguments").cloned())
        .unwrap_or_else(|| json!({}));
    Some(json!({
        "type": "tool_use",
        "id": id,
        "name": name,
        "input": input
    }))
}

fn tool_message_to_anthropic_result(tool: &Value) -> Option<Value> {
    let tool_use_id = tool.get("tool_call_id").and_then(|value| value.as_str())?;
    Some(json!({
        "type": "tool_result",
        "tool_use_id": tool_use_id,
        "content": content_value_to_text(tool.get("content").unwrap_or(&Value::Null))
    }))
}

fn tool_result_context_text(tool: &Value) -> Option<String> {
    let tool_call_id = tool
        .get("tool_call_id")
        .and_then(|value| value.as_str())
        .unwrap_or("unknown");
    let content = content_value_to_text(tool.get("content").unwrap_or(&Value::Null));
    if content.trim().is_empty() {
        None
    } else {
        Some(format!("Tool result for {}:\n{}", tool_call_id, content))
    }
}

fn extract_reasoning_text(item: &Value) -> Option<String> {
    if let Some(reasoning_content) = item
        .get("reasoning_content")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return Some(reasoning_content.to_string());
    }
    if let Some(text) = extract_text_value(item.get("summary")) {
        return Some(text);
    }
    extract_text_value(item.get("content"))
}

fn content_value_to_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Array(items) => items
            .iter()
            .map(content_value_to_text)
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Object(map) => map
            .get("text")
            .or_else(|| map.get("content"))
            .and_then(|value| value.as_str())
            .map(|value| value.to_string())
            .unwrap_or_else(|| value.to_string()),
        _ => String::new(),
    }
}

fn normalize_tools(tools: Option<&Value>) -> Option<Vec<Value>> {
    let tools = tools?.as_array()?;
    let out = tools
        .iter()
        .filter_map(|tool| {
            if tool.get("type").and_then(|value| value.as_str()) != Some("function") {
                return None;
            }
            if let Some(function) = tool.get("function") {
                let name = function.get("name").and_then(|value| value.as_str())?;
                return Some(json!({
                    "name": name,
                    "description": function.get("description").and_then(|value| value.as_str()).unwrap_or(""),
                    "input_schema": function.get("parameters").cloned().unwrap_or_else(|| json!({"type": "object"}))
                }));
            }
            let name = tool.get("name").and_then(|value| value.as_str())?;
            Some(json!({
                "name": name,
                "description": tool.get("description").and_then(|value| value.as_str()).unwrap_or(""),
                "input_schema": tool.get("parameters").cloned().unwrap_or_else(|| json!({"type": "object"}))
            }))
        })
        .collect::<Vec<_>>();
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

fn normalize_tool_choice(value: Option<&Value>) -> Option<Value> {
    match value {
        Some(Value::String(text)) => match text.as_str() {
            "auto" => Some(json!({"type": "auto"})),
            "required" => Some(json!({"type": "any"})),
            "none" => Some(json!({"type": "none"})),
            _ => None,
        },
        Some(Value::Object(map)) => {
            if let Some(function) = map.get("function").and_then(|value| value.as_object()) {
                if let Some(name) = function.get("name").and_then(|value| value.as_str()) {
                    return Some(json!({"type": "tool", "name": name}));
                }
            }
            if let Some(name) = map.get("name").and_then(|value| value.as_str()) {
                return Some(json!({"type": "tool", "name": name}));
            }
            None
        }
        _ => None,
    }
}

fn normalize_stop_sequences(value: &Value) -> Value {
    match value {
        Value::String(text) => json!([text]),
        Value::Array(_) => value.clone(),
        _ => json!([]),
    }
}

pub(crate) fn anthropic_message_to_response(message: &Value, model: &str) -> Value {
    let mut output: Vec<Value> = Vec::new();
    for block in message
        .get("content")
        .and_then(|value| value.as_array())
        .into_iter()
        .flatten()
    {
        match block.get("type").and_then(|value| value.as_str()) {
            Some("text") => {
                if let Some(text) = block.get("text").and_then(|value| value.as_str()) {
                    let part = json!({
                        "type": "output_text", "text": text, "annotations": [], "logprobs": []
                    });
                    // Coalesce adjacent text blocks without moving text ahead of
                    // the thinking or tool blocks that preceded it upstream.
                    if let Some(item) = output.last_mut().filter(|item| item["type"] == "message") {
                        item["content"].as_array_mut().unwrap().push(part);
                    } else {
                        output.push(json!({
                            "id": format!("msg_{}", Uuid::new_v4().simple()),
                            "type": "message",
                            "role": "assistant",
                            "content": [part],
                            "status": "completed"
                        }));
                    }
                }
            }
            Some("thinking") => {
                if let Some(text) = block.get("thinking").and_then(|value| value.as_str()) {
                    output.push(json!({
                        "id": format!("rs_{}", Uuid::new_v4().simple()),
                        "type": "reasoning",
                        "summary": [{"type": "summary_text", "text": text}],
                        "status": "completed"
                    }));
                }
            }
            Some("tool_use") => {
                let id = block
                    .get("id")
                    .and_then(|value| value.as_str())
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("call_{}", Uuid::new_v4().simple()));
                let name = block
                    .get("name")
                    .and_then(|value| value.as_str())
                    .unwrap_or("tool");
                let arguments = block
                    .get("__iogw_partial_json")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .or_else(|| block.get("input").map(Value::to_string))
                    .unwrap_or_else(|| "{}".to_string());
                output.push(json!({
                    "type": "function_call",
                    "id": format!("fc_{}", Uuid::new_v4().simple()),
                    "call_id": id,
                    "name": name,
                    "arguments": arguments,
                    "status": "completed"
                }));
            }
            _ => {}
        }
    }
    let truncated = message.get("stop_reason").and_then(Value::as_str) == Some("max_tokens");
    if truncated {
        if let Some(item) = output.last_mut() {
            item["status"] = json!("incomplete");
        }
    }

    let usage = message.get("usage").cloned().unwrap_or_else(|| json!({}));
    let input_tokens = usage
        .get("input_tokens")
        .and_then(|value| value.as_u64())
        .unwrap_or(0);
    let output_tokens = usage
        .get("output_tokens")
        .and_then(|value| value.as_u64())
        .unwrap_or(0);
    json!({
        "id": message.get("id").and_then(|value| value.as_str()).unwrap_or("resp_claude"),
        "object": "response",
        "created_at": chrono::Utc::now().timestamp(),
        "model": message.get("model").and_then(|value| value.as_str()).unwrap_or(model),
        "status": if truncated { "incomplete" } else { "completed" },
        "error": null,
        "incomplete_details": if truncated { json!({"reason": "max_output_tokens"}) } else { Value::Null },
        "output": output,
        "usage": {
            "input_tokens": input_tokens,
            "output_tokens": output_tokens,
            "total_tokens": input_tokens + output_tokens
        }
    })
}

pub(crate) fn responses_sse(mut response: Value) -> axum::response::Response {
    // This emitter is also used by compatible APIs. Assign missing IDs once,
    // before making any events, so deltas, done events and the final snapshot
    // all refer to the same items.
    for item in response
        .get_mut("output")
        .and_then(Value::as_array_mut)
        .into_iter()
        .flatten()
    {
        if item
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .is_none()
        {
            item["id"] = json!(format!("item_{}", Uuid::new_v4().simple()));
        }
    }
    let mut events = String::new();
    let mut sequence_number = 0_u64;
    let mut emit = |kind: &str, mut data: Value| {
        data["type"] = json!(kind);
        data["sequence_number"] = json!(sequence_number);
        sequence_number += 1;
        events.push_str(&format!("event: {kind}\ndata: {data}\n\n"));
    };
    // The upstream call is buffered, but the downstream lifecycle must still
    // start empty and finish each item. Codex consumes output_item.done to
    // finalize answers and dispatch tools; response.completed alone is not enough.
    let mut initial = response.clone();
    initial["status"] = json!("in_progress");
    initial["output"] = json!([]);
    initial["usage"] = Value::Null;
    initial["error"] = Value::Null;
    initial["incomplete_details"] = Value::Null;
    emit("response.created", json!({"response": initial}));
    emit("response.in_progress", json!({"response": initial}));
    for (output_index, item) in response
        .get("output")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
    {
        let item_id = &item["id"];
        let unfinished_tool = item["type"] == "function_call"
            && matches!(item["status"].as_str(), Some("incomplete" | "in_progress"));
        let mut added = item.clone();
        added["status"] = json!("in_progress");
        match item["type"].as_str() {
            Some("message") => added["content"] = json!([]),
            Some("function_call") => added["arguments"] = json!(""),
            Some("reasoning") => added["summary"] = json!([]),
            _ => {}
        }
        emit(
            "response.output_item.added",
            json!({"output_index": output_index, "item": added}),
        );
        match item["type"].as_str() {
            Some("message") => {
                for (content_index, part) in item
                    .get("content")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .enumerate()
                {
                    let (field, prefix) = match part["type"].as_str() {
                        Some("output_text") => ("text", "response.output_text"),
                        Some("refusal") => ("refusal", "response.refusal"),
                        _ => continue,
                    };
                    let text = part.get(field).and_then(Value::as_str).unwrap_or("");
                    let mut empty_part = part.clone();
                    empty_part[field] = json!("");
                    let common = json!({
                        "item_id": item_id, "output_index": output_index, "content_index": content_index
                    });
                    let mut data = common.clone();
                    data["part"] = empty_part;
                    emit("response.content_part.added", data);
                    let mut data = common.clone();
                    data["delta"] = json!(text);
                    if field == "text" {
                        data["logprobs"] = json!([]);
                    }
                    if !text.is_empty() {
                        emit(&format!("{prefix}.delta"), data);
                    }
                    let mut data = common.clone();
                    data[field] = json!(text);
                    if field == "text" {
                        data["logprobs"] = json!([]);
                    }
                    emit(&format!("{prefix}.done"), data);
                    let mut data = common;
                    data["part"] = part.clone();
                    emit("response.content_part.done", data);
                }
            }
            Some("function_call") => {
                let arguments = item.get("arguments").and_then(Value::as_str).unwrap_or("");
                if !arguments.is_empty() {
                    emit(
                        "response.function_call_arguments.delta",
                        json!({
                            "item_id": item_id, "output_index": output_index, "delta": arguments
                        }),
                    );
                }
                if !unfinished_tool {
                    emit(
                        "response.function_call_arguments.done",
                        json!({
                            "item_id": item_id, "output_index": output_index, "arguments": arguments
                        }),
                    );
                }
            }
            Some("reasoning") => {
                for (summary_index, part) in item
                    .get("summary")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .enumerate()
                {
                    let text = part.get("text").and_then(Value::as_str).unwrap_or("");
                    let common = json!({
                        "item_id": item_id, "output_index": output_index, "summary_index": summary_index
                    });
                    let mut data = common.clone();
                    data["part"] = json!({"type": "summary_text", "text": ""});
                    emit("response.reasoning_summary_part.added", data);
                    if !text.is_empty() {
                        let mut data = common.clone();
                        data["delta"] = json!(text);
                        emit("response.reasoning_summary_text.delta", data);
                    }
                    let mut data = common.clone();
                    data["text"] = json!(text);
                    emit("response.reasoning_summary_text.done", data);
                    let mut data = common;
                    data["part"] = part.clone();
                    emit("response.reasoning_summary_part.done", data);
                }
            }
            _ => {}
        }
        // An unfinished tool is visible in the final incomplete snapshot, but
        // must never be finalized as executable. Codex dispatches tools on
        // output_item.done even when that item's status says incomplete.
        if !unfinished_tool {
            emit(
                "response.output_item.done",
                json!({"output_index": output_index, "item": item}),
            );
        }
    }
    let terminal = match response.get("status").and_then(Value::as_str) {
        Some("incomplete") => "response.incomplete",
        Some("failed") => "response.failed",
        _ => "response.completed",
    };
    emit(terminal, json!({"response": response}));
    (
        StatusCode::OK,
        [
            ("Content-Type", "text/event-stream"),
            ("Cache-Control", "no-cache"),
        ],
        events,
    )
        .into_response()
}

async fn stream_anthropic_passthrough(
    state: crate::AppState,
    context: crate::UsageContext,
    resp: reqwest::Response,
    headers: HeaderMap,
) -> axum::response::Response {
    let usage_state = state.clone();
    let usage_context = context.clone();
    // Construct this before the lazy stream so dropping an unpolled response
    // still abandons the selected account and settles its conservative hold.
    let lifecycle = crate::StreamRequestGuard::new(&usage_state, &usage_context);
    let stream = async_stream::stream! {
        let mut lifecycle = lifecycle;
        let mut upstream = resp.bytes_stream();
        let mut parser = AnthropicSseUsageTracker::default();
        while let Some(chunk) = upstream.next().await {
            match chunk {
                Ok(bytes) => {
                    parser.push(&bytes);
                    if let Some(message) = parser.terminal_error() {
                        crate::record_claude_error(&usage_state, &usage_context, message);
                        lifecycle.finish();
                        yield Ok::<Bytes, std::io::Error>(bytes);
                        return;
                    }
                    yield Ok::<Bytes, std::io::Error>(bytes);
                }
                Err(err) => {
                    let message = claude_transport_error("stream read", err);
                    crate::record_claude_error(&usage_state, &usage_context, &message);
                    lifecycle.finish();
                    yield Err(std::io::Error::new(std::io::ErrorKind::Other, "stream"));
                    return;
                }
            }
        }
        match parser.finish() {
            Err(message) => crate::record_claude_error(&usage_state, &usage_context, &message),
            Ok(Some(usage)) => crate::record_claude_success(&usage_state, &usage_context, &usage),
            Ok(None) => crate::record_claude_success(
                &usage_state,
                &usage_context,
                &crate::UsageMetrics::default(),
            ),
        }
        lifecycle.finish();
    };
    (StatusCode::OK, headers, Body::from_stream(stream)).into_response()
}

fn models_to_openai_entries(models: &[super::accounts::ClaudeModelInfo]) -> Vec<Value> {
    let source = if models.is_empty() {
        fallback_model_infos()
    } else {
        models.to_vec()
    };
    source
        .into_iter()
        .map(|model| {
            json!({
                "id": model.id,
                "object": "model",
                "created": 0,
                "owned_by": "anthropic",
                "display_name": model.display_name,
                "provider": super::PROVIDER_NAME,
                "upstream_model": model.id
            })
        })
        .collect()
}

fn fallback_models() -> Vec<Value> {
    models_to_openai_entries(&fallback_model_infos())
}

fn fallback_model_infos() -> Vec<super::accounts::ClaudeModelInfo> {
    [
        "claude-opus-4-1-20250805",
        "claude-opus-4-20250514",
        "claude-sonnet-4-20250514",
        "claude-3-7-sonnet-20250219",
        "claude-3-5-haiku-20241022",
    ]
    .iter()
    .map(|id| super::accounts::ClaudeModelInfo {
        id: (*id).to_string(),
        display_name: None,
        created_at: None,
        model_type: Some("model".to_string()),
    })
    .collect()
}

fn should_drop_anthropic_incoming_header(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    crate::should_drop_incoming_header(&lower) || lower == "x-api-key"
}

fn response_headers(headers: &HeaderMap, fallback_content_type: &'static str) -> HeaderMap {
    let mut out = HeaderMap::new();
    for (key, value) in headers.iter() {
        let lower = key.as_str().to_ascii_lowercase();
        if lower == "content-length" || lower == "content-encoding" || is_hop_header(&lower) {
            continue;
        }
        out.insert(key, value.clone());
    }
    if !out.contains_key(axum::http::header::CONTENT_TYPE) {
        out.insert(
            axum::http::header::CONTENT_TYPE,
            HeaderValue::from_static(fallback_content_type),
        );
    }
    out
}

fn is_hop_header(name: &str) -> bool {
    matches!(
        name,
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

fn anthropic_error(
    status: StatusCode,
    error_type: &str,
    message: &str,
) -> axum::response::Response {
    (
        status,
        [("Content-Type", "application/json")],
        serde_json::to_vec(&json!({
            "type": "error",
            "error": {
                "type": error_type,
                "message": message
            }
        }))
        .unwrap_or_default(),
    )
        .into_response()
}

fn openai_error(status: StatusCode, error_type: &str, message: &str) -> axum::response::Response {
    (
        status,
        [("Content-Type", "application/json")],
        crate::source::v1::response::openai_error_body(message, error_type, None),
    )
        .into_response()
}

fn openai_error_body_from_anthropic(text: &str) -> Vec<u8> {
    let message = serde_json::from_str::<Value>(text)
        .ok()
        .and_then(|value| {
            value
                .get("error")
                .and_then(|error| error.get("message"))
                .and_then(|value| value.as_str())
                .map(|value| value.to_string())
        })
        .unwrap_or_else(|| text.to_string());
    crate::source::v1::response::openai_error_body(&message, "server_error", None).to_vec()
}

#[derive(Default)]
struct AnthropicSseUsageTracker {
    buffer: Vec<u8>,
    usage: crate::UsageMetrics,
    saw_usage: bool,
    saw_message_stop: bool,
    saw_final_output_usage: bool,
    terminal_error: Option<String>,
}

impl AnthropicSseUsageTracker {
    fn push(&mut self, bytes: &Bytes) {
        self.buffer.extend_from_slice(bytes);
        while let Some((event_end, delimiter_len)) = find_sse_boundary(&self.buffer) {
            let raw = self
                .buffer
                .drain(..event_end + delimiter_len)
                .collect::<Vec<_>>();
            self.absorb_event(&raw[..event_end]);
        }
    }

    fn finish(mut self) -> Result<Option<crate::UsageMetrics>, String> {
        if !self.buffer.is_empty() {
            let raw = std::mem::take(&mut self.buffer);
            self.absorb_event(&raw);
        }
        if let Some(error) = self.terminal_error {
            return Err(error);
        }
        if self.saw_usage {
            crate::quota_usage::mark_stream_usage(
                &mut self.usage,
                "anthropic",
                self.saw_message_stop && self.saw_final_output_usage,
            );
            Ok(Some(self.usage))
        } else {
            Ok(None)
        }
    }

    fn terminal_error(&self) -> Option<&str> {
        self.terminal_error.as_deref()
    }

    fn absorb_event(&mut self, raw_event: &[u8]) {
        if self.terminal_error.is_none() {
            if let Some(error) = crate::sse_terminal_error_from_event(raw_event) {
                self.terminal_error = Some(error);
                return;
            }
        }
        let Some(value) = crate::sse_event_json_value(raw_event) else {
            return;
        };
        if value.get("type").and_then(Value::as_str) == Some("message_stop") {
            self.saw_message_stop = true;
        }
        if value.get("type").and_then(Value::as_str) == Some("message_delta") {
            self.saw_final_output_usage |= value
                .get("usage")
                .is_some_and(crate::quota_usage::has_output_observation)
                && value
                    .pointer("/delta/stop_reason")
                    .and_then(Value::as_str)
                    .is_some_and(|reason| !reason.is_empty());
        }
        let usage = crate::usage_metrics_from_response_value(&value);
        if usage.raw_usage.is_some() {
            merge_usage_metrics(&mut self.usage, usage);
            self.saw_usage = true;
        }
    }
}

fn merge_usage_metrics(current: &mut crate::UsageMetrics, incoming: crate::UsageMetrics) {
    current.input_tokens = current.input_tokens.max(incoming.input_tokens);
    current.output_tokens = current.output_tokens.max(incoming.output_tokens);
    current.cache_tokens = current.cache_tokens.max(incoming.cache_tokens);
    current.reasoning_tokens = current.reasoning_tokens.max(incoming.reasoning_tokens);
    current.total_tokens = current
        .total_tokens
        .max(incoming.total_tokens)
        .max(current.input_tokens.saturating_add(current.output_tokens));
    if let Some(incoming) = incoming.raw_usage {
        crate::quota_usage::merge_cumulative_usage(&mut current.raw_usage, &incoming);
    }
}

fn find_sse_boundary(buffer: &[u8]) -> Option<(usize, usize)> {
    let crlf = buffer
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|idx| (idx, 4));
    let lf = buffer
        .windows(2)
        .position(|window| window == b"\n\n")
        .map(|idx| (idx, 2));
    crlf.into_iter().chain(lf).min_by_key(|(idx, _)| *idx)
}

fn parse_sse_data(raw_event: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(raw_event);
    let mut data_lines = Vec::new();
    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        if let Some(value) = line.strip_prefix("data:") {
            data_lines.push(value.trim_start().to_string());
        }
    }
    if data_lines.is_empty() {
        None
    } else {
        Some(data_lines.join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn accumulate_fixture(events: &[Value]) -> Result<Value, String> {
        let mut parser = AnthropicMessageAccumulator::default();
        let mut bytes = Vec::new();
        for (index, event) in events.iter().enumerate() {
            // Split UTF-8 and SSE boundaries across arbitrary network chunks,
            // with both permitted line endings in a single read.
            let delimiter = if index % 2 == 0 { "\n\n" } else { "\r\n\r\n" };
            bytes.extend_from_slice(format!("data: {event}{delimiter}").as_bytes());
        }
        for chunk in bytes.chunks(7) {
            parser.push(chunk)?;
        }
        parser.finish()
    }

    fn tool_stream_fixture(stop: &str, arguments: &str) -> Vec<Value> {
        vec![
            json!({"type":"message_start","message":{"id":"msg_test","model":"claude-sonnet-5-5",
                "type":"message","role":"assistant","content":[],"usage":{"input_tokens":5,"output_tokens":0}}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"Plan café 🐈"}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"fixture-signature"}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"ping"}),
            json!({"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"call_one","name":"shell","input":{}}}),
            json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":arguments}}),
            json!({"type":"content_block_stop","index":1}),
            json!({"type":"message_delta","delta":{"stop_reason":stop},"usage":{"output_tokens":9}}),
            json!({"type":"message_stop"}),
        ]
    }

    #[test]
    fn streamed_message_preserves_blocks_arguments_signatures_and_usage() {
        let message =
            accumulate_fixture(&tool_stream_fixture("tool_use", r#"{"cmd":"printf café"}"#))
                .unwrap();
        assert_eq!(message["content"][0]["thinking"], "Plan café 🐈");
        assert_eq!(message["content"][0]["signature"], "fixture-signature");
        assert_eq!(message["content"][1]["input"], json!({"cmd":"printf café"}));
        assert_eq!(message["stop_reason"], "tool_use");
        assert_eq!(
            message["usage"],
            json!({"input_tokens":5,"output_tokens":9})
        );
    }

    #[test]
    fn streamed_partial_tool_arguments_remain_unfinished_without_json_repair() {
        let message = accumulate_fixture(&tool_stream_fixture(
            "max_tokens",
            r#"{"cmd":"printf unfinished"#,
        ))
        .unwrap();
        let response = anthropic_message_to_response(&message, "claude-sonnet-5-5");
        assert_eq!(response["status"], "incomplete");
        assert_eq!(response["output"][1]["status"], "incomplete");
        assert_eq!(
            response["output"][1]["arguments"],
            r#"{"cmd":"printf unfinished"#
        );
        assert!(accumulate_fixture(&tool_stream_fixture(
            "tool_use",
            r#"{"cmd":"printf unfinished"#
        ))
        .is_err());
    }

    #[test]
    fn streamed_message_requires_terminal_events_before_tools_can_execute() {
        let events = tool_stream_fixture("tool_use", r#"{"cmd":"printf ok"}"#);
        assert!(accumulate_fixture(&events[..events.len() - 1])
            .unwrap_err()
            .contains("message_stop"));
        let mut missing_block_stop = events.clone();
        missing_block_stop.remove(8);
        assert!(accumulate_fixture(&missing_block_stop)
            .unwrap_err()
            .contains("unfinished content block"));
        let mut missing_stop_reason = events;
        missing_stop_reason.remove(9);
        assert!(accumulate_fixture(&missing_stop_reason)
            .unwrap_err()
            .contains("stop_reason"));
    }

    #[test]
    fn truncated_recovery_only_increases_an_implicit_supported_budget() {
        for model in [
            "claude-sonnet-5-5",
            "claude-sonnet-4-20250514",
            "claude-opus-4-5",
            "claude-haiku-4-5",
        ] {
            assert_eq!(
                recovery_max_tokens(&json!({}), model),
                Some(64_000),
                "{model}"
            );
            assert_eq!(
                recovery_max_tokens(&json!({"max_output_tokens":null}), model),
                Some(64_000)
            );
            assert_eq!(
                recovery_max_tokens(&json!({"max_tokens":null,"max_output_tokens":32}), model),
                None
            );
            assert_eq!(recovery_max_tokens(&json!({"max_tokens":32}), model), None);
        }
        for model in [
            "claude-opus-4-1-20250805",
            "claude-opus-4-20250514",
            "claude-3-7-sonnet",
            "unknown",
            "claude-sonnet-99",
        ] {
            assert_eq!(recovery_max_tokens(&json!({}), model), None, "{model}");
        }
    }

    #[test]
    fn quota_native_tool_arguments_are_literal_but_result_media_is_not() {
        let raw = json!({
            "model":"claude-sonnet-4", "max_tokens":20,
            "messages":[
                {"role":"assistant","content":[{"type":"tool_use","id":"call_lookup","name":"lookup",
                 "input":{"context":"query","file_id":"label","image_url":"an argument"}}]},
                {"role":"user","content":[{"type":"tool_result","tool_use_id":"call_lookup","content":"found"}]}
            ]
        });
        let prepared = prepare_anthropic_body(Bytes::from(raw.to_string())).0;
        let mut payload: Value = serde_json::from_slice(&prepared).unwrap();
        let bounds = crate::quota_usage::prepared_bounds("claude", &payload).unwrap();
        assert!(bounds.input_measurable);
        assert_eq!(bounds.input_upper_bound, prepared.len() as u64);
        payload["messages"][1]["content"][0]["content"] = json!([{
            "type":"image", "source":{"type":"url","url":"https://example.test/a.png"}
        }]);
        assert!(
            !crate::quota_usage::prepared_bounds("claude", &payload)
                .unwrap()
                .input_measurable
        );
    }

    #[test]
    fn quota_native_mcp_connector_is_unmeasurable_after_preparation() {
        let raw = json!({
            "model":"claude-sonnet-4", "max_tokens":20,
            "messages":[{"role":"user","content":"use the connector"}],
            "mcp_servers":[{"type":"url","url":"https://example.test/mcp","name":"fixture"}]
        });
        let prepared = prepare_anthropic_body(Bytes::from(raw.to_string())).0;
        let payload: Value = serde_json::from_slice(&prepared).unwrap();
        assert_eq!(payload["mcp_servers"], raw["mcp_servers"]);
        let bounds = crate::quota_usage::prepared_bounds("claude", &payload).unwrap();
        assert!(!bounds.input_measurable);
        assert!(bounds.output_upper_bound.is_none());
        assert_eq!(bounds.input_upper_bound, prepared.len() as u64);
    }

    #[test]
    fn quota_stream_requires_final_output_usage_not_just_message_stop() {
        for (tail, complete) in [
            ("", false),
            ("data: {\"type\":\"message_stop\"}\n\n", false),
            ("data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{}}\n\ndata: {\"type\":\"message_stop\"}\n\n", false),
            ("data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":9}}\n\ndata: {\"type\":\"message_stop\"}\n\n", true),
        ] {
            let mut tracker = AnthropicSseUsageTracker::default();
            tracker.push(&Bytes::from_static(b"data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":5,\"cache_read_input_tokens\":7,\"cache_creation_input_tokens\":3,\"output_tokens\":0}}}\n\n"));
            tracker.push(&Bytes::copy_from_slice(tail.as_bytes()));
            let metrics = tracker.finish().unwrap().unwrap();
            let normalized = crate::quota_usage::normalize("claude", metrics.raw_usage.as_ref().unwrap(), true);
            assert_eq!(normalized.trustworthy_final, complete, "{tail:?}");
            assert_eq!(normalized.input_tokens, Some(15));
            if complete { assert_eq!(normalized.output_tokens, Some(9)); }
        }
    }

    #[test]
    fn responses_payload_defaults_leave_room_for_modern_claude_thinking() {
        for model in [
            "claude-sonnet-5-5",
            "claude-sonnet-4-20250514",
            "claude-opus-4-1-20250805",
            "claude-haiku-4-5-20251001",
            "claude-fable-5",
            "claude-mythos-5-2",
        ] {
            let out =
                responses_to_anthropic_messages(&json!({"input": "hello"}), model, false).unwrap();
            assert_eq!(out["max_tokens"], 32_000, "{model}");
        }
        for model in ["claude-3-5-sonnet-20241022", "claude-2.1", "custom-model"] {
            let out =
                responses_to_anthropic_messages(&json!({"input": "hello"}), model, false).unwrap();
            assert_eq!(out["max_tokens"], 4096, "{model}");
        }
    }

    #[test]
    fn responses_payload_preserves_explicit_output_budgets() {
        for (limits, expected) in [
            (json!({"max_output_tokens": 64}), 64),
            (json!({"max_tokens": 128}), 128),
            (json!({"max_tokens": 128, "max_output_tokens": 256}), 128),
            (json!({"max_tokens": null, "max_output_tokens": 64}), 64),
            (
                json!({"max_tokens": "invalid", "max_output_tokens": 64}),
                64,
            ),
        ] {
            let mut payload = limits;
            payload["input"] = json!("hello");
            let out =
                responses_to_anthropic_messages(&payload, "claude-sonnet-5-5", false).unwrap();
            assert_eq!(out["max_tokens"], expected);
        }
    }

    #[test]
    fn responses_payload_maps_text_tools_and_tool_history() {
        let payload = json!({
            "model": "claude-sonnet-4-20250514",
            "instructions": "Be brief",
            "input": [
                {"role": "user", "content": [{"type": "input_text", "text": "lookup alpha"}]},
                {"type": "function_call", "call_id": "call_1", "name": "lookup", "arguments": "{\"q\":\"alpha\"}"},
                {"type": "function_call_output", "call_id": "call_1", "output": "alpha=42"}
            ],
            "tools": [{"type": "function", "name": "lookup", "parameters": {"type": "object"}}],
            "tool_choice": "required",
            "max_output_tokens": 100
        });
        let out =
            responses_to_anthropic_messages(&payload, "claude-sonnet-4-20250514", false).unwrap();
        assert_eq!(out["system"][0]["text"], "Be brief");
        assert_eq!(out["messages"].as_array().unwrap().len(), 3);
        assert_eq!(out["tools"][0]["name"], "lookup");
        assert_eq!(out["tool_choice"]["type"], "any");
    }

    #[test]
    fn responses_payload_maps_developer_messages_to_system_blocks() {
        let payload = json!({
            "model": "claude-sonnet-4-20250514",
            "messages": [
                {"role": "developer", "content": "keep answers concise"},
                {"role": "user", "content": "hello"}
            ]
        });
        let out =
            responses_to_anthropic_messages(&payload, "claude-sonnet-4-20250514", false).unwrap();
        assert_eq!(out["system"][0]["text"], "keep answers concise");
        assert_eq!(out["messages"][0]["role"], "user");
        assert_eq!(out["messages"][0]["content"][0]["text"], "hello");
    }

    #[test]
    fn responses_payload_maps_direct_tool_turns() {
        let payload = json!({
            "model": "claude-sonnet-4-20250514",
            "messages": [
                {"role": "user", "content": "lookup alpha"},
                {
                    "role": "assistant",
                    "content": "calling tool",
                    "tool_calls": [
                        {
                            "id": "call_1",
                            "type": "function",
                            "function": {
                                "name": "lookup",
                                "arguments": "{\"q\":\"alpha\"}"
                            }
                        }
                    ]
                },
                {"role": "tool", "tool_call_id": "call_1", "content": "alpha=42"}
            ]
        });
        let out =
            responses_to_anthropic_messages(&payload, "claude-sonnet-4-20250514", false).unwrap();
        assert_eq!(out["messages"][1]["role"], "assistant");
        assert_eq!(out["messages"][1]["content"][1]["type"], "tool_use");
        assert_eq!(out["messages"][1]["content"][1]["name"], "lookup");
        assert_eq!(out["messages"][2]["content"][0]["type"], "tool_result");
        assert_eq!(out["messages"][2]["content"][0]["content"], "alpha=42");
    }

    #[test]
    fn anthropic_tool_use_maps_to_responses_function_call() {
        let msg = json!({
            "id": "msg_1",
            "model": "claude-sonnet-4-20250514",
            "content": [{"type": "tool_use", "id": "toolu_1", "name": "trace", "input": {"ok": true}}],
            "usage": {"input_tokens": 5, "output_tokens": 2}
        });
        let out = anthropic_message_to_response(&msg, "claude-sonnet-4-20250514");
        assert_eq!(out["output"][0]["type"], "function_call");
        assert_eq!(out["output"][0]["arguments"], "{\"ok\":true}");
        assert_eq!(out["usage"]["total_tokens"], 7);
    }

    fn response_fixture(content: Value) -> Value {
        anthropic_message_to_response(
            &json!({
                "id": "msg_fixture", "model": "claude-fixture", "content": content,
                "stop_reason": "end_turn", "usage": {"input_tokens": 5, "output_tokens": 9}
            }),
            "claude-fixture",
        )
    }

    async fn buffered_sse_events(response: Value) -> Vec<Value> {
        let response = responses_sse(response);
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["content-type"], "text/event-stream");
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body = std::str::from_utf8(&body).unwrap();
        let events: Vec<Value> =
            body.split("\n\n")
                .filter(|frame| !frame.is_empty())
                .map(|frame| {
                    let value: Value =
                        serde_json::from_str(&parse_sse_data(frame.as_bytes()).unwrap()).unwrap();
                    assert!(
                        frame.starts_with(&format!("event: {}\n", value["type"].as_str().unwrap()))
                    );
                    value
                })
                .collect();
        for (index, event) in events.iter().enumerate() {
            assert_eq!(event["sequence_number"], index);
        }
        assert_eq!(events[0]["type"], "response.created");
        assert_eq!(events[1]["type"], "response.in_progress");
        assert_eq!(events[0]["response"]["status"], "in_progress");
        assert_eq!(events[0]["response"]["output"], json!([]));
        assert_eq!(events[0]["response"]["usage"], Value::Null);
        let final_output = events.last().unwrap()["response"]["output"]
            .as_array()
            .unwrap();
        for (index, item) in final_output.iter().enumerate() {
            let added: Vec<_> = events
                .iter()
                .enumerate()
                .filter(|(_, event)| {
                    event["type"] == "response.output_item.added" && event["output_index"] == index
                })
                .collect();
            let done: Vec<_> = events
                .iter()
                .enumerate()
                .filter(|(_, event)| {
                    event["type"] == "response.output_item.done" && event["output_index"] == index
                })
                .collect();
            assert_eq!(added.len(), 1);
            if item["type"] == "function_call"
                && matches!(item["status"].as_str(), Some("incomplete" | "in_progress"))
            {
                assert!(done.is_empty());
                continue;
            }
            assert_eq!(done.len(), 1);
            assert!(added[0].0 < done[0].0);
            assert!(done[0].0 < events.len() - 1);
            assert_eq!(added[0].1["item"]["id"], item["id"]);
            assert_eq!(added[0].1["item"]["status"], "in_progress");
            assert_eq!(&done[0].1["item"], item);
            for (event_index, event) in events.iter().enumerate().filter(|(_, event)| {
                event.get("item_id").is_some() && event["output_index"] == index
            }) {
                assert_eq!(event["item_id"], item["id"]);
                assert!(added[0].0 < event_index && event_index < done[0].0);
            }
        }
        events
    }

    #[test]
    fn anthropic_response_preserves_block_order_and_distinct_tool_ids() {
        let out = response_fixture(json!([
            {"type": "thinking", "thinking": "Plan"},
            {"type": "text", "text": "Hello"},
            {"type": "text", "text": " world"},
            {"type": "tool_use", "id": "toolu_a", "name": "first", "input": {}},
            {"type": "tool_use", "id": "toolu_b", "name": "second", "input": {"q": "café"}},
            {"type": "text", "text": "After tools"}
        ]));
        let items = out["output"].as_array().unwrap();
        assert_eq!(
            items
                .iter()
                .map(|item| item["type"].as_str().unwrap())
                .collect::<Vec<_>>(),
            [
                "reasoning",
                "message",
                "function_call",
                "function_call",
                "message"
            ]
        );
        assert_eq!(items[1]["content"].as_array().unwrap().len(), 2);
        assert_eq!(items[2]["call_id"], "toolu_a");
        assert_eq!(items[3]["call_id"], "toolu_b");
        let ids: std::collections::HashSet<_> = items
            .iter()
            .map(|item| item["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids.len(), items.len());
        assert!(ids.iter().all(|id| !id.is_empty()));
    }

    #[tokio::test]
    async fn responses_sse_finishes_multipart_text_without_duplicate_deltas() {
        let response = response_fixture(json!([
            {"type": "text", "text": "Hello\n"}, {"type": "text", "text": "café 🐈"}
        ]));
        let events = buffered_sse_events(response.clone()).await;
        assert_eq!(events.last().unwrap()["type"], "response.completed");
        assert_eq!(events.last().unwrap()["response"], response);
        let deltas: Vec<_> = events
            .iter()
            .filter(|e| e["type"] == "response.output_text.delta")
            .collect();
        assert_eq!(
            deltas
                .iter()
                .map(|e| e["delta"].as_str().unwrap())
                .collect::<String>(),
            "Hello\ncafé 🐈"
        );
        assert_eq!(deltas[0]["content_index"], 0);
        assert_eq!(deltas[1]["content_index"], 1);
        for event in events
            .iter()
            .filter(|e| e["type"] == "response.content_part.added")
        {
            assert_eq!(event["part"]["text"], "");
        }
        let done: Vec<_> = events
            .iter()
            .filter(|e| e["type"] == "response.content_part.done")
            .collect();
        assert_eq!(done.len(), 2);
        assert_eq!(done[1]["part"]["text"], "café 🐈");
    }

    #[tokio::test]
    async fn responses_sse_finishes_reasoning_and_parallel_tool_arguments() {
        let response = response_fixture(json!([
            {"type": "thinking", "thinking": "Check two values"},
            {"type": "text", "text": "Checking"},
            {"type": "tool_use", "id": "toolu_a", "name": "lookup", "input": {}},
            {"type": "tool_use", "id": "toolu_b", "name": "lookup", "input": {"q": "café"}}
        ]));
        let events = buffered_sse_events(response.clone()).await;
        assert_eq!(events.last().unwrap()["response"], response);
        let indices: Vec<_> = events
            .iter()
            .filter(|e| e["type"] == "response.output_item.done")
            .map(|e| e["output_index"].as_u64().unwrap())
            .collect();
        assert_eq!(indices, [0, 1, 2, 3]);
        assert!(events
            .iter()
            .any(|e| e["type"] == "response.reasoning_summary_text.done"
                && e["text"] == "Check two values"));
        for index in [2, 3] {
            let delta = events
                .iter()
                .find(|e| {
                    e["type"] == "response.function_call_arguments.delta"
                        && e["output_index"] == index
                })
                .unwrap();
            let done = events
                .iter()
                .find(|e| {
                    e["type"] == "response.function_call_arguments.done"
                        && e["output_index"] == index
                })
                .unwrap();
            assert_eq!(delta["delta"], response["output"][index]["arguments"]);
            assert_eq!(done["arguments"], delta["delta"]);
            let added = events
                .iter()
                .find(|e| e["type"] == "response.output_item.added" && e["output_index"] == index)
                .unwrap();
            assert_eq!(added["item"]["arguments"], "");
        }
    }

    #[tokio::test]
    async fn responses_sse_reports_max_tokens_as_incomplete() {
        let response = anthropic_message_to_response(
            &json!({
                "id": "msg_truncated", "content": [
                    {"type": "thinking", "thinking": "Plan"},
                    {"type": "text", "text": "Partial answer"}
                ],
                "stop_reason": "max_tokens", "usage": {"input_tokens": 5, "output_tokens": 100}
            }),
            "claude-fixture",
        );
        assert_eq!(response["output"][0]["status"], "completed");
        assert_eq!(response["output"][1]["status"], "incomplete");
        let events = buffered_sse_events(response.clone()).await;
        assert_eq!(events.last().unwrap()["type"], "response.incomplete");
        assert_eq!(events.last().unwrap()["response"], response);
        assert_eq!(
            response["incomplete_details"]["reason"],
            "max_output_tokens"
        );
        assert!(!events.iter().any(|e| e["type"] == "response.completed"));
    }

    #[tokio::test]
    async fn responses_sse_never_finalizes_a_truncated_tool_as_executable() {
        let response = anthropic_message_to_response(
            &json!({
                "id": "msg_truncated_tool", "stop_reason": "max_tokens",
                "content": [{"type": "tool_use", "id": "toolu_partial", "name": "lookup", "input": {}}]
            }),
            "claude-fixture",
        );
        let events = buffered_sse_events(response.clone()).await;
        assert_eq!(events.last().unwrap()["type"], "response.incomplete");
        assert_eq!(events.last().unwrap()["response"], response);
        assert_eq!(response["output"][0]["call_id"], "toolu_partial");
        assert_eq!(response["output"][0]["status"], "incomplete");
        assert!(!events
            .iter()
            .any(|e| e["type"] == "response.function_call_arguments.done"
                || e["type"] == "response.output_item.done"));
    }

    #[tokio::test]
    async fn responses_sse_finishes_only_completed_tools_in_a_truncated_batch() {
        let response = anthropic_message_to_response(
            &json!({
                "id": "msg_mixed", "stop_reason": "max_tokens",
                "content": [
                    {"type": "tool_use", "id": "toolu_complete", "name": "lookup", "input": {"q": "complete"}},
                    {"type": "tool_use", "id": "toolu_partial", "name": "lookup", "input": {"q": "partial"}}
                ]
            }),
            "claude-sonnet-5-5",
        );
        let events = buffered_sse_events(response.clone()).await;
        let done: Vec<_> = events
            .iter()
            .filter(|e| e["type"] == "response.output_item.done")
            .collect();
        assert_eq!(done.len(), 1);
        assert_eq!(done[0]["item"]["call_id"], "toolu_complete");
        assert_eq!(response["output"][1]["status"], "incomplete");
        assert!(!events.iter().any(
            |e| e["type"] == "response.function_call_arguments.done" && e["output_index"] == 1
        ));
        assert_eq!(events.last().unwrap()["type"], "response.incomplete");
    }

    #[tokio::test]
    async fn responses_sse_does_not_finalize_a_compatible_tool_still_in_progress() {
        let response = json!({
            "id": "resp_partial_compatible", "status": "incomplete",
            "incomplete_details": {"reason": "max_output_tokens"},
            "output": [{"type": "function_call", "call_id": "call_partial", "name": "lookup",
                "arguments": "{\"q\":\"valid JSON is not proof of completion\"}", "status": "in_progress"}]
        });
        let events = buffered_sse_events(response).await;
        assert!(!events
            .iter()
            .any(|e| e["type"] == "response.function_call_arguments.done"
                || e["type"] == "response.output_item.done"));
    }

    #[tokio::test]
    async fn responses_sse_assigns_stable_ids_and_finishes_empty_parts() {
        let events = buffered_sse_events(json!({
            "id": "resp_compatible", "status": "completed", "output": [
                {"type": "message", "role": "assistant", "status": "completed", "content": [
                    {"type": "output_text", "text": "", "annotations": []},
                    {"type": "refusal", "refusal": "Unavailable"}
                ]},
                {"type": "function_call", "call_id": "call_empty", "name": "test", "arguments": "", "status": "completed"}
            ]
        })).await;
        assert!(events
            .iter()
            .any(|e| e["type"] == "response.output_text.done" && e["text"] == ""));
        assert!(events
            .iter()
            .any(|e| e["type"] == "response.refusal.done" && e["refusal"] == "Unavailable"));
        assert!(events
            .iter()
            .any(|e| e["type"] == "response.function_call_arguments.done" && e["arguments"] == ""));
        let items = events.last().unwrap()["response"]["output"]
            .as_array()
            .unwrap();
        assert!(items
            .iter()
            .all(|item| item["id"].as_str().is_some_and(|id| !id.is_empty())));
        assert_ne!(items[0]["id"], items[1]["id"]);
    }

    #[test]
    fn prepare_anthropic_body_injects_claude_code_system_markers() {
        let body = Bytes::from(
            serde_json::to_vec(&json!({
                "model": "claude-fable-5",
                "messages": [{"role": "user", "content": "Reply with OK only."}]
            }))
            .unwrap(),
        );
        let (prepared, beta) = prepare_anthropic_body(body);
        let out: Value = serde_json::from_slice(&prepared).unwrap();

        assert_eq!(beta, None);
        assert_eq!(out["system"][0]["text"], CLAUDE_CODE_ATTRIBUTION_MARKER);
        assert_eq!(out["system"][1]["text"], CLAUDE_CODE_AGENT_PROMPT);
    }

    #[test]
    fn prepare_anthropic_body_preserves_existing_system_content() {
        let body = Bytes::from(
            serde_json::to_vec(&json!({
                "model": "claude-fable-5",
                "messages": [{"role": "user", "content": "Reply with OK only."}],
                "system": [{"type": "text", "text": "Keep answers concise."}],
                "betas": ["fine-grained-tool-streaming-2025-05-14"]
            }))
            .unwrap(),
        );
        let (prepared, beta) = prepare_anthropic_body(body);
        let out: Value = serde_json::from_slice(&prepared).unwrap();

        assert_eq!(
            beta.as_deref(),
            Some("fine-grained-tool-streaming-2025-05-14")
        );
        assert_eq!(out["system"][0]["text"], CLAUDE_CODE_ATTRIBUTION_MARKER);
        assert_eq!(out["system"][1]["text"], CLAUDE_CODE_AGENT_PROMPT);
        assert_eq!(out["system"][2]["text"], "Keep answers concise.");
        assert!(out.get("betas").is_none());
    }

    #[test]
    fn policy_metrics_include_translated_and_injected_system_content() {
        let native = Bytes::from(
            serde_json::to_vec(&json!({
                "model": "claude-fable-5",
                "messages": [{"role": "user", "content": "hi"}]
            }))
            .unwrap(),
        );
        let native_raw: Value = serde_json::from_slice(&native).unwrap();
        let native_metrics = policy_prompt_metrics("anthropic/v1/messages", &native).unwrap();
        assert!(
            native_metrics.estimated_input_tokens()
                > crate::prompt_metrics_from_request_value(&native_raw).estimated_input_tokens()
        );

        let responses = Bytes::from(
            serde_json::to_vec(&json!({
                "model": "claude-fable-5",
                "input": "hi"
            }))
            .unwrap(),
        );
        let responses_raw: Value = serde_json::from_slice(&responses).unwrap();
        let responses_metrics = policy_prompt_metrics("responses", &responses).unwrap();
        assert!(
            responses_metrics.estimated_input_tokens()
                > crate::prompt_metrics_from_request_value(&responses_raw).estimated_input_tokens()
        );
    }

    #[test]
    fn anthropic_sse_tracker_fails_on_error_event() {
        let mut tracker = AnthropicSseUsageTracker::default();
        tracker.push(&Bytes::from_static(
            b"event: error\ndata: {\"error\":{\"message\":\"quota exhausted\"}}\n\n",
        ));
        assert!(matches!(
            tracker.finish(),
            Err(message) if message == "quota exhausted"
        ));
    }
}
