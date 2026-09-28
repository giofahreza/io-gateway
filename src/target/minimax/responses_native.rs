//! Native OpenAI Responses API pass-through to MiniMax.
//!
//! MiniMax's Codex setup documents `base_url = "https://api.minimax.io/v1"`
//! with `wire_api = "responses"`, so the gateway forwards Codex Responses
//! bodies to `POST /v1/responses` without translating them to chat
//! completions.

use axum::{
    body::Body,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
};
use bytes::Bytes;
use futures_util::StreamExt;
use serde_json::Value;
use std::time::Duration;

use super::accounts::MiniMaxAccount;
use super::api::{
    build_chat_completions_payload, chat_completion_to_responses, chat_completions_url,
    stream_chat_completions,
};
use super::DEFAULT_BASE_URL;

const RESPONSES_PATH: &str = "/v1/responses";

/// Normalize the account's configured base URL to a bare host (no trailing
/// slash).
pub fn normalize_base_url(base_url: Option<&str>) -> String {
    base_url
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(DEFAULT_BASE_URL)
        .trim_end_matches('/')
        .to_string()
}

fn responses_url(base_url: &str) -> String {
    let base = normalize_base_url(Some(base_url));
    if base.ends_with(RESPONSES_PATH) {
        return base;
    }
    if base.ends_with("/responses") {
        return base;
    }
    if base.ends_with("/v1") {
        return format!("{}/responses", base);
    }
    format!("{}{}", base, RESPONSES_PATH)
}

pub fn use_native_responses(account_base_url: Option<&str>) -> bool {
    let normalized = normalize_base_url(account_base_url);
    !normalized.ends_with("/v1/chat/completions") && !normalized.ends_with("/chat/completions")
}

pub async fn responses(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    if !crate::check_api_key(&state, &headers) {
        crate::release_api_key_budget_before_dispatch(&state);
        return (
            StatusCode::UNAUTHORIZED,
            [("Content-Type", "application/json")],
            crate::source::v1::response::openai_error_body(
                "Invalid proxy API key",
                "authentication_error",
                Some("invalid_api_key"),
            ),
        )
            .into_response();
    }

    let raw: Value = match serde_json::from_slice(&body) {
        Ok(value) => value,
        Err(_) => {
            crate::release_api_key_budget_before_dispatch(&state);
            return (
                StatusCode::BAD_REQUEST,
                [("Content-Type", "application/json")],
                crate::source::v1::response::openai_error_body(
                    "Invalid request body",
                    "invalid_request_error",
                    None,
                ),
            )
                .into_response();
        }
    };

    let model = match raw.get("model").and_then(|v| v.as_str()) {
        Some(model) if !model.trim().is_empty() => model.to_string(),
        _ => {
            crate::release_api_key_budget_before_dispatch(&state);
            return (
                StatusCode::BAD_REQUEST,
                [("Content-Type", "application/json")],
                crate::source::v1::response::openai_error_body(
                    "model is required",
                    "invalid_request_error",
                    None,
                ),
            )
                .into_response();
        }
    };

    let accounts = super::accounts::candidate_accounts(&state);
    if accounts.is_empty() {
        crate::release_api_key_budget_before_dispatch(&state);
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            [("Content-Type", "application/json")],
            crate::source::v1::response::openai_error_body(
                "No MiniMax accounts configured",
                "server_error",
                None,
            ),
        )
            .into_response();
    }

    let mut last_error: Option<(StatusCode, String)> = None;
    for (attempt_idx, account) in accounts.iter().enumerate() {
        let base_url = normalize_base_url(account.base_url.as_deref());
        let response = if use_native_responses(Some(&base_url)) {
            native_responses(&state, account, &base_url, &model, &raw, &headers, &body).await
        } else {
            chat_completions_responses(&state, account, &base_url, &model, &raw, &headers, &body)
                .await
        };

        let status = response.status();
        if status.is_success() {
            return response;
        }

        let headers = response.headers().clone();
        let body_bytes = match axum::body::to_bytes(response.into_body(), usize::MAX).await {
            Ok(bytes) => bytes,
            Err(err) => {
                let message = format!("MiniMax error body read failed: {}", err);
                if attempt_idx + 1 < accounts.len() {
                    last_error = Some((StatusCode::BAD_GATEWAY, message));
                    continue;
                }
                return (
                    StatusCode::BAD_GATEWAY,
                    [("Content-Type", "application/json")],
                    crate::source::v1::response::openai_error_body(&message, "server_error", None),
                )
                    .into_response();
            }
        };
        let message = String::from_utf8_lossy(&body_bytes).to_string();
        if attempt_idx + 1 < accounts.len() && crate::should_retry_account_error(status, &message) {
            last_error = Some((status, message));
            continue;
        }
        return (status, headers, body_bytes).into_response();
    }

    let (status, message) = last_error.unwrap_or_else(|| {
        (
            StatusCode::BAD_GATEWAY,
            "All MiniMax accounts failed".to_string(),
        )
    });
    (
        status,
        [("Content-Type", "application/json")],
        crate::source::v1::response::openai_error_body(
            &format!("All MiniMax accounts failed; last error: {}", message),
            "server_error",
            None,
        ),
    )
        .into_response()
}

async fn native_responses(
    state: &crate::AppState,
    account: &MiniMaxAccount,
    base_url: &str,
    model: &str,
    raw: &Value,
    incoming_headers: &HeaderMap,
    body: &Bytes,
) -> axum::response::Response {
    let context = crate::minimax_usage_context(
        account,
        Some(model.to_string()),
        "/minimax/v1/responses",
        crate::prompt_metrics_from_request_value(raw),
    );
    if let Err(response) =
        crate::api_key_quota_runtime::reserve_api_key_budgets_for_prepared_dispatch_with_protocol(
            state,
            context.provider_name,
            &context.key,
            body,
            crate::quota_usage::PreparedProtocol::OpenAiResponses,
        )
        .await
    {
        return response;
    }
    crate::record_minimax_request(state, &context);

    let wants_stream = crate::source::wants_stream(incoming_headers, body);
    let url = responses_url(base_url);
    let mut request = state
        .client
        .post(&url)
        .timeout(Duration::from_secs(180))
        .body(body.clone());

    for (key, value) in incoming_headers.iter() {
        if crate::should_drop_incoming_header(key.as_str())
            || key.as_str().eq_ignore_ascii_case("x-api-key")
        {
            continue;
        }
        request = request.header(key, value);
    }

    request = request
        .header(
            "Authorization",
            format!("Bearer {}", account.api_key.trim()),
        )
        .header("Content-Type", "application/json")
        .header(
            "Accept",
            if wants_stream {
                "text/event-stream"
            } else {
                "application/json"
            },
        );

    if !incoming_headers.contains_key("accept-encoding") {
        request = request.header("Accept-Encoding", "identity");
    }
    if !incoming_headers.contains_key("user-agent") {
        request = request.header("User-Agent", "io-gateway/1.0");
    }

    let resp = match request.send().await {
        Ok(resp) => resp,
        Err(err) => {
            let message = format!("MiniMax responses request failed: {}", err);
            crate::record_minimax_error(state, &context, &message);
            return (
                StatusCode::BAD_GATEWAY,
                [("Content-Type", "application/json")],
                crate::source::v1::response::openai_error_body(&message, "server_error", None),
            )
                .into_response();
        }
    };

    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        let message = format!("MiniMax returned {}: {}", status, text);
        crate::record_minimax_error(state, &context, &message);
        return (status, [("Content-Type", "application/json")], text).into_response();
    }

    if wants_stream {
        return stream_native_responses(state, &context, resp).await;
    }

    let text = match resp.text().await {
        Ok(text) => text,
        Err(err) => {
            let message = format!("MiniMax body read failed: {}", err);
            crate::record_minimax_error(state, &context, &message);
            return (
                StatusCode::BAD_GATEWAY,
                [("Content-Type", "application/json")],
                crate::source::v1::response::openai_error_body(&message, "server_error", None),
            )
                .into_response();
        }
    };

    let value: Value = match serde_json::from_str(&text) {
        Ok(value) => value,
        Err(err) => {
            let message = format!("invalid MiniMax response: {}", err);
            crate::record_minimax_error(state, &context, &message);
            return (
                StatusCode::BAD_GATEWAY,
                [("Content-Type", "application/json")],
                crate::source::v1::response::openai_error_body(&message, "server_error", None),
            )
                .into_response();
        }
    };

    let usage = crate::usage_metrics_from_response_value(&value);
    crate::record_minimax_success(state, &context, &usage);

    (
        StatusCode::OK,
        [("Content-Type", "application/json")],
        Bytes::from(text),
    )
        .into_response()
}

async fn stream_native_responses(
    state: &crate::AppState,
    context: &crate::UsageContext,
    resp: reqwest::Response,
) -> axum::response::Response {
    let usage_state = state.clone();
    let usage_context = context.clone();
    let lifecycle = crate::StreamRequestGuard::new(&usage_state, &usage_context);
    let stream = async_stream::stream! {
        let mut lifecycle = lifecycle;
        let mut upstream = resp.bytes_stream();
        let mut parser = NativeResponsesSseUsageTracker::default();
        while let Some(chunk) = upstream.next().await {
            match chunk {
                Ok(bytes) => {
                    parser.push(&bytes);
                    if let Some(message) = parser.terminal_error() {
                        crate::record_minimax_error(&usage_state, &usage_context, message);
                        lifecycle.finish();
                        yield Ok::<Bytes, std::io::Error>(bytes);
                        return;
                    }
                    yield Ok::<Bytes, std::io::Error>(bytes);
                }
                Err(err) => {
                    let message = format!("MiniMax stream read failed: {}", err);
                    crate::record_minimax_error(&usage_state, &usage_context, &message);
                    lifecycle.finish();
                    yield Err(std::io::Error::new(std::io::ErrorKind::Other, "stream"));
                    return;
                }
            }
        }
        match parser.finish() {
            Err(message) => crate::record_minimax_error(&usage_state, &usage_context, &message),
            Ok(Some(response)) => {
                let usage = crate::usage_metrics_from_response_value(&response);
                crate::record_minimax_success(&usage_state, &usage_context, &usage);
            }
            Ok(None) => crate::record_minimax_success(
                &usage_state,
                &usage_context,
                &crate::UsageMetrics::default(),
            ),
        }
        lifecycle.finish();
    };

    (
        StatusCode::OK,
        [
            ("Content-Type", "text/event-stream"),
            ("Cache-Control", "no-store"),
        ],
        Body::from_stream(stream),
    )
        .into_response()
}

#[derive(Default)]
struct NativeResponsesSseUsageTracker {
    buffer: Vec<u8>,
    last_response: Option<Value>,
    terminal_error: Option<String>,
}

impl NativeResponsesSseUsageTracker {
    fn push(&mut self, bytes: &Bytes) {
        self.buffer.extend_from_slice(bytes);
        while let Some((event_end, delimiter_len)) = find_sse_boundary(&self.buffer) {
            let raw_event: Vec<u8> = self.buffer.drain(..event_end + delimiter_len).collect();
            self.absorb_event(&raw_event[..event_end]);
        }
    }

    fn finish(mut self) -> Result<Option<Value>, String> {
        if !self.buffer.is_empty() {
            let raw = std::mem::take(&mut self.buffer);
            self.absorb_event(&raw);
        }
        self.terminal_error.map_or(Ok(self.last_response), Err)
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
        let Some(data) = parse_sse_data(raw_event) else {
            return;
        };
        if data.trim() == "[DONE]" {
            return;
        }
        let Ok(value) = serde_json::from_str::<Value>(&data) else {
            return;
        };
        if value.get("type").and_then(|v| v.as_str()) == Some("response.completed") {
            if let Some(response) = value.get("response").cloned() {
                self.last_response = Some(response);
            }
        }
    }
}

fn find_sse_boundary(buffer: &[u8]) -> Option<(usize, usize)> {
    buffer
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|idx| (idx, 4))
        .or_else(|| {
            buffer
                .windows(2)
                .position(|window| window == b"\n\n")
                .map(|idx| (idx, 2))
        })
}

fn parse_sse_data(raw_event: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(raw_event);
    let mut data_lines: Vec<String> = Vec::new();
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

/// Chat-Completions fallback path. Used when the configured account's
/// `base_url` explicitly points at the chat-completions endpoint.
async fn chat_completions_responses(
    state: &crate::AppState,
    account: &MiniMaxAccount,
    base_url: &str,
    model: &str,
    raw: &Value,
    headers: &HeaderMap,
    body: &Bytes,
) -> axum::response::Response {
    let context = crate::minimax_usage_context(
        account,
        Some(model.to_string()),
        "/minimax/v1/chat/completions",
        crate::prompt_metrics_from_request_value(raw),
    );
    let wants_stream = crate::source::wants_stream(headers, body);

    let mut chat_payload = match build_chat_completions_payload(raw, model) {
        Ok(payload) => payload,
        Err(err) => {
            crate::record_request_pre_dispatch_error(state, &context, err.as_str());
            return (
                StatusCode::BAD_REQUEST,
                [("Content-Type", "application/json")],
                crate::source::v1::response::openai_error_body(&err, "invalid_request_error", None),
            )
                .into_response();
        }
    };

    if wants_stream {
        chat_payload["stream"] = serde_json::json!(true);
        if chat_payload.get("stream_options").is_none() {
            chat_payload["stream_options"] = serde_json::json!({"include_usage": true});
        }
    }
    if let Err(response) =
        crate::api_key_quota_runtime::reserve_api_key_budgets_for_prepared_dispatch_with_protocol(
            state,
            context.provider_name,
            &context.key,
            &serde_json::to_vec(&chat_payload).unwrap_or_default(),
            crate::quota_usage::PreparedProtocol::ChatCompletions,
        )
        .await
    {
        return response;
    }
    crate::record_minimax_request(state, &context);

    if wants_stream {
        return match stream_chat_completions(
            state,
            &account.api_key,
            base_url,
            &chat_payload,
            &context,
            model,
            headers,
        )
        .await
        {
            Ok(response) => response,
            Err((status, message)) => {
                crate::record_minimax_error(state, &context, &message);
                (
                    status,
                    [("Content-Type", "application/json")],
                    crate::source::v1::response::openai_error_body(
                        &message,
                        if status.is_client_error() {
                            "invalid_request_error"
                        } else {
                            "server_error"
                        },
                        None,
                    ),
                )
                    .into_response()
            }
        };
    }

    let resp = match state
        .client
        .post(chat_completions_url(base_url))
        .header(
            "Authorization",
            format!("Bearer {}", account.api_key.trim()),
        )
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .body(chat_payload.to_string())
        .timeout(Duration::from_secs(180))
        .send()
        .await
    {
        Ok(resp) => resp,
        Err(err) => {
            let message = format!("MiniMax request failed: {}", err);
            crate::record_minimax_error(state, &context, &message);
            return (
                StatusCode::BAD_GATEWAY,
                [("Content-Type", "application/json")],
                crate::source::v1::response::openai_error_body(&message, "server_error", None),
            )
                .into_response();
        }
    };

    let status = resp.status();
    let text = match resp.text().await {
        Ok(text) => text,
        Err(err) => {
            let message = format!("MiniMax body read failed: {}", err);
            crate::record_minimax_error(state, &context, &message);
            return (
                StatusCode::BAD_GATEWAY,
                [("Content-Type", "application/json")],
                crate::source::v1::response::openai_error_body(&message, "server_error", None),
            )
                .into_response();
        }
    };

    if !status.is_success() {
        crate::record_minimax_error(
            state,
            &context,
            format!("MiniMax returned {}: {}", status, text),
        );
        return (
            StatusCode::BAD_GATEWAY,
            [("Content-Type", "application/json")],
            crate::source::v1::response::openai_error_body(
                &format!("MiniMax returned {}: {}", status, text),
                "server_error",
                None,
            ),
        )
            .into_response();
    }

    let chat_response: Value = match serde_json::from_str(&text) {
        Ok(value) => value,
        Err(err) => {
            let message = format!("invalid MiniMax response: {}", err);
            crate::record_minimax_error(state, &context, &message);
            return (
                StatusCode::BAD_GATEWAY,
                [("Content-Type", "application/json")],
                crate::source::v1::response::openai_error_body(&message, "server_error", None),
            )
                .into_response();
        }
    };

    let response = chat_completion_to_responses(&chat_response, model);
    let mut usage = crate::usage_metrics_from_response_value(&response);
    crate::quota_usage::preserve_native_usage(&mut usage, &chat_response, "openai");
    crate::record_minimax_success(state, &context, &usage);

    let body = serde_json::to_vec(&response).unwrap_or_default();
    (StatusCode::OK, [("Content-Type", "application/json")], body).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_base_url_defaults_to_official() {
        assert_eq!(normalize_base_url(None), DEFAULT_BASE_URL);
        assert_eq!(
            normalize_base_url(Some("https://example.com/")),
            "https://example.com"
        );
    }

    #[test]
    fn responses_url_handles_known_shapes() {
        assert_eq!(
            responses_url("https://api.minimax.io"),
            "https://api.minimax.io/v1/responses"
        );
        assert_eq!(
            responses_url("https://api.minimax.io/v1"),
            "https://api.minimax.io/v1/responses"
        );
        assert_eq!(
            responses_url("https://example.com/v1/responses"),
            "https://example.com/v1/responses"
        );
    }

    #[test]
    fn use_native_responses_by_default_unless_chat_completions_is_explicit() {
        assert!(use_native_responses(None));
        assert!(use_native_responses(Some("https://api.minimax.io")));
        assert!(use_native_responses(Some("https://api.minimax.io/v1")));
        assert!(use_native_responses(Some(
            "https://example.com/v1/responses"
        )));
        assert!(!use_native_responses(Some(
            "https://example.com/v1/chat/completions"
        )));
    }

    #[test]
    fn native_sse_tracker_rejects_terminal_error_events() {
        let mut tracker = NativeResponsesSseUsageTracker::default();
        tracker.push(&Bytes::from_static(
            b"event: error\ndata: {\"error\":{\"message\":\"minimax unavailable\"}}\n\n",
        ));
        assert!(matches!(
            tracker.finish(),
            Err(message) if message == "minimax unavailable"
        ));
    }
}
