use axum::{
    body::Body,
    extract::State,
    http::{HeaderMap, HeaderValue, StatusCode},
    response::IntoResponse,
};
use bytes::Bytes;
use futures_util::StreamExt;
use serde_json::{json, Map, Value};
use std::time::Duration;

use super::DEFAULT_ANTHROPIC_BASE_URL;

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
    format!("{}/v1/messages", base)
}

pub fn normalize_anthropic_base_url(base_url: Option<&str>) -> String {
    base_url
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(DEFAULT_ANTHROPIC_BASE_URL)
        .trim_end_matches('/')
        .to_string()
}

fn normalize_account_anthropic_base_url(account: &super::accounts::GlmAccount) -> String {
    if let Some(base_url) = account
        .anthropic_base_url
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        return normalize_anthropic_base_url(Some(base_url));
    }
    if let Some(base_url) = account
        .base_url
        .as_deref()
        .filter(|value| value.to_ascii_lowercase().contains("anthropic"))
    {
        return normalize_anthropic_base_url(Some(base_url));
    }
    normalize_anthropic_base_url(None)
}

fn upstream_root(base_url: &str) -> String {
    let mut base = normalize_anthropic_base_url(Some(base_url));
    for suffix in [
        "/v1/chat/completions",
        "/chat/completions",
        "/v1/responses",
        "/responses",
        "/v1/messages",
        "/messages",
        "/v1",
    ] {
        if let Some(stripped) = base.strip_suffix(suffix) {
            base = stripped.trim_end_matches('/').to_string();
            break;
        }
    }
    if base.is_empty() {
        DEFAULT_ANTHROPIC_BASE_URL.to_string()
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
            "No GLM accounts configured",
        );
    }

    let wants_stream = crate::source::wants_stream(&headers, &body);
    let prompt_metrics = crate::prompt_metrics_from_request_value(&raw);
    let mut last_error: Option<(StatusCode, String)> = None;

    for (attempt_idx, account) in accounts.iter().enumerate() {
        let context = crate::glm_usage_context(
            account,
            Some(model.clone()),
            "/glm/anthropic/v1/messages",
            prompt_metrics.clone(),
        );
        if !account.is_subscription() {
            let chat_payload = match anthropic_messages_to_chat_completions(&raw, &model) {
                Ok(payload) => payload,
                Err(err) => {
                    crate::record_request_pre_dispatch_error(&state, &context, err.as_str());
                    return anthropic_error(StatusCode::BAD_REQUEST, "invalid_request_error", &err);
                }
            };

            if let Err(response) = crate::reserve_api_key_budgets_for_prepared_dispatch(
                &state,
                context.provider_name,
                &context.key,
                &serde_json::to_vec(&chat_payload).unwrap_or_default(),
            )
            .await
            {
                return response;
            }
            crate::record_glm_request(&state, &context);

            match api_usage_messages_via_chat_completions(
                &state,
                account,
                &context,
                &chat_payload,
                &model,
                wants_stream,
            )
            .await
            {
                Ok(response) => return response,
                Err((status, message)) => {
                    crate::record_glm_error(&state, &context, &message);
                    if attempt_idx + 1 < accounts.len()
                        && crate::should_retry_account_error(status, &message)
                    {
                        last_error = Some((status, message));
                        continue;
                    }
                    return anthropic_error(
                        status,
                        if status.is_client_error() {
                            "invalid_request_error"
                        } else {
                            "api_error"
                        },
                        &message,
                    );
                }
            }
        }

        if let Err(response) = crate::reserve_api_key_budgets_for_prepared_dispatch(
            &state,
            context.provider_name,
            &context.key,
            &body,
        )
        .await
        {
            return response;
        }
        crate::record_glm_request(&state, &context);

        let base_url = normalize_account_anthropic_base_url(account);
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
                let message = format!("GLM Anthropic request failed: {}", err);
                crate::record_glm_error(&state, &context, &message);
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
                let message = format!("GLM Anthropic body read failed: {}", err);
                crate::record_glm_error(&state, &context, &message);
                last_error = Some((StatusCode::BAD_GATEWAY, message));
                if attempt_idx + 1 < accounts.len() {
                    continue;
                }
                break;
            }
        };

        if !status.is_success() {
            let message = format!(
                "GLM Anthropic returned {}: {}",
                status,
                String::from_utf8_lossy(&bytes)
            );
            crate::record_glm_error(&state, &context, &message);
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
        crate::record_glm_success(&state, &context, &usage);
        return (status, out_headers, bytes).into_response();
    }

    let (status, message) = last_error.unwrap_or_else(|| {
        (
            StatusCode::BAD_GATEWAY,
            "All GLM accounts failed".to_string(),
        )
    });
    anthropic_error(
        status,
        "api_error",
        &format!("All GLM accounts failed; last error: {}", message),
    )
}

async fn api_usage_messages_via_chat_completions(
    state: &crate::AppState,
    account: &super::accounts::GlmAccount,
    context: &crate::UsageContext,
    chat_payload: &Value,
    model: &str,
    wants_stream: bool,
) -> Result<axum::response::Response, (StatusCode, String)> {
    let base_url = account.openai_base_url();
    let resp = state
        .client
        .post(super::api::chat_completions_url(&base_url))
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
        .map_err(|err| {
            (
                StatusCode::BAD_GATEWAY,
                format!("GLM API usage chat request failed: {}", err),
            )
        })?;

    let status = resp.status();
    let bytes = resp.bytes().await.map_err(|err| {
        (
            StatusCode::BAD_GATEWAY,
            format!("GLM API usage chat body read failed: {}", err),
        )
    })?;

    if !status.is_success() {
        return Err((
            status,
            format!(
                "GLM API usage chat returned {}: {}",
                status,
                String::from_utf8_lossy(&bytes)
            ),
        ));
    }

    let chat = serde_json::from_slice::<Value>(&bytes).map_err(|err| {
        (
            StatusCode::BAD_GATEWAY,
            format!("invalid GLM API usage chat response: {}", err),
        )
    })?;
    let mut usage = crate::usage_metrics_from_response_value(&chat);
    crate::quota_usage::preserve_native_usage(&mut usage, &chat, "openai");
    let response = super::api::chat_completion_to_responses(&chat, model);
    let message = responses_to_anthropic_message(&response, model);
    crate::record_glm_success(state, context, &usage);

    if wants_stream {
        return Ok(anthropic_stream_response(&message));
    }

    let body = serde_json::to_vec(&message).unwrap_or_default();
    Ok((StatusCode::OK, [("Content-Type", "application/json")], body).into_response())
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
                        crate::record_glm_error(&usage_state, &usage_context, message);
                        lifecycle.finish();
                        yield Ok::<Bytes, std::io::Error>(bytes);
                        return;
                    }
                    yield Ok::<Bytes, std::io::Error>(bytes);
                }
                Err(err) => {
                    let message = format!("GLM Anthropic stream read failed: {}", err);
                    crate::record_glm_error(&usage_state, &usage_context, &message);
                    lifecycle.finish();
                    yield Err(std::io::Error::new(std::io::ErrorKind::Other, "stream"));
                    return;
                }
            }
        }
        match parser.finish() {
            Err(message) => crate::record_glm_error(&usage_state, &usage_context, &message),
            Ok(Some(usage)) => crate::record_glm_success(&usage_state, &usage_context, &usage),
            Ok(None) => crate::record_glm_success(
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

fn anthropic_messages_to_chat_completions(payload: &Value, model: &str) -> Result<Value, String> {
    let messages = payload
        .get("messages")
        .and_then(|value| value.as_array())
        .ok_or_else(|| "messages is required".to_string())?;

    let mut chat_messages = Vec::new();
    if let Some(system) = translate_anthropic_system(payload.get("system")) {
        chat_messages.push(json!({"role": "system", "content": system}));
    }
    for message in messages {
        append_anthropic_message_as_chat(&mut chat_messages, message)?;
    }
    if chat_messages.is_empty() {
        return Err("messages is required".to_string());
    }

    let mut out = Map::new();
    out.insert("model".to_string(), Value::String(model.to_string()));
    out.insert(
        "messages".to_string(),
        Value::Array(super::api::sanitize_chat_messages(chat_messages)),
    );
    if let Some(max_tokens) = payload.get("max_tokens") {
        out.insert("max_tokens".to_string(), max_tokens.clone());
    }
    copy_if_present(payload, &mut out, "temperature");
    copy_if_present(payload, &mut out, "top_p");
    if let Some(stop) = payload
        .get("stop_sequences")
        .or_else(|| payload.get("stop"))
    {
        out.insert("stop".to_string(), stop.clone());
    }
    if let Some(tools) = translate_anthropic_tools(payload.get("tools")) {
        out.insert("tools".to_string(), Value::Array(tools));
    }
    if let Some(tool_choice) = translate_anthropic_tool_choice(payload.get("tool_choice")) {
        out.insert("tool_choice".to_string(), tool_choice);
    }
    out.insert(
        "thinking".to_string(),
        translate_anthropic_thinking(payload.get("thinking")),
    );
    out.insert("stream".to_string(), Value::Bool(false));
    Ok(Value::Object(out))
}

fn append_anthropic_message_as_chat(
    messages: &mut Vec<Value>,
    message: &Value,
) -> Result<(), String> {
    let role = message
        .get("role")
        .and_then(|value| value.as_str())
        .ok_or_else(|| "message role is required".to_string())?;
    let content = message
        .get("content")
        .ok_or_else(|| "message content is required".to_string())?;

    match role {
        "user" => append_anthropic_user_content(messages, content),
        "assistant" => append_anthropic_assistant_content(messages, content),
        _ => Ok(()),
    }
}

fn append_anthropic_user_content(messages: &mut Vec<Value>, content: &Value) -> Result<(), String> {
    if let Some(text) = content.as_str() {
        if !text.trim().is_empty() {
            messages.push(json!({"role": "user", "content": text}));
        }
        return Ok(());
    }

    let Some(parts) = content.as_array() else {
        return Ok(());
    };
    let mut text_parts = Vec::new();
    let mut rich_parts = Vec::new();
    let mut has_image = false;

    for part in parts {
        match part.get("type").and_then(|value| value.as_str()) {
            Some("tool_result") => {
                let call_id = part
                    .get("tool_use_id")
                    .and_then(|value| value.as_str())
                    .unwrap_or("");
                if !call_id.is_empty() {
                    messages.push(json!({
                        "role": "tool",
                        "tool_call_id": call_id,
                        "content": stringify_tool_result(part.get("content"))
                    }));
                }
            }
            Some("text") => {
                if let Some(text) = part.get("text").and_then(|value| value.as_str()) {
                    if !text.trim().is_empty() {
                        text_parts.push(text.to_string());
                        rich_parts.push(json!({"type": "text", "text": text}));
                    }
                }
            }
            Some("image") => {
                if let Some(image) = translate_anthropic_image_to_chat(part) {
                    has_image = true;
                    rich_parts.push(image);
                }
            }
            _ => {}
        }
    }

    if !rich_parts.is_empty() {
        let content = if has_image {
            Value::Array(rich_parts)
        } else {
            Value::String(text_parts.join("\n\n"))
        };
        messages.push(json!({"role": "user", "content": content}));
    }
    Ok(())
}

fn append_anthropic_assistant_content(
    messages: &mut Vec<Value>,
    content: &Value,
) -> Result<(), String> {
    if let Some(text) = content.as_str() {
        if !text.trim().is_empty() {
            messages.push(json!({"role": "assistant", "content": text}));
        }
        return Ok(());
    }

    let Some(parts) = content.as_array() else {
        return Ok(());
    };
    let text = parts
        .iter()
        .filter(|part| part.get("type").and_then(|value| value.as_str()) == Some("text"))
        .filter_map(|part| part.get("text").and_then(|value| value.as_str()))
        .filter(|text| !text.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");
    let tool_calls = parts
        .iter()
        .filter(|part| part.get("type").and_then(|value| value.as_str()) == Some("tool_use"))
        .filter_map(|part| {
            let id = part.get("id").and_then(|value| value.as_str())?;
            let name = part.get("name").and_then(|value| value.as_str())?;
            let arguments = part
                .get("input")
                .cloned()
                .unwrap_or_else(|| json!({}))
                .to_string();
            Some(json!({
                "id": id,
                "type": "function",
                "function": {
                    "name": name,
                    "arguments": arguments
                }
            }))
        })
        .collect::<Vec<_>>();

    if !text.is_empty() || !tool_calls.is_empty() {
        let mut message = json!({
            "role": "assistant",
            "content": if text.is_empty() { Value::Null } else { Value::String(text) }
        });
        if !tool_calls.is_empty() {
            message["tool_calls"] = Value::Array(tool_calls);
        }
        messages.push(message);
    }
    Ok(())
}

fn translate_anthropic_system(value: Option<&Value>) -> Option<String> {
    match value {
        Some(Value::String(text)) => Some(text.clone()),
        Some(Value::Array(parts)) => {
            let joined = parts
                .iter()
                .filter_map(|part| part.get("text").and_then(|value| value.as_str()))
                .collect::<Vec<_>>()
                .join("\n\n");
            (!joined.is_empty()).then_some(joined)
        }
        _ => None,
    }
}

fn translate_anthropic_image_to_chat(part: &Value) -> Option<Value> {
    let source = part.get("source")?;
    match source.get("type").and_then(|value| value.as_str())? {
        "base64" => {
            let media_type = source.get("media_type").and_then(|value| value.as_str())?;
            let data = source.get("data").and_then(|value| value.as_str())?;
            Some(json!({
                "type": "image_url",
                "image_url": { "url": format!("data:{};base64,{}", media_type, data) }
            }))
        }
        "url" => {
            let url = source.get("url").and_then(|value| value.as_str())?;
            Some(json!({
                "type": "image_url",
                "image_url": { "url": url }
            }))
        }
        _ => None,
    }
}

fn stringify_tool_result(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|part| {
                if part.get("type").and_then(|value| value.as_str()) == Some("text") {
                    part.get("text")
                        .and_then(|value| value.as_str())
                        .map(str::to_string)
                } else {
                    Some(part.to_string())
                }
            })
            .collect::<Vec<_>>()
            .join("\n\n"),
        Some(value) => value.to_string(),
        None => String::new(),
    }
}

fn translate_anthropic_tools(value: Option<&Value>) -> Option<Vec<Value>> {
    let tools = value?.as_array()?;
    let out = tools
        .iter()
        .filter_map(|tool| {
            let name = tool.get("name").and_then(|value| value.as_str())?;
            Some(json!({
                "type": "function",
                "function": {
                    "name": name,
                    "description": tool.get("description").and_then(|value| value.as_str()).unwrap_or(""),
                    "parameters": tool.get("input_schema").cloned().unwrap_or_else(|| json!({ "type": "object" }))
                }
            }))
        })
        .collect::<Vec<_>>();
    (!out.is_empty()).then_some(out)
}

fn translate_anthropic_tool_choice(value: Option<&Value>) -> Option<Value> {
    let value = value?;
    match value.get("type").and_then(|value| value.as_str()) {
        Some("auto") => Some(Value::String("auto".to_string())),
        Some("none") => Some(Value::String("none".to_string())),
        Some("any") => Some(Value::String("required".to_string())),
        Some("tool") => value
            .get("name")
            .and_then(|name| name.as_str())
            .map(|name| json!({"type": "function", "function": { "name": name }})),
        _ => None,
    }
}

fn translate_anthropic_thinking(value: Option<&Value>) -> Value {
    match value
        .and_then(|value| value.get("type"))
        .and_then(|value| value.as_str())
    {
        Some("enabled") => json!({"type": "enabled"}),
        _ => json!({"type": "disabled"}),
    }
}

fn responses_to_anthropic_message(response: &Value, client_model: &str) -> Value {
    let content = response_to_anthropic_content(response);
    let has_tool = content
        .iter()
        .any(|block| block.get("type").and_then(|value| value.as_str()) == Some("tool_use"));
    let usage = response.get("usage").cloned().unwrap_or_else(|| json!({}));
    json!({
        "id": response.get("id").and_then(|value| value.as_str()).unwrap_or("msg_glm"),
        "type": "message",
        "role": "assistant",
        "content": content,
        "model": client_model,
        "stop_reason": if has_tool { "tool_use" } else { "end_turn" },
        "stop_sequence": Value::Null,
        "usage": {
            "input_tokens": usage.get("input_tokens").and_then(|value| value.as_u64()).unwrap_or(0),
            "output_tokens": usage.get("output_tokens").and_then(|value| value.as_u64()).unwrap_or(0)
        }
    })
}

fn response_to_anthropic_content(response: &Value) -> Vec<Value> {
    let mut content = Vec::new();
    if let Some(output) = response.get("output").and_then(|value| value.as_array()) {
        for item in output {
            match item.get("type").and_then(|value| value.as_str()) {
                Some("message") => {
                    if let Some(parts) = item.get("content").and_then(|value| value.as_array()) {
                        for part in parts {
                            if let Some(text) = part.get("text").and_then(|value| value.as_str()) {
                                if !text.is_empty() {
                                    content.push(json!({"type": "text", "text": text}));
                                }
                            }
                        }
                    }
                }
                Some("function_call") => {
                    let arguments = item
                        .get("arguments")
                        .and_then(|value| value.as_str())
                        .and_then(|value| serde_json::from_str::<Value>(value).ok())
                        .unwrap_or_else(|| json!({}));
                    content.push(json!({
                        "type": "tool_use",
                        "id": item.get("call_id").or_else(|| item.get("id")).and_then(|value| value.as_str()).unwrap_or("call_glm"),
                        "name": item.get("name").and_then(|value| value.as_str()).unwrap_or("tool"),
                        "input": arguments
                    }));
                }
                _ => {}
            }
        }
    }
    if content.is_empty() {
        if let Some(text) = response.get("output_text").and_then(|value| value.as_str()) {
            if !text.is_empty() {
                content.push(json!({"type": "text", "text": text}));
            }
        }
    }
    content
}

fn anthropic_stream_response(message: &Value) -> axum::response::Response {
    let events = anthropic_sse_events(message);
    let stream = async_stream::stream! {
        for event in events {
            yield Ok::<Bytes, std::io::Error>(Bytes::from(event));
        }
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

fn anthropic_sse_events(message: &Value) -> Vec<String> {
    let mut out = Vec::new();
    let mut start = message.clone();
    if let Some(object) = start.as_object_mut() {
        object.insert("content".to_string(), Value::Array(Vec::new()));
        object.insert("stop_reason".to_string(), Value::Null);
        object.insert("stop_sequence".to_string(), Value::Null);
    }
    out.push(sse_event(
        "message_start",
        &json!({
            "type": "message_start",
            "message": start
        }),
    ));

    if let Some(content) = message.get("content").and_then(|value| value.as_array()) {
        for (index, block) in content.iter().enumerate() {
            out.push(sse_event(
                "content_block_start",
                &json!({
                    "type": "content_block_start",
                    "index": index,
                    "content_block": if block.get("type").and_then(|value| value.as_str()) == Some("text") {
                        json!({"type":"text","text":""})
                    } else {
                        block.clone()
                    }
                }),
            ));
            if block.get("type").and_then(|value| value.as_str()) == Some("text") {
                out.push(sse_event(
                    "content_block_delta",
                    &json!({
                        "type": "content_block_delta",
                        "index": index,
                        "delta": {
                            "type": "text_delta",
                            "text": block.get("text").and_then(|value| value.as_str()).unwrap_or("")
                        }
                    }),
                ));
            }
            out.push(sse_event(
                "content_block_stop",
                &json!({
                    "type": "content_block_stop",
                    "index": index
                }),
            ));
        }
    }

    out.push(sse_event(
        "message_delta",
        &json!({
            "type": "message_delta",
            "delta": {
                "stop_reason": message.get("stop_reason").and_then(|value| value.as_str()).unwrap_or("end_turn"),
                "stop_sequence": Value::Null
            },
            "usage": {
                "output_tokens": message.get("usage").and_then(|usage| usage.get("output_tokens")).and_then(|value| value.as_u64()).unwrap_or(0)
            }
        }),
    ));
    out.push(sse_event("message_stop", &json!({"type": "message_stop"})));
    out
}

fn sse_event(event: &str, data: &Value) -> String {
    format!("event: {}\ndata: {}\n\n", event, data)
}

fn copy_if_present(input: &Value, output: &mut Map<String, Value>, key: &str) {
    if let Some(value) = input.get(key) {
        output.insert(key.to_string(), value.clone());
    }
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
            anthropic_messages_url("https://api.z.ai/api/anthropic"),
            "https://api.z.ai/api/anthropic/v1/messages"
        );
        assert_eq!(
            anthropic_messages_url("https://api.z.ai/api/anthropic/v1"),
            "https://api.z.ai/api/anthropic/v1/messages"
        );
        assert_eq!(
            anthropic_messages_url("https://api.z.ai/api/anthropic/v1/messages"),
            "https://api.z.ai/api/anthropic/v1/messages"
        );
        assert_eq!(
            anthropic_messages_url("https://custom.example/anthropic"),
            "https://custom.example/anthropic/v1/messages"
        );
    }

    #[test]
    fn anthropic_messages_url_converts_codex_endpoint_base() {
        assert_eq!(
            anthropic_messages_url("https://api.z.ai/api/anthropic/v1/messages"),
            "https://api.z.ai/api/anthropic/v1/messages"
        );
        assert_eq!(
            normalize_account_anthropic_base_url(&crate::target::glm::accounts::GlmAccount {
                base_url: Some("https://api.z.ai/api/coding/paas/v4".to_string()),
                ..Default::default()
            }),
            DEFAULT_ANTHROPIC_BASE_URL
        );
    }

    #[test]
    fn api_usage_accounts_translate_anthropic_messages_to_chat() {
        let payload = json!({
            "model": "glm-5.2",
            "system": "Be brief.",
            "max_tokens": 64,
            "messages": [
                {"role": "user", "content": "Say hi"},
                {
                    "role": "assistant",
                    "content": [
                        {"type": "text", "text": "Need tool"},
                        {"type": "tool_use", "id": "toolu_1", "name": "lookup", "input": {"q": "hi"}}
                    ]
                },
                {
                    "role": "user",
                    "content": [
                        {"type": "tool_result", "tool_use_id": "toolu_1", "content": "ok"}
                    ]
                }
            ],
            "tools": [
                {"name": "lookup", "description": "Lookup", "input_schema": {"type": "object"}}
            ]
        });

        let out = anthropic_messages_to_chat_completions(&payload, "glm-5.2").unwrap();
        assert_eq!(out["model"], "glm-5.2");
        assert_eq!(out["max_tokens"], 64);
        assert_eq!(out["messages"][0]["role"], "system");
        assert_eq!(out["messages"][1]["role"], "user");
        assert_eq!(out["messages"][2]["tool_calls"][0]["id"], "toolu_1");
        assert_eq!(out["messages"][3]["role"], "tool");
        assert_eq!(out["tools"][0]["function"]["name"], "lookup");
        assert_eq!(out["thinking"]["type"], "disabled");
    }

    #[test]
    fn anthropic_sse_tracker_rejects_terminal_error_events() {
        let mut tracker = AnthropicSseUsageTracker::default();
        tracker.push(&Bytes::from_static(
            b"event: error\ndata: {\"error\":{\"message\":\"glm quota\"}}\n\n",
        ));
        assert!(matches!(
            tracker.finish(),
            Err(message) if message == "glm quota"
        ));
    }
}
