use axum::{
    body::Body,
    extract::State,
    http::{HeaderMap, HeaderValue, StatusCode},
    response::IntoResponse,
};
use bytes::Bytes;
use futures_util::StreamExt;
use serde_json::Value;
use std::time::Duration;

use super::DEFAULT_BASE_URL;

const DEFAULT_ANTHROPIC_VERSION: &str = "2023-06-01";

pub fn anthropic_messages_url(base_url: &str) -> String {
    let base = upstream_root(base_url);
    if base.ends_with("/anthropic/v1/messages")
        || base.ends_with("/v1/messages")
        || base.ends_with("/messages")
    {
        return base;
    }
    if base.ends_with("/anthropic/v1") {
        return format!("{}/messages", base);
    }
    if base.ends_with("/anthropic") {
        return format!("{}/v1/messages", base);
    }
    format!("{}/anthropic/v1/messages", base)
}

fn upstream_root(base_url: &str) -> String {
    let mut base = super::api::normalize_base_url(Some(base_url));
    for suffix in [
        "/v1/chat/completions",
        "/chat/completions",
        "/v1/responses",
        "/responses",
        "/v1",
    ] {
        if let Some(stripped) = base.strip_suffix(suffix) {
            base = stripped.trim_end_matches('/').to_string();
            break;
        }
    }
    if base.is_empty() {
        DEFAULT_BASE_URL.to_string()
    } else {
        base
    }
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

    let raw: Value = match serde_json::from_slice(&body) {
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

    let model = match raw.get("model").and_then(|v| v.as_str()) {
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
    if accounts.is_empty() {
        crate::release_api_key_budget_before_dispatch(&state);
        return anthropic_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "api_error",
            "No MiniMax accounts configured",
        );
    }

    let wants_stream = crate::source::wants_stream(&headers, &body);
    let prompt_metrics = crate::prompt_metrics_from_request_value(&raw);
    let mut last_error: Option<(StatusCode, String)> = None;

    for (attempt_idx, account) in accounts.iter().enumerate() {
        let context = crate::minimax_usage_context(
            account,
            Some(model.clone()),
            "/minimax/anthropic/v1/messages",
            prompt_metrics.clone(),
        );
        if let Err(response) = crate::api_key_quota_runtime::reserve_api_key_budgets_for_prepared_dispatch_with_protocol(
            &state, context.provider_name, &context.key, &body,
            crate::quota_usage::PreparedProtocol::AnthropicMessages,
        ).await {
            return response;
        }
        crate::record_minimax_request(&state, &context);

        let base_url = super::api::normalize_base_url(account.base_url.as_deref());
        let url = anthropic_messages_url(&base_url);
        let mut request = state
            .client
            .post(url)
            .body(body.clone())
            .timeout(Duration::from_secs(180));

        for (key, value) in headers.iter() {
            if should_drop_anthropic_incoming_header(key.as_str()) {
                continue;
            }
            request = request.header(key, value);
        }

        request = request
            .header(
                "Authorization",
                format!("Bearer {}", account.api_key.trim()),
            )
            .header("x-api-key", account.api_key.trim())
            .header("Content-Type", "application/json")
            .header(
                "Accept",
                if wants_stream {
                    "text/event-stream"
                } else {
                    "application/json"
                },
            );
        if !headers.contains_key("anthropic-version") {
            request = request.header("anthropic-version", DEFAULT_ANTHROPIC_VERSION);
        }

        let resp = match request.send().await {
            Ok(resp) => resp,
            Err(err) => {
                let message = format!("MiniMax Anthropic request failed: {}", err);
                crate::record_minimax_error(&state, &context, &message);
                last_error = Some((StatusCode::BAD_GATEWAY, message));
                if attempt_idx + 1 < accounts.len() {
                    continue;
                }
                break;
            }
        };

        let status = resp.status();
        let out_headers = response_headers(
            resp.headers(),
            if wants_stream {
                "text/event-stream"
            } else {
                "application/json"
            },
        );

        if wants_stream && status.is_success() {
            return stream_messages(state, context, resp, out_headers).await;
        }

        let bytes = match resp.bytes().await {
            Ok(bytes) => bytes,
            Err(err) => {
                let message = format!("MiniMax Anthropic body read failed: {}", err);
                crate::record_minimax_error(&state, &context, &message);
                last_error = Some((StatusCode::BAD_GATEWAY, message));
                if attempt_idx + 1 < accounts.len() {
                    continue;
                }
                break;
            }
        };

        if !status.is_success() {
            let message = format!(
                "MiniMax Anthropic returned {}: {}",
                status,
                String::from_utf8_lossy(&bytes)
            );
            crate::record_minimax_error(&state, &context, &message);
            if attempt_idx + 1 < accounts.len()
                && crate::should_retry_account_error(status, &message)
            {
                last_error = Some((status, message));
                continue;
            }
            return (status, out_headers, bytes).into_response();
        }

        let usage = serde_json::from_slice::<Value>(&bytes)
            .map(|value| {
                let mut usage = crate::usage_metrics_from_response_value(&value);
                crate::quota_usage::preserve_native_usage(&mut usage, &value, "anthropic");
                usage
            })
            .unwrap_or_default();
        crate::record_minimax_success(&state, &context, &usage);
        return (status, out_headers, bytes).into_response();
    }

    let (status, message) = last_error.unwrap_or_else(|| {
        (
            StatusCode::BAD_GATEWAY,
            "All MiniMax accounts failed".to_string(),
        )
    });
    anthropic_error(
        status,
        "api_error",
        &format!("All MiniMax accounts failed; last error: {}", message),
    )
}

async fn stream_messages(
    state: crate::AppState,
    context: crate::UsageContext,
    resp: reqwest::Response,
    headers: HeaderMap,
) -> axum::response::Response {
    let usage_state = state.clone();
    let usage_context = context.clone();
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
                        crate::record_minimax_error(&usage_state, &usage_context, message);
                        lifecycle.finish();
                        yield Ok::<Bytes, std::io::Error>(bytes);
                        return;
                    }
                    yield Ok::<Bytes, std::io::Error>(bytes);
                }
                Err(err) => {
                    let message = format!("MiniMax Anthropic stream read failed: {}", err);
                    crate::record_minimax_error(&usage_state, &usage_context, &message);
                    lifecycle.finish();
                    yield Err(std::io::Error::new(std::io::ErrorKind::Other, "stream"));
                    return;
                }
            }
        }
        match parser.finish() {
            Err(message) => crate::record_minimax_error(&usage_state, &usage_context, &message),
            Ok(Some(usage)) => crate::record_minimax_success(&usage_state, &usage_context, &usage),
            Ok(None) => crate::record_minimax_success(
                &usage_state,
                &usage_context,
                &crate::UsageMetrics::default(),
            ),
        }
        lifecycle.finish();
    };

    (StatusCode::OK, headers, Body::from_stream(stream)).into_response()
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
    let body = serde_json::to_vec(&serde_json::json!({
        "type": "error",
        "error": {
            "type": error_type,
            "message": message
        }
    }))
    .unwrap_or_default();
    (status, [("Content-Type", "application/json")], body).into_response()
}

#[derive(Default)]
struct AnthropicSseUsageTracker {
    buffer: Vec<u8>,
    last_usage: Option<crate::UsageMetrics>,
    terminal_error: Option<String>,
    saw_message_stop: bool,
    saw_final_output_usage: bool,
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
        if let Some(usage) = self.last_usage.as_mut() {
            crate::quota_usage::mark_stream_usage(
                usage,
                "anthropic",
                self.saw_message_stop && self.saw_final_output_usage,
            );
        }
        self.terminal_error.map_or(Ok(self.last_usage), Err)
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
        if let Some(raw) = usage.raw_usage.as_ref() {
            let previous = self
                .last_usage
                .get_or_insert_with(crate::UsageMetrics::default);
            previous.input_tokens = previous.input_tokens.max(usage.input_tokens);
            previous.output_tokens = previous.output_tokens.max(usage.output_tokens);
            previous.total_tokens = previous
                .total_tokens
                .max(usage.total_tokens)
                .max(previous.input_tokens.saturating_add(previous.output_tokens));
            previous.cache_tokens = previous.cache_tokens.max(usage.cache_tokens);
            previous.reasoning_tokens = previous.reasoning_tokens.max(usage.reasoning_tokens);
            crate::quota_usage::merge_cumulative_usage(&mut previous.raw_usage, raw);
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
    fn anthropic_messages_url_uses_official_anthropic_route() {
        assert_eq!(
            anthropic_messages_url("https://api.minimax.io"),
            "https://api.minimax.io/anthropic/v1/messages"
        );
        assert_eq!(
            anthropic_messages_url("https://api.minimax.io/v1"),
            "https://api.minimax.io/anthropic/v1/messages"
        );
        assert_eq!(
            anthropic_messages_url("https://api.minimax.io/anthropic"),
            "https://api.minimax.io/anthropic/v1/messages"
        );
        assert_eq!(
            anthropic_messages_url("https://api.minimax.io/anthropic/v1"),
            "https://api.minimax.io/anthropic/v1/messages"
        );
        assert_eq!(
            anthropic_messages_url("https://api.minimaxi.com/anthropic"),
            "https://api.minimaxi.com/anthropic/v1/messages"
        );
    }

    #[test]
    fn anthropic_messages_url_converts_codex_endpoint_base() {
        assert_eq!(
            anthropic_messages_url("https://api.minimax.io/v1/responses"),
            "https://api.minimax.io/anthropic/v1/messages"
        );
        assert_eq!(
            anthropic_messages_url("https://api.minimax.io/v1/chat/completions"),
            "https://api.minimax.io/anthropic/v1/messages"
        );
    }

    #[test]
    fn anthropic_sse_tracker_rejects_terminal_error_events() {
        let mut tracker = AnthropicSseUsageTracker::default();
        tracker.push(&Bytes::from_static(
            b"data: {\"type\":\"error\",\"error\":{\"message\":\"minimax quota\"}}\n\n",
        ));
        assert!(matches!(
            tracker.finish(),
            Err(message) if message == "minimax quota"
        ));
    }
}
