use crate::source::v1::multimodal::openai_chat_content;
use axum::{
    body::Body,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
};
use bytes::Bytes;
use futures_util::StreamExt;
use serde_json::{json, Value};
use std::time::Duration;
use uuid::Uuid;

use super::DEFAULT_BASE_URL;

const MODEL_FALLBACKS: &[&str] = &[
    "MiniMax-M3",
    "MiniMax-M2.7",
    "MiniMax-M2.7-highspeed",
    "MiniMax-M2",
];

pub fn normalize_base_url(base_url: Option<&str>) -> String {
    base_url
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(DEFAULT_BASE_URL)
        .trim_end_matches('/')
        .to_string()
}

pub(super) fn chat_completions_url(base_url: &str) -> String {
    let base = normalize_base_url(Some(base_url));
    if base.ends_with("/v1/chat/completions") {
        return base;
    }
    if base.ends_with("/chat/completions") {
        return base;
    }
    if base.ends_with("/v1") {
        return format!("{}/chat/completions", base);
    }
    format!("{}/v1/chat/completions", base)
}

fn models_url(base_url: &str) -> String {
    let base = normalize_base_url(Some(base_url));
    if base.ends_with("/models") {
        return base;
    }
    if base.ends_with("/v1") {
        return format!("{}/models", base);
    }
    format!("{}/v1/models", base)
}

pub async fn validate_api_key(
    client: &reqwest::Client,
    api_key: &str,
    base_url: &str,
) -> Result<(), String> {
    let resp = client
        .get(models_url(base_url))
        .header("Authorization", format!("Bearer {}", api_key.trim()))
        .header("Accept", "application/json")
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .map_err(|e| format!("MiniMax models request failed: {}", e))?;
    let status = resp.status();
    let text = resp
        .text()
        .await
        .map_err(|e| format!("MiniMax models body read failed: {}", e))?;
    if !status.is_success() {
        return Err(format!("MiniMax models returned {}: {}", status, text));
    }
    Ok(())
}

pub async fn models(State(state): State<crate::AppState>, headers: HeaderMap) -> impl IntoResponse {
    if !crate::check_api_key(&state, &headers) {
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

    let account = match super::accounts::first_enabled(&state) {
        Some(account) => account,
        None => {
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
    };

    let api_key = account.api_key.clone();
    let base_url = normalize_base_url(account.base_url.as_deref());
    match fetch_models_json(&state.client, &api_key, &base_url).await {
        Ok(value) => match models_to_openai_json(&value) {
            Ok(body) => {
                (StatusCode::OK, [("Content-Type", "application/json")], body).into_response()
            }
            Err(err) => (
                StatusCode::BAD_GATEWAY,
                [("Content-Type", "application/json")],
                crate::source::v1::response::openai_error_body(&err, "server_error", None),
            )
                .into_response(),
        },
        Err(err) => {
            let mut data = MODEL_FALLBACKS
                .iter()
                .map(|id| {
                    json!({
                        "id": id,
                        "object": "model",
                        "created": 0,
                        "owned_by": "minimax"
                    })
                })
                .collect::<Vec<_>>();
            append_media_models(&mut data);
            let body = serde_json::to_vec(&json!({
                "object": "list",
                "data": data,
                "models": data,
                "warning": err
            }))
            .unwrap_or_default();
            (StatusCode::OK, [("Content-Type", "application/json")], body).into_response()
        }
    }
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

    let wants_stream = crate::source::wants_stream(&headers, &body);

    let mut chat_payload = match build_chat_completions_payload(&raw, &model) {
        Ok(payload) => payload,
        Err(err) => {
            crate::release_api_key_budget_before_dispatch(&state);
            return (
                StatusCode::BAD_REQUEST,
                [("Content-Type", "application/json")],
                crate::source::v1::response::openai_error_body(&err, "invalid_request_error", None),
            )
                .into_response();
        }
    };

    if wants_stream {
        chat_payload["stream"] = json!(true);
        if chat_payload.get("stream_options").is_none() {
            chat_payload["stream_options"] = json!({"include_usage": true});
        }
    }

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

    let prompt_metrics = crate::prompt_metrics_from_request_value(&raw);
    let mut last_error: Option<(StatusCode, String)> = None;

    for (attempt_idx, account) in accounts.iter().enumerate() {
        let context = crate::minimax_usage_context(
            account,
            Some(model.clone()),
            "/minimax/v1/chat/completions",
            prompt_metrics.clone(),
        );
        if let Err(response) = crate::api_key_quota_runtime::reserve_api_key_budgets_for_prepared_dispatch_with_protocol(
            &state, context.provider_name, &context.key,
            &serde_json::to_vec(&chat_payload).unwrap_or_default(),
            crate::quota_usage::PreparedProtocol::ChatCompletions,
        ).await {
            return response;
        }
        crate::record_minimax_request(&state, &context);

        let base_url = normalize_base_url(account.base_url.as_deref());

        if wants_stream {
            match stream_chat_completions(
                &state,
                &account.api_key,
                &base_url,
                &chat_payload,
                &context,
                &model,
                &headers,
            )
            .await
            {
                Ok(response) => return response,
                Err((status, message)) => {
                    crate::record_minimax_error(&state, &context, &message);
                    if attempt_idx + 1 < accounts.len()
                        && crate::should_retry_account_error(status, &message)
                    {
                        last_error = Some((status, message));
                        continue;
                    }
                    return (
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
                        .into_response();
                }
            }
        }

        let resp = match state
            .client
            .post(chat_completions_url(&base_url))
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
                crate::record_minimax_error(&state, &context, &message);
                last_error = Some((StatusCode::BAD_GATEWAY, message));
                if attempt_idx + 1 < accounts.len() {
                    continue;
                }
                break;
            }
        };

        let status = resp.status();
        let text = match resp.text().await {
            Ok(text) => text,
            Err(err) => {
                let message = format!("MiniMax body read failed: {}", err);
                crate::record_minimax_error(&state, &context, &message);
                last_error = Some((StatusCode::BAD_GATEWAY, message));
                if attempt_idx + 1 < accounts.len() {
                    continue;
                }
                break;
            }
        };

        if !status.is_success() {
            let message = format!("MiniMax returned {}: {}", status, text);
            crate::record_minimax_error(&state, &context, &message);
            if attempt_idx + 1 < accounts.len()
                && crate::should_retry_account_error(status, &message)
            {
                last_error = Some((status, message));
                continue;
            }
            return (
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
                .into_response();
        }

        let chat_response: Value = match serde_json::from_str(&text) {
            Ok(value) => value,
            Err(err) => {
                let message = format!("invalid MiniMax response: {}", err);
                crate::record_minimax_error(&state, &context, &message);
                return (
                    StatusCode::BAD_GATEWAY,
                    [("Content-Type", "application/json")],
                    crate::source::v1::response::openai_error_body(&message, "server_error", None),
                )
                    .into_response();
            }
        };

        let response = chat_completion_to_responses(&chat_response, &model);
        let mut usage = crate::usage_metrics_from_response_value(&response);
        crate::quota_usage::preserve_native_usage(&mut usage, &chat_response, "openai");
        crate::record_minimax_success(&state, &context, &usage);

        let body = serde_json::to_vec(&response).unwrap_or_default();
        return (StatusCode::OK, [("Content-Type", "application/json")], body).into_response();
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

pub(super) async fn stream_chat_completions(
    state: &crate::AppState,
    api_key: &str,
    base_url: &str,
    payload: &Value,
    context: &crate::UsageContext,
    model: &str,
    _headers: &HeaderMap,
) -> Result<axum::response::Response, (StatusCode, String)> {
    let mut payload = payload.clone();
    payload["stream"] = json!(true);
    if payload.get("stream_options").is_none() {
        payload["stream_options"] = json!({ "include_usage": true });
    }

    let resp = match state
        .client
        .post(chat_completions_url(base_url))
        .header("Authorization", format!("Bearer {}", api_key.trim()))
        .header("Content-Type", "application/json")
        .header("Accept", "text/event-stream")
        .body(payload.to_string())
        .timeout(Duration::from_secs(180))
        .send()
        .await
    {
        Ok(resp) => resp,
        Err(err) => {
            let message = format!("MiniMax stream request failed: {}", err);
            return Err((StatusCode::BAD_GATEWAY, message));
        }
    };

    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        return Err((
            status,
            format!("MiniMax stream returned {}: {}", status, text),
        ));
    }

    let usage_state = state.clone();
    let usage_context = context.clone();
    let model = model.to_string();
    let lifecycle = crate::StreamRequestGuard::new(&usage_state, &usage_context);
    let stream = async_stream::stream! {
        let mut lifecycle = lifecycle;
        let mut upstream = resp.bytes_stream();
        let mut parser = MiniMaxSseParser::default();
        let mut accumulator = MiniMaxStreamAccumulator::new(model.clone());
        yield Ok::<Bytes, std::io::Error>(response_sse_event(&json!({
            "type": "response.created",
            "response": accumulator.in_progress_response()
        })));

        while let Some(chunk) = upstream.next().await {
            let bytes = match chunk {
                Ok(bytes) => bytes,
                Err(err) => {
                    let message = format!("MiniMax stream body read failed: {}", err);
                    crate::record_minimax_error(&usage_state, &usage_context, &message);
                    lifecycle.finish();
                    yield Ok(response_sse_event(&json!({
                        "type": "response.failed",
                        "error": {
                            "message": message,
                            "type": "server_error"
                        }
                    })));
                    yield Ok(done_sse_event());
                    return;
                }
            };

            for event in parser.push(&bytes) {
                accumulator.absorb_sse_data(&event);
            }
            if let Some(message) = parser.terminal_error().map(str::to_string) {
                crate::record_minimax_error(&usage_state, &usage_context, &message);
                lifecycle.finish();
                yield Ok(response_sse_event(&json!({
                    "type": "response.failed",
                    "error": {
                        "message": message,
                        "type": "server_error"
                    }
                })));
                yield Ok(done_sse_event());
                return;
            }
        }

        for event in parser.finish() {
            accumulator.absorb_sse_data(&event);
        }
        if let Some(message) = parser.terminal_error().map(str::to_string) {
            crate::record_minimax_error(&usage_state, &usage_context, &message);
            lifecycle.finish();
            yield Ok(response_sse_event(&json!({
                "type": "response.failed",
                "error": {
                    "message": message,
                    "type": "server_error"
                }
            })));
            yield Ok(done_sse_event());
            return;
        }

        let response = accumulator.to_response();
        let mut metrics = crate::usage_metrics_from_response_value(&response);
        crate::quota_usage::mark_stream_usage(&mut metrics, "openai", accumulator.usage_at_terminal);
        crate::record_minimax_success(&usage_state, &usage_context, &metrics);
        lifecycle.finish();
        for event in response_output_events(&response) {
            yield Ok(event);
        }
        yield Ok(response_sse_event(&json!({
            "type": "response.completed",
            "response": response
        })));
        yield Ok(done_sse_event());
    };

    Ok((
        StatusCode::OK,
        [
            ("Content-Type", "text/event-stream"),
            ("Cache-Control", "no-store"),
        ],
        Body::from_stream(stream),
    )
        .into_response())
}

async fn fetch_models_json(
    client: &reqwest::Client,
    api_key: &str,
    base_url: &str,
) -> Result<Value, String> {
    let resp = client
        .get(models_url(base_url))
        .header("Authorization", format!("Bearer {}", api_key.trim()))
        .header("Accept", "application/json")
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .map_err(|e| format!("MiniMax models request failed: {}", e))?;

    let status = resp.status();
    let text = resp
        .text()
        .await
        .map_err(|e| format!("MiniMax models body read failed: {}", e))?;
    if !status.is_success() {
        return Err(format!("MiniMax models returned {}: {}", status, text));
    }
    serde_json::from_str(&text).map_err(|e| format!("invalid MiniMax models JSON: {}", e))
}

fn models_to_openai_json(value: &Value) -> Result<Vec<u8>, String> {
    let models = value
        .get("data")
        .and_then(|v| v.as_array())
        .or_else(|| value.as_array())
        .ok_or_else(|| "MiniMax models response was missing data".to_string())?;

    let mut data = models
        .iter()
        .filter_map(|model| {
            let id = model
                .get("id")
                .and_then(|v| v.as_str())
                .or_else(|| model.get("model").and_then(|v| v.as_str()))?;
            Some(json!({
                "id": id,
                "object": "model",
                "created": 0,
                "owned_by": model.get("owned_by").and_then(|v| v.as_str()).unwrap_or("minimax")
            }))
        })
        .collect::<Vec<_>>();
    append_media_models(&mut data);

    serde_json::to_vec(&json!({
        "object": "list",
        "data": data,
        "models": data
    }))
    .map_err(|e| e.to_string())
}

fn append_media_models(data: &mut Vec<Value>) {
    for media_model in super::media::media_model_records() {
        let id = media_model.get("id").and_then(|value| value.as_str());
        if id.is_some_and(|id| {
            data.iter()
                .any(|existing| existing.get("id").and_then(|value| value.as_str()) == Some(id))
        }) {
            continue;
        }
        data.push(media_model);
    }
}

pub(super) fn build_chat_completions_payload(raw: &Value, model: &str) -> Result<Value, String> {
    let mut out = json!({
        "model": model,
        "stream": false,
    });

    if let Some(messages) = build_chat_messages(raw)? {
        out["messages"] = messages;
    } else {
        return Err("request did not contain any messages or input".to_string());
    }

    if let Some(max_tokens) = raw
        .get("max_output_tokens")
        .or_else(|| raw.get("max_tokens"))
        .and_then(|v| v.as_u64())
    {
        out["max_tokens"] = json!(max_tokens);
    }
    if let Some(temperature) = raw.get("temperature").and_then(|v| v.as_f64()) {
        out["temperature"] = json!(temperature);
    }
    if let Some(top_p) = raw.get("top_p").and_then(|v| v.as_f64()) {
        out["top_p"] = json!(top_p);
    }
    if let Some(tools) = build_chat_tools(raw) {
        out["tools"] = tools;
    }
    if let Some(tool_choice) = build_chat_tool_choice(raw) {
        out["tool_choice"] = tool_choice;
    }
    if let Some(stop) = raw.get("stop").cloned() {
        out["stop"] = stop;
    }
    if let Some(stream) = raw.get("stream").cloned() {
        out["stream"] = stream;
    }
    if let Some(thinking) = build_chat_thinking(raw) {
        out["thinking"] = thinking;
    }
    Ok(out)
}

/// Map the Codex SDK's `reasoning: {effort: ...}` field to MiniMax's
/// `thinking: {type: ...}` field for the chat-completions endpoint.
///
/// * `effort: "none"` — explicit no-thinking → `thinking: {type: "disabled"}`.
/// * Any other value — adaptive thinking → `thinking: {type: "adaptive"}`.
/// * `reasoning_effort: "low|medium|high|minimal"` (top-level alias) — same
///   translation as above.
/// * Default for `MiniMax-M3` when the client did not set reasoning —
///   adaptive thinking, so the model uses its full reasoning depth on
///   agentic tasks. For M2.x models MiniMax always thinks, so leaving
///   the field off is fine.
fn build_chat_thinking(raw: &Value) -> Option<Value> {
    if let Some(reasoning) = raw.get("reasoning") {
        if let Some(obj) = reasoning.as_object() {
            let effort = obj.get("effort").and_then(|v| v.as_str()).unwrap_or("none");
            return Some(match effort {
                "none" => json!({ "type": "disabled" }),
                _ => json!({ "type": "adaptive" }),
            });
        }
    }

    if let Some(effort) = raw.get("reasoning_effort").and_then(|v| v.as_str()) {
        return Some(match effort {
            "none" => json!({ "type": "disabled" }),
            _ => json!({ "type": "adaptive" }),
        });
    }

    let model = raw.get("model").and_then(|v| v.as_str()).unwrap_or("");
    if model.eq_ignore_ascii_case("MiniMax-M3") {
        // M3 only enters Adaptive Thinking when the `thinking` field is
        // present. Without it the model produces very short answers and
        // the Codex agent loop sees the model as "stopping before task
        // done" because there is not enough content for Codex to
        // consider the turn complete. We default to `adaptive` for M3.
        return Some(json!({ "type": "adaptive" }));
    }

    None
}

fn build_chat_messages(raw: &Value) -> Result<Option<Value>, String> {
    if let Some(messages) = raw.get("messages").and_then(|v| v.as_array()) {
        let mut out = Vec::new();
        for message in messages {
            let role = message
                .get("role")
                .and_then(|v| v.as_str())
                .unwrap_or("user")
                .trim()
                .to_ascii_lowercase();
            let role = match role.as_str() {
                "system" | "developer" => "system",
                "assistant" => "assistant",
                "user" => "user",
                "tool" => "tool",
                other => other,
            };
            let content = openai_chat_content(message.get("content"));
            let mut entry = json!({
                "role": role,
                "content": content.unwrap_or(Value::String(String::new()))
            });
            if let Some(name) = message.get("name").and_then(|v| v.as_str()) {
                entry["name"] = json!(name);
            }
            if let Some(tool_call_id) = message.get("tool_call_id").and_then(|v| v.as_str()) {
                entry["tool_call_id"] = json!(tool_call_id);
            }
            if let Some(tool_calls) = message.get("tool_calls").and_then(|v| v.as_array()) {
                entry["tool_calls"] = Value::Array(tool_calls.clone());
            }
            out.push(entry);
        }
        return Ok(Some(Value::Array(sanitize_chat_messages(out))));
    }

    if let Some(prompt) = raw.get("prompt") {
        if let Some(text) = prompt.as_str() {
            return Ok(Some(json!([
                { "role": "user", "content": text }
            ])));
        }
    }

    if let Some(input) = raw.get("input") {
        if let Some(text) = input.as_str() {
            let mut out = Vec::new();
            if let Some(instructions) = raw
                .get("instructions")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                out.push(json!({ "role": "system", "content": instructions }));
            }
            out.push(json!({ "role": "user", "content": text }));
            return Ok(Some(Value::Array(out)));
        }

        if let Some(items) = input.as_array() {
            let mut out = Vec::new();
            if let Some(instructions) = raw
                .get("instructions")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                out.push(json!({ "role": "system", "content": instructions }));
            }
            for item in items {
                if let Some(entry) = input_item_to_chat_message(item)? {
                    out.push(entry);
                }
            }
            if out.is_empty() {
                return Ok(None);
            }
            return Ok(Some(Value::Array(sanitize_chat_messages(out))));
        }
    }

    Ok(None)
}

fn sanitize_chat_messages(messages: Vec<Value>) -> Vec<Value> {
    let mut out = Vec::new();
    let mut index = 0;

    while index < messages.len() {
        let message = &messages[index];
        let role = chat_message_role(message);
        if role == Some("tool") {
            index += 1;
            continue;
        }

        let has_tool_calls = message
            .get("tool_calls")
            .and_then(|v| v.as_array())
            .map(|calls| !calls.is_empty())
            .unwrap_or(false);
        if role == Some("assistant") && has_tool_calls {
            let mut pending_ids = chat_message_tool_call_ids(message);
            if pending_ids.is_empty() {
                index += 1;
                continue;
            }

            let mut tool_messages = Vec::new();
            let mut next = index + 1;
            while next < messages.len() && chat_message_role(&messages[next]) == Some("tool") {
                if let Some(tool_call_id) = chat_message_tool_call_id(&messages[next]) {
                    if let Some(pos) = pending_ids.iter().position(|id| id == tool_call_id) {
                        pending_ids.remove(pos);
                        tool_messages.push(messages[next].clone());
                    }
                }
                next += 1;
            }

            if !tool_messages.is_empty() {
                out.push(message.clone());
                out.extend(tool_messages);
            }
            index = next;
            continue;
        }

        out.push(message.clone());
        index += 1;
    }

    out
}

fn chat_message_role(message: &Value) -> Option<&str> {
    message.get("role").and_then(|v| v.as_str())
}

fn chat_message_tool_call_ids(message: &Value) -> Vec<String> {
    message
        .get("tool_calls")
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
        .filter_map(|call| call.get("id").and_then(|v| v.as_str()))
        .filter(|id| !id.is_empty())
        .map(ToString::to_string)
        .collect()
}

fn chat_message_tool_call_id(message: &Value) -> Option<&str> {
    message
        .get("tool_call_id")
        .and_then(|v| v.as_str())
        .filter(|id| !id.is_empty())
}

fn input_item_to_chat_message(item: &Value) -> Result<Option<Value>, String> {
    let item_type = item.get("type").and_then(|v| v.as_str()).unwrap_or("");
    let role = item
        .get("role")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();

    match item_type {
        "message" | "" => {
            let role = match role.as_str() {
                "system" | "developer" => "system",
                "assistant" => "assistant",
                "user" | "" => "user",
                "tool" => "tool",
                other => other,
            };
            let content = openai_chat_content(item.get("content"));
            let mut entry =
                json!({ "role": role, "content": content.unwrap_or(Value::String(String::new())) });
            if let Some(name) = item.get("name").and_then(|v| v.as_str()) {
                entry["name"] = json!(name);
            }
            if let Some(tool_call_id) = item.get("tool_call_id").and_then(|v| v.as_str()) {
                entry["tool_call_id"] = json!(tool_call_id);
            }
            if let Some(tool_calls) = item.get("tool_calls").and_then(|v| v.as_array()) {
                entry["tool_calls"] = Value::Array(tool_calls.clone());
            }
            Ok(Some(entry))
        }
        "function_call" => {
            let call_id = item
                .get("call_id")
                .or_else(|| item.get("id"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let name = item
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let arguments = item
                .get("arguments")
                .and_then(|v| v.as_str())
                .unwrap_or("{}");
            if name.is_empty() {
                return Ok(None);
            }
            let tool_call = json!({
                "id": call_id,
                "type": "function",
                "function": { "name": name, "arguments": arguments }
            });
            let assistant_text = item.get("arguments").and_then(|v| v.as_str()).unwrap_or("");
            let content = if assistant_text.is_empty() {
                Value::String(String::new())
            } else {
                Value::String(assistant_text.to_string())
            };
            Ok(Some(json!({
                "role": "assistant",
                "content": content,
                "tool_calls": [tool_call]
            })))
        }
        "function_call_output" => {
            let call_id = item
                .get("call_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let output = item
                .get("output")
                .cloned()
                .unwrap_or(Value::String(String::new()));
            let output_text = match output {
                Value::String(s) => s,
                other => serde_json::to_string(&other).unwrap_or_default(),
            };
            Ok(Some(json!({
                "role": "tool",
                "tool_call_id": call_id,
                "content": output_text
            })))
        }
        _ => Ok(None),
    }
}

fn build_chat_tools(raw: &Value) -> Option<Value> {
    let tools = raw.get("tools")?.as_array()?;
    let mut out = Vec::new();
    for tool in tools {
        if let Some(function) = tool.get("function") {
            if unsupported_tool_name(function.get("name").and_then(|v| v.as_str())) {
                continue;
            }
            let mut mapped = json!({
                "type": "function",
                "function": {
                    "name": function.get("name").cloned().unwrap_or(Value::String(String::new())),
                    "description": function.get("description").cloned().unwrap_or(Value::String(String::new())),
                    "parameters": function.get("parameters").cloned().unwrap_or(json!({ "type": "object" }))
                }
            });
            if let Some(strict) = function.get("strict").and_then(|v| v.as_bool()) {
                mapped["function"]["strict"] = json!(strict);
            }
            out.push(mapped);
        } else if let Some(name) = tool.get("name").and_then(|v| v.as_str()) {
            if unsupported_tool_name(Some(name)) {
                continue;
            }
            out.push(json!({
                "type": "function",
                "function": {
                    "name": name,
                    "description": tool.get("description").cloned().unwrap_or(Value::String(String::new())),
                    "parameters": tool.get("parameters").cloned().unwrap_or(json!({ "type": "object" }))
                }
            }));
        }
    }
    if out.is_empty() {
        None
    } else {
        Some(Value::Array(out))
    }
}

fn unsupported_tool_name(name: Option<&str>) -> bool {
    matches!(name, Some("apply_patch"))
}

fn build_chat_tool_choice(raw: &Value) -> Option<Value> {
    let choice = raw.get("tool_choice")?;
    if let Some(value) = choice.as_str() {
        return match value {
            "auto" | "none" | "required" => Some(json!(value)),
            other => Some(json!(other)),
        };
    }
    if choice
        .get("function")
        .and_then(|function| function.get("name"))
        .and_then(|name| name.as_str())
        .map(|name| unsupported_tool_name(Some(name)))
        .unwrap_or(false)
        || choice
            .get("name")
            .and_then(|name| name.as_str())
            .map(|name| unsupported_tool_name(Some(name)))
            .unwrap_or(false)
    {
        return None;
    }
    Some(choice.clone())
}

#[derive(Default)]
struct MiniMaxSseParser {
    buffer: Vec<u8>,
    terminal_error: Option<String>,
}

impl MiniMaxSseParser {
    fn push(&mut self, chunk: &[u8]) -> Vec<String> {
        self.buffer.extend_from_slice(chunk);
        let mut events = Vec::new();
        while let Some((event_end, delimiter_len)) = find_minimax_sse_boundary(&self.buffer) {
            let raw = self
                .buffer
                .drain(..event_end + delimiter_len)
                .collect::<Vec<_>>();
            if self.terminal_error.is_none() {
                if let Some(error) = crate::sse_terminal_error_from_event(&raw[..event_end]) {
                    self.terminal_error = Some(error);
                    continue;
                }
            }
            if let Some(data) = parse_minimax_sse_data(&raw[..event_end]) {
                events.push(data);
            }
        }
        events
    }

    fn finish(&mut self) -> Vec<String> {
        if self.buffer.is_empty() {
            return Vec::new();
        }
        let raw = std::mem::take(&mut self.buffer);
        if self.terminal_error.is_none() {
            if let Some(error) = crate::sse_terminal_error_from_event(&raw) {
                self.terminal_error = Some(error);
                return Vec::new();
            }
        }
        parse_minimax_sse_data(&raw).into_iter().collect()
    }

    fn terminal_error(&self) -> Option<&str> {
        self.terminal_error.as_deref()
    }
}

fn find_minimax_sse_boundary(buffer: &[u8]) -> Option<(usize, usize)> {
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

fn parse_minimax_sse_data(raw_event: &[u8]) -> Option<String> {
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

#[derive(Clone, Default)]
struct StreamToolCall {
    id: String,
    name: String,
    arguments: String,
}

struct MiniMaxStreamAccumulator {
    id: String,
    model: String,
    created: u64,
    content: String,
    reasoning_content: String,
    tool_calls: Vec<StreamToolCall>,
    usage: Option<Value>,
    saw_terminal: bool,
    usage_at_terminal: bool,
}

impl MiniMaxStreamAccumulator {
    fn new(model: String) -> Self {
        Self {
            id: format!("chatcmpl-{}", Uuid::new_v4().simple()),
            model,
            created: chrono::Utc::now().timestamp() as u64,
            content: String::new(),
            reasoning_content: String::new(),
            tool_calls: Vec::new(),
            usage: None,
            saw_terminal: false,
            usage_at_terminal: false,
        }
    }

    fn in_progress_response(&self) -> Value {
        json!({
            "id": self.response_id(),
            "object": "response",
            "created": self.created,
            "model": self.model,
            "status": "in_progress",
            "output": [],
            "output_text": ""
        })
    }

    fn absorb_sse_data(&mut self, data: &str) {
        if data.trim() == "[DONE]" {
            self.saw_terminal = true;
            return;
        }
        let Ok(value) = serde_json::from_str::<Value>(data) else {
            return;
        };
        self.absorb_chat_value(&value);
    }

    fn absorb_chat_value(&mut self, value: &Value) {
        self.saw_terminal |=
            value
                .get("choices")
                .and_then(Value::as_array)
                .is_some_and(|choices| {
                    choices.iter().any(|choice| {
                        choice
                            .get("finish_reason")
                            .and_then(Value::as_str)
                            .is_some()
                    })
                });
        if let Some(id) = value.get("id").and_then(|v| v.as_str()) {
            self.id = id.to_string();
        }
        if let Some(created) = value.get("created").and_then(|v| v.as_u64()) {
            self.created = created;
        }
        if let Some(usage) = value.get("usage").filter(|usage| !usage.is_null()) {
            crate::quota_usage::merge_cumulative_usage(&mut self.usage, usage);
            self.usage_at_terminal =
                self.saw_terminal && crate::quota_usage::has_output_observation(usage);
        }

        let Some(choices) = value.get("choices").and_then(|v| v.as_array()) else {
            return;
        };
        for choice in choices {
            if let Some(delta) = choice.get("delta") {
                self.absorb_delta(delta);
            }
            if let Some(message) = choice.get("message") {
                self.absorb_delta(message);
            }
        }
    }

    fn absorb_delta(&mut self, delta: &Value) {
        if let Some(text) = delta.get("content").and_then(|v| v.as_str()) {
            self.content.push_str(text);
        }
        if let Some(text) = delta.get("reasoning_content").and_then(|v| v.as_str()) {
            self.reasoning_content.push_str(text);
        }
        if let Some(tool_calls) = delta.get("tool_calls").and_then(|v| v.as_array()) {
            for tool_call in tool_calls {
                self.absorb_tool_call(tool_call);
            }
        }
    }

    fn absorb_tool_call(&mut self, value: &Value) {
        let index = value
            .get("index")
            .and_then(|v| v.as_u64())
            .unwrap_or(self.tool_calls.len() as u64) as usize;
        while self.tool_calls.len() <= index {
            self.tool_calls.push(StreamToolCall::default());
        }
        let tool_call = &mut self.tool_calls[index];
        if let Some(id) = value.get("id").and_then(|v| v.as_str()) {
            tool_call.id = id.to_string();
        }
        if let Some(function) = value.get("function") {
            if let Some(name) = function.get("name").and_then(|v| v.as_str()) {
                tool_call.name.push_str(name);
            }
            if let Some(arguments) = function.get("arguments").and_then(|v| v.as_str()) {
                tool_call.arguments.push_str(arguments);
            }
        }
    }

    fn to_response(&self) -> Value {
        let mut message = json!({
            "role": "assistant",
            "content": self.content.clone()
        });
        if !self.reasoning_content.trim().is_empty() {
            message["reasoning_content"] = json!(self.reasoning_content.clone());
        }
        let tool_calls = self
            .tool_calls
            .iter()
            .filter(|tool_call| !tool_call.name.is_empty())
            .map(|tool_call| {
                let id = if tool_call.id.is_empty() {
                    format!("call_{}", Uuid::new_v4().simple())
                } else {
                    tool_call.id.clone()
                };
                let arguments = if tool_call.arguments.is_empty() {
                    "{}".to_string()
                } else {
                    tool_call.arguments.clone()
                };
                json!({
                    "id": id,
                    "type": "function",
                    "function": {
                        "name": tool_call.name.clone(),
                        "arguments": arguments
                    }
                })
            })
            .collect::<Vec<_>>();
        if !tool_calls.is_empty() {
            message["tool_calls"] = Value::Array(tool_calls);
        }

        let chat = json!({
            "id": self.id.clone(),
            "created": self.created,
            "model": self.model.clone(),
            "choices": [{ "message": message }],
            "usage": self.usage.clone().unwrap_or_else(|| json!({}))
        });
        let mut response = chat_completion_to_responses(&chat, &self.model);
        if let Some(response_obj) = response.as_object_mut() {
            response_obj.insert("id".to_string(), Value::String(self.response_id()));
        }
        response
    }

    fn response_id(&self) -> String {
        if self.id.starts_with("resp_") {
            self.id.clone()
        } else {
            format!("resp_{}", self.id)
        }
    }
}

fn response_sse_event(value: &Value) -> Bytes {
    let data = serde_json::to_string(value).unwrap_or_else(|_| "{}".to_string());
    let event = value
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or("message");
    Bytes::from(format!("event: {}\ndata: {}\n\n", event, data))
}

fn done_sse_event() -> Bytes {
    Bytes::from_static(b"data: [DONE]\n\n")
}

fn response_output_events(response: &Value) -> Vec<Bytes> {
    let mut output_items = response
        .get("output")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    if output_items.is_empty() {
        if let Some(text) = response
            .get("output_text")
            .and_then(|v| v.as_str())
            .filter(|text| !text.is_empty())
        {
            output_items.push(json!({
                "type": "message",
                "id": format!("msg_{}", Uuid::new_v4().simple()),
                "status": "completed",
                "role": "assistant",
                "content": [{
                    "type": "output_text",
                    "text": text,
                    "annotations": []
                }]
            }));
        }
    }

    let mut events = Vec::new();
    for (output_index, item) in output_items.iter().enumerate() {
        events.extend(response_output_item_events(output_index, item));
    }
    events
}

fn response_output_item_events(output_index: usize, item: &Value) -> Vec<Bytes> {
    let mut events = vec![response_sse_event(&json!({
        "type": "response.output_item.added",
        "output_index": output_index,
        "item": response_item_with_status(item, "in_progress")
    }))];

    match item.get("type").and_then(|v| v.as_str()) {
        Some("message") => {
            if let Some(content) = item.get("content").and_then(|v| v.as_array()) {
                for (content_index, part) in content.iter().enumerate() {
                    if part.get("type").and_then(|v| v.as_str()) != Some("output_text") {
                        continue;
                    }
                    let text = part.get("text").and_then(|v| v.as_str()).unwrap_or("");
                    let item_id = response_item_id(item);
                    let added_part = json!({
                        "type": "output_text",
                        "text": "",
                        "annotations": part
                            .get("annotations")
                            .cloned()
                            .unwrap_or_else(|| json!([]))
                    });
                    events.push(response_sse_event(&json!({
                        "type": "response.content_part.added",
                        "item_id": item_id,
                        "output_index": output_index,
                        "content_index": content_index,
                        "part": added_part
                    })));
                    for delta in text.split_inclusive('\n').filter(|delta| !delta.is_empty()) {
                        events.push(response_sse_event(&json!({
                            "type": "response.output_text.delta",
                            "item_id": item_id,
                            "output_index": output_index,
                            "content_index": content_index,
                            "delta": delta
                        })));
                    }
                    events.push(response_sse_event(&json!({
                        "type": "response.output_text.done",
                        "item_id": item_id,
                        "output_index": output_index,
                        "content_index": content_index,
                        "text": text
                    })));
                    events.push(response_sse_event(&json!({
                        "type": "response.content_part.done",
                        "item_id": item_id,
                        "output_index": output_index,
                        "content_index": content_index,
                        "part": part
                    })));
                }
            }
        }
        Some("function_call") => {
            if let Some(arguments) = item
                .get("arguments")
                .and_then(|v| v.as_str())
                .filter(|arguments| !arguments.is_empty())
            {
                let item_id = response_item_id(item);
                events.push(response_sse_event(&json!({
                    "type": "response.function_call_arguments.delta",
                    "item_id": item_id,
                    "output_index": output_index,
                    "delta": arguments
                })));
                events.push(response_sse_event(&json!({
                    "type": "response.function_call_arguments.done",
                    "item_id": item_id,
                    "output_index": output_index,
                    "arguments": arguments
                })));
            }
        }
        Some("reasoning") => {}
        _ => {}
    }

    events.push(response_sse_event(&json!({
        "type": "response.output_item.done",
        "output_index": output_index,
        "item": item
    })));
    events
}

fn response_item_with_status(item: &Value, status: &str) -> Value {
    let mut item = item.clone();
    if let Some(object) = item.as_object_mut() {
        if object.contains_key("status") {
            object.insert("status".to_string(), json!(status));
        }
    }
    item
}

fn response_item_id(item: &Value) -> String {
    item.get("id")
        .and_then(|v| v.as_str())
        .or_else(|| item.get("call_id").and_then(|v| v.as_str()))
        .map(str::to_string)
        .unwrap_or_else(|| format!("item_{}", Uuid::new_v4().simple()))
}

pub(super) fn chat_completion_to_responses(chat: &Value, model: &str) -> Value {
    let id = chat
        .get("id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| format!("chatcmpl-{}", Uuid::new_v4().simple()));
    let created = chat
        .get("created")
        .and_then(|v| v.as_u64())
        .unwrap_or_else(|| chrono::Utc::now().timestamp() as u64);

    let mut output_text = String::new();
    let mut output: Vec<Value> = Vec::new();
    if let Some(choices) = chat.get("choices").and_then(|v| v.as_array()) {
        for choice in choices {
            if let Some(message) = choice.get("message") {
                let mut had_reasoning = false;
                if let Some(reasoning) = message
                    .get("reasoning_content")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    push_reasoning_output(&mut output, reasoning);
                    had_reasoning = true;
                }
                let mut had_content = false;
                if let Some(content) = message.get("content") {
                    if let Some(text) = content.as_str() {
                        let (inline_reasoning, visible_text) = split_inline_thinking(text);
                        if let Some(reasoning) = inline_reasoning {
                            if !had_reasoning {
                                push_reasoning_output(&mut output, &reasoning);
                            }
                        }
                        if !visible_text.is_empty() {
                            output_text.push_str(&visible_text);
                            had_content = true;
                        }
                    } else if let Some(parts) = content.as_array() {
                        for part in parts {
                            if let Some(text) = part.get("text").and_then(|v| v.as_str()) {
                                let (inline_reasoning, visible_text) = split_inline_thinking(text);
                                if let Some(reasoning) = inline_reasoning {
                                    if !had_reasoning {
                                        push_reasoning_output(&mut output, &reasoning);
                                        had_reasoning = true;
                                    }
                                }
                                if !visible_text.is_empty() {
                                    output_text.push_str(&visible_text);
                                    had_content = true;
                                }
                            }
                        }
                    }
                }
                if had_content {
                    output.push(json!({
                        "type": "message",
                        "id": format!("msg_{}", Uuid::new_v4().simple()),
                        "status": "completed",
                        "role": "assistant",
                        "content": [{
                            "type": "output_text",
                            "text": output_text.clone(),
                            "annotations": []
                        }]
                    }));
                }
                if let Some(tool_calls) = message.get("tool_calls").and_then(|v| v.as_array()) {
                    for call in tool_calls {
                        let call_id = call
                            .get("id")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string())
                            .unwrap_or_else(|| format!("call_{}", Uuid::new_v4().simple()));
                        let name = call
                            .get("function")
                            .and_then(|f| f.get("name"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("tool")
                            .to_string();
                        let arguments = call
                            .get("function")
                            .and_then(|f| f.get("arguments"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("{}")
                            .to_string();
                        output.push(json!({
                            "id": call_id,
                            "type": "function_call",
                            "call_id": call_id,
                            "name": name,
                            "arguments": arguments
                        }));
                    }
                }
            }
        }
    }

    let usage = chat.get("usage").cloned().unwrap_or(json!({}));
    let usage = crate::quota_usage::openai_compat_usage(&usage);

    json!({
        "id": id,
        "object": "response",
        "created": created,
        "model": model,
        "status": "completed",
        "output": output,
        "output_text": output_text,
        "usage": usage
    })
}

fn push_reasoning_output(output: &mut Vec<Value>, reasoning: &str) {
    output.push(json!({
        "id": format!("rs_{}", Uuid::new_v4().simple()),
        "type": "reasoning",
        "summary": [{
            "type": "summary_text",
            "text": reasoning
        }],
        "content": reasoning
    }));
}

fn split_inline_thinking(text: &str) -> (Option<String>, String) {
    let trimmed = text.trim_start();
    let Some(rest) = trimmed.strip_prefix("<think>") else {
        return (None, text.to_string());
    };
    let Some(end) = rest.find("</think>") else {
        return (None, text.to_string());
    };

    let reasoning = rest[..end].trim().to_string();
    let visible = rest[end + "</think>".len()..]
        .trim_start_matches(['\r', '\n'])
        .to_string();
    let reasoning = if reasoning.is_empty() {
        None
    } else {
        Some(reasoning)
    };
    (reasoning, visible)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quota_stream_partial_usage_cannot_be_blessed_by_empty_final_event() {
        let mut accumulator = MiniMaxStreamAccumulator::new("fixture-model".into());
        accumulator.absorb_chat_value(&json!({"choices":[{"delta":{"content":"x"}}],"usage":{"prompt_tokens":10,"completion_tokens":0}}));
        accumulator.absorb_chat_value(&json!({"choices":[{"finish_reason":"stop"}],"usage":{}}));
        assert!(!accumulator.usage_at_terminal);
        accumulator.absorb_chat_value(
            &json!({"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":8}}),
        );
        assert!(accumulator.usage_at_terminal);
        let response = accumulator.to_response();
        let usage = crate::quota_usage::normalize("openai", &response["usage"], true);
        assert_eq!(usage.input_tokens, Some(10));
        assert_eq!(usage.output_tokens, Some(8));
        assert_eq!(usage.cache_tokens, None);
        assert_eq!(usage.reasoning_tokens, None);
    }

    #[test]
    fn quota_conversion_does_not_invent_zero_usage() {
        let response = chat_completion_to_responses(
            &json!({"choices":[{"message":{"content":"x"}}]}),
            "fixture-model",
        );
        let usage = crate::quota_usage::normalize("openai", &response["usage"], true);
        assert_eq!(usage.input_tokens, None);
        assert_eq!(usage.output_tokens, None);
        assert_eq!(usage.cache_tokens, None);
        assert_eq!(usage.reasoning_tokens, None);
    }

    #[test]
    fn normalize_base_url_defaults_to_official() {
        assert_eq!(normalize_base_url(None), DEFAULT_BASE_URL);
        assert_eq!(
            normalize_base_url(Some("https://example.com/")),
            "https://example.com"
        );
    }

    #[test]
    fn chat_completions_url_handles_known_shapes() {
        assert_eq!(
            chat_completions_url("https://api.minimaxi.chat"),
            "https://api.minimaxi.chat/v1/chat/completions"
        );
        assert_eq!(
            chat_completions_url("https://api.minimaxi.chat/v1"),
            "https://api.minimaxi.chat/v1/chat/completions"
        );
        assert_eq!(
            chat_completions_url("https://example.com/v1/chat/completions"),
            "https://example.com/v1/chat/completions"
        );
    }

    #[test]
    fn build_chat_completions_payload_maps_messages_and_tools() {
        let raw = json!({
            "model": "MiniMax-Text-01",
            "messages": [
                { "role": "system", "content": "be brief" },
                { "role": "user", "content": "hi" }
            ],
            "max_output_tokens": 64,
            "tools": [
                {
                    "type": "function",
                    "function": {
                        "name": "echo",
                        "parameters": { "type": "object" }
                    }
                }
            ]
        });
        let payload = build_chat_completions_payload(&raw, "MiniMax-Text-01").unwrap();
        assert_eq!(payload["model"], "MiniMax-Text-01");
        assert_eq!(payload["max_tokens"], 64);
        assert_eq!(payload["messages"][0]["role"], "system");
        assert_eq!(payload["messages"][1]["content"], "hi");
        assert_eq!(payload["tools"][0]["function"]["name"], "echo");
    }

    #[test]
    fn build_chat_completions_payload_filters_apply_patch_tool() {
        let raw = json!({
            "model": "MiniMax-Text-01",
            "input": "hi",
            "tools": [
                {
                    "type": "function",
                    "name": "apply_patch",
                    "description": "patch files",
                    "parameters": { "type": "object" }
                },
                {
                    "type": "function",
                    "name": "shell",
                    "description": "run command",
                    "parameters": { "type": "object" }
                }
            ],
            "tool_choice": {
                "type": "function",
                "function": { "name": "apply_patch" }
            }
        });

        let payload = build_chat_completions_payload(&raw, "MiniMax-Text-01").unwrap();
        assert_eq!(payload["tools"].as_array().unwrap().len(), 1);
        assert_eq!(payload["tools"][0]["function"]["name"], "shell");
        assert!(payload.get("tool_choice").is_none());
    }

    #[test]
    fn build_chat_completions_payload_defaults_thinking_for_m3() {
        let raw = json!({"model": "MiniMax-M3", "input": "hi"});
        let payload = build_chat_completions_payload(&raw, "MiniMax-M3").unwrap();
        assert_eq!(payload["thinking"]["type"], "adaptive");
    }

    #[test]
    fn build_chat_completions_payload_respects_explicit_none_reasoning() {
        let raw = json!({
            "model": "MiniMax-M3",
            "input": "hi",
            "reasoning": {"effort": "none"}
        });
        let payload = build_chat_completions_payload(&raw, "MiniMax-M3").unwrap();
        assert_eq!(payload["thinking"]["type"], "disabled");
    }

    #[test]
    fn build_chat_completions_payload_forwards_non_none_reasoning_as_adaptive() {
        let raw = json!({
            "model": "MiniMax-M3",
            "input": "hi",
            "reasoning": {"effort": "high"}
        });
        let payload = build_chat_completions_payload(&raw, "MiniMax-M3").unwrap();
        assert_eq!(payload["thinking"]["type"], "adaptive");
    }

    #[test]
    fn build_chat_completions_payload_handles_reasoning_effort_alias() {
        let raw = json!({
            "model": "MiniMax-M3",
            "input": "hi",
            "reasoning_effort": "medium"
        });
        let payload = build_chat_completions_payload(&raw, "MiniMax-M3").unwrap();
        assert_eq!(payload["thinking"]["type"], "adaptive");
    }

    #[test]
    fn build_chat_completions_payload_omits_thinking_for_m2() {
        let raw = json!({"model": "MiniMax-M2.7", "input": "hi"});
        let payload = build_chat_completions_payload(&raw, "MiniMax-M2.7").unwrap();
        assert!(payload.get("thinking").is_none());
    }

    #[test]
    fn models_to_openai_json_includes_codex_models_field() {
        let body = models_to_openai_json(&json!({
            "data": [{ "id": "MiniMax-M3", "owned_by": "minimax" }]
        }))
        .unwrap();
        let value: Value = serde_json::from_slice(&body).unwrap();

        assert_eq!(value["data"][0]["id"], "MiniMax-M3");
        assert_eq!(value["models"][0]["id"], "MiniMax-M3");
        assert!(value["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|model| model["id"] == crate::target::minimax::media::IMAGE_MODEL));
        assert!(value["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|model| model["id"] == crate::target::minimax::media::VIDEO_MODEL_H3));
    }

    #[test]
    fn build_chat_completions_payload_maps_responses_input() {
        let raw = json!({
            "model": "MiniMax-Text-01",
            "instructions": "be brief",
            "input": [
                { "type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}] },
                { "type": "function_call", "call_id": "call_1", "name": "echo", "arguments": "{\"x\":1}" },
                { "type": "function_call_output", "call_id": "call_1", "output": "ok" }
            ]
        });
        let payload = build_chat_completions_payload(&raw, "MiniMax-Text-01").unwrap();
        assert_eq!(payload["messages"][0]["role"], "system");
        assert_eq!(payload["messages"][1]["role"], "user");
        assert_eq!(payload["messages"][2]["role"], "assistant");
        assert_eq!(payload["messages"][2]["tool_calls"][0]["id"], "call_1");
        assert_eq!(payload["messages"][3]["role"], "tool");
        assert_eq!(payload["messages"][3]["tool_call_id"], "call_1");
        assert_eq!(payload["messages"][3]["content"], "ok");
    }

    #[test]
    fn build_chat_completions_payload_drops_orphan_tool_outputs() {
        let raw = json!({
            "model": "MiniMax-M3",
            "input": [
                { "type": "message", "role": "user", "content": [{"type": "input_text", "text": "continue"}] },
                { "type": "function_call_output", "call_id": "call_missing", "output": "ok" }
            ],
            "tools": [
                { "type": "function", "name": "shell", "description": "Run a shell command", "parameters": { "type": "object" } }
            ]
        });

        let payload = build_chat_completions_payload(&raw, "MiniMax-M3").unwrap();
        let messages = payload["messages"].as_array().unwrap();

        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["role"], "user");
    }

    #[test]
    fn build_chat_completions_payload_drops_interrupted_tool_outputs() {
        let raw = json!({
            "model": "MiniMax-M3",
            "input": [
                { "type": "message", "role": "user", "content": [{"type": "input_text", "text": "run"}] },
                { "type": "function_call", "call_id": "call_1", "name": "shell", "arguments": "{\"command\":\"echo ok\"}" },
                { "type": "message", "role": "user", "content": [{"type": "input_text", "text": "next"}] },
                { "type": "function_call_output", "call_id": "call_1", "output": "ok" }
            ],
            "tools": [
                { "type": "function", "name": "shell", "description": "Run a shell command", "parameters": { "type": "object" } }
            ]
        });

        let payload = build_chat_completions_payload(&raw, "MiniMax-M3").unwrap();
        let messages = payload["messages"].as_array().unwrap();

        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["role"], "user");
        assert_eq!(messages[1]["role"], "user");
        assert!(messages.iter().all(|message| message["role"] != "tool"));
        assert!(messages
            .iter()
            .all(|message| message.get("tool_calls").is_none()));
    }

    #[test]
    fn openai_chat_content_passes_through_openai_image_url() {
        let value = json!([
            { "type": "text", "text": "describe" },
            { "type": "image_url", "image_url": { "url": "data:image/png;base64,AAAA" } }
        ]);
        let out = openai_chat_content(Some(&value)).unwrap();
        let arr = out.as_array().expect("multimodal content must be an array");
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["type"], "text");
        assert_eq!(arr[0]["text"], "describe");
        assert_eq!(arr[1]["type"], "image_url");
        assert_eq!(arr[1]["image_url"]["url"], "data:image/png;base64,AAAA");
    }

    #[test]
    fn openai_chat_content_normalizes_responses_input_image() {
        // Codex Responses API shape: input_image with image_url as a string
        let value = json!([
            { "type": "input_text", "text": "what is this?" },
            { "type": "input_image", "image_url": "data:image/jpeg;base64,BBBB" }
        ]);
        let out = openai_chat_content(Some(&value)).unwrap();
        let arr = out.as_array().expect("must be array");
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["type"], "text");
        assert_eq!(arr[0]["text"], "what is this?");
        assert_eq!(arr[1]["type"], "image_url");
        assert_eq!(arr[1]["image_url"]["url"], "data:image/jpeg;base64,BBBB");
    }

    #[test]
    fn openai_chat_content_collapses_text_only_to_string() {
        let value = json!([
            { "type": "input_text", "text": "hello " },
            { "type": "input_text", "text": "world" }
        ]);
        let out = openai_chat_content(Some(&value)).unwrap();
        assert_eq!(out, "hello \nworld");
    }

    #[test]
    fn openai_chat_content_handles_image_only_array() {
        let value = json!([
            { "type": "input_image", "image_url": "https://example.com/x.png" }
        ]);
        let out = openai_chat_content(Some(&value)).unwrap();
        let arr = out.as_array().expect("must be array");
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["type"], "image_url");
        assert_eq!(arr[0]["image_url"]["url"], "https://example.com/x.png");
    }

    #[test]
    fn build_chat_completions_payload_preserves_responses_input_image() {
        let raw = json!({
            "model": "MiniMax-Text-01",
            "input": [
                {
                    "type": "message",
                    "role": "user",
                    "content": [
                        { "type": "input_text", "text": "describe the screenshot" },
                        { "type": "input_image", "image_url": "data:image/png;base64,ZZZZ" }
                    ]
                }
            ]
        });
        let payload = build_chat_completions_payload(&raw, "MiniMax-Text-01").unwrap();
        let content = &payload["messages"][0]["content"];
        let arr = content
            .as_array()
            .expect("multimodal content must be an array");
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["type"], "text");
        assert_eq!(arr[0]["text"], "describe the screenshot");
        assert_eq!(arr[1]["type"], "image_url");
        assert_eq!(arr[1]["image_url"]["url"], "data:image/png;base64,ZZZZ");
    }

    #[test]
    fn build_chat_completions_payload_preserves_chat_message_image() {
        let raw = json!({
            "model": "MiniMax-M3",
            "messages": [
                {
                    "role": "user",
                    "content": [
                        { "type": "text", "text": "what is this?" },
                        { "type": "image_url", "image_url": { "url": "data:image/png;base64,CCCC" } }
                    ]
                }
            ]
        });
        let payload = build_chat_completions_payload(&raw, "MiniMax-M3").unwrap();
        let content = &payload["messages"][0]["content"];
        let arr = content
            .as_array()
            .expect("multimodal content must be an array");
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["text"], "what is this?");
        assert_eq!(arr[1]["image_url"]["url"], "data:image/png;base64,CCCC");
    }

    #[test]
    fn chat_completion_to_responses_maps_text_thinking_and_tool_use() {
        let chat = json!({
            "id": "chatcmpl-1",
            "created": 1700000000,
            "model": "MiniMax-Text-01",
            "choices": [{
                "message": {
                    "reasoning_content": "think",
                    "content": "hello",
                    "tool_calls": [{
                        "id": "call_1",
                        "function": { "name": "echo", "arguments": "{}" }
                    }]
                }
            }],
            "usage": { "prompt_tokens": 3, "completion_tokens": 4, "total_tokens": 7 }
        });
        let response = chat_completion_to_responses(&chat, "MiniMax-Text-01");
        let output = response["output"].as_array().unwrap();
        assert_eq!(output[0]["type"], "reasoning");
        assert_eq!(output[1]["type"], "message");
        assert_eq!(output[2]["type"], "function_call");
        assert_eq!(response["usage"]["input_tokens"], 3);
        assert_eq!(response["usage"]["output_tokens"], 4);
        assert_eq!(response["output_text"], "hello");
    }

    #[test]
    fn chat_completion_to_responses_moves_inline_thinking_out_of_output_text() {
        let chat = json!({
            "choices": [{
                "message": {
                    "content": "<think>\nThe user asked for a short greeting.\n</think>\nHi"
                }
            }],
            "usage": { "prompt_tokens": 3, "completion_tokens": 9, "total_tokens": 12 }
        });

        let response = chat_completion_to_responses(&chat, "MiniMax-M3");
        let output = response["output"].as_array().unwrap();
        assert_eq!(response["output_text"], "Hi");
        assert_eq!(output[0]["type"], "reasoning");
        assert_eq!(
            output[0]["summary"][0]["text"],
            "The user asked for a short greeting."
        );
        assert_eq!(output[1]["type"], "message");
        assert_eq!(output[1]["content"][0]["text"], "Hi");
    }

    #[test]
    fn minimax_sse_parser_handles_split_events() {
        let first = json!({
            "choices": [{ "delta": { "content": "H" } }]
        })
        .to_string();
        let second = json!({
            "choices": [{ "delta": { "content": "i" } }]
        })
        .to_string();
        let mut parser = MiniMaxSseParser::default();

        assert!(parser
            .push(format!("data: {}", first).as_bytes())
            .is_empty());
        let events = parser.push(format!("\n\ndata: {}\n\n", second).as_bytes());

        assert_eq!(events.len(), 2);
        assert!(events[0].contains("\"H\""));
        assert!(events[1].contains("\"i\""));
    }

    #[test]
    fn minimax_sse_parser_marks_terminal_error_events() {
        let mut parser = MiniMaxSseParser::default();
        parser.push(
            b"data: {\"type\":\"error\",\"error\":{\"message\":\"minimax upstream error\"}}\n\n",
        );
        assert_eq!(parser.terminal_error(), Some("minimax upstream error"));
    }

    #[test]
    fn minimax_stream_accumulator_builds_completed_response() {
        let mut accumulator = MiniMaxStreamAccumulator::new("MiniMax-M3".to_string());
        accumulator.absorb_chat_value(&json!({
            "id": "chatcmpl_1",
            "created": 1700000000,
            "choices": [{
                "delta": { "reasoning_content": "think first" }
            }]
        }));
        accumulator.absorb_chat_value(&json!({
            "choices": [{
                "delta": { "content": "Hi" }
            }],
            "usage": {
                "prompt_tokens": 3,
                "completion_tokens": 2,
                "total_tokens": 5
            }
        }));

        let response = accumulator.to_response();
        let stream_events = response_output_events(&response);
        let completed = response_sse_event(&json!({
            "type": "response.completed",
            "response": response.clone()
        }));
        let stream_text = stream_events
            .iter()
            .map(|event| String::from_utf8(event.to_vec()).unwrap())
            .collect::<Vec<_>>()
            .join("");
        let completed_text = String::from_utf8(completed.to_vec()).unwrap();
        let item_added = stream_text.find("response.output_item.added").unwrap();
        let text_delta = stream_text.find("response.output_text.delta").unwrap();
        let item_done = stream_text.rfind("response.output_item.done").unwrap();

        assert!(item_added < text_delta);
        assert!(text_delta < item_done);
        assert!(stream_text.contains("response.content_part.added"));
        assert!(stream_text.contains("\"Hi\""));
        assert!(completed_text.contains("response.completed"));
        assert_eq!(response["id"], "resp_chatcmpl_1");
        assert_eq!(response["output_text"], "Hi");
        assert_eq!(response["output"][0]["type"], "reasoning");
        assert_eq!(response["output"][1]["type"], "message");
        assert_eq!(response["usage"]["total_tokens"], 5);
    }
}
