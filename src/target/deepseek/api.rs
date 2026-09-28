use crate::source::v1::multimodal::{classify_content, is_data_url, split_data_url, PartKind};
use axum::{
    body::Body,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
};
use bytes::Bytes;
use serde_json::{json, Value};
use std::time::Duration;
use uuid::Uuid;

const DEFAULT_BASE_URL: &str = "https://api.deepseek.com";
const MODEL_FALLBACKS: &[(&str, &str)] = &[
    ("deepseek-v4-flash", "DeepSeek V4 Flash"),
    ("deepseek-v4-pro", "DeepSeek V4 Pro"),
    ("deepseek-chat", "DeepSeek Chat"),
    ("deepseek-reasoner", "DeepSeek Reasoner"),
];

pub fn normalize_base_url(base_url: Option<&str>) -> String {
    base_url
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(DEFAULT_BASE_URL)
        .trim_end_matches('/')
        .to_string()
}

pub async fn validate_api_key(
    client: &reqwest::Client,
    api_key: &str,
    base_url: &str,
) -> Result<(), String> {
    fetch_models_json(client, api_key, base_url)
        .await
        .map(|_| ())
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
                    "No DeepSeek accounts configured",
                    "server_error",
                    None,
                ),
            )
                .into_response();
        }
    };

    match fetch_models_json(
        &state.client,
        &account.api_key,
        &normalize_base_url(account.base_url.as_deref()),
    )
    .await
    {
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
            let data = MODEL_FALLBACKS
                .iter()
                .map(|(id, label)| {
                    json!({
                        "id": id,
                        "object": "model",
                        "created": 0,
                        "owned_by": "deepseek",
                        "display_name": label
                    })
                })
                .collect::<Vec<_>>();
            let body = serde_json::to_vec(&json!({
                "object": "list",
                "data": data,
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

    let request_value: serde_json::Value = match serde_json::from_slice(&body) {
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

    let model = match request_value.get("model").and_then(|v| v.as_str()) {
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

    let payload = match build_anthropic_payload(&request_value, &model) {
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

    let accounts = super::accounts::candidate_accounts(&state);
    if accounts.is_empty() {
        crate::release_api_key_budget_before_dispatch(&state);
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            [("Content-Type", "application/json")],
            crate::source::v1::response::openai_error_body(
                "No DeepSeek accounts configured",
                "server_error",
                None,
            ),
        )
            .into_response();
    }

    let wants_stream = crate::source::wants_stream(&headers, &body);
    let prompt_metrics = crate::prompt_metrics_from_request_value(&request_value);
    let mut last_error: Option<(StatusCode, String)> = None;

    for (attempt_idx, account) in accounts.iter().enumerate() {
        let context = crate::deepseek_usage_context(
            account,
            Some(model.clone()),
            "/deepseek/v1/responses",
            prompt_metrics.clone(),
        );
        if let Err(response) = crate::reserve_api_key_budgets_for_prepared_dispatch(
            &state,
            context.provider_name,
            &context.key,
            &serde_json::to_vec(&payload).unwrap_or_default(),
        )
        .await
        {
            return response;
        }
        crate::record_deepseek_request(&state, &context);

        let upstream = match send_anthropic_request(
            &state.client,
            &account.api_key,
            &normalize_base_url(account.base_url.as_deref()),
            &payload,
        )
        .await
        {
            Ok(value) => value,
            Err((status, message)) => {
                crate::record_deepseek_error(&state, &context, &message);
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
        };

        let response = anthropic_to_openai_response(&upstream, &model);
        let mut usage = crate::usage_metrics_from_response_value(&response);
        crate::quota_usage::preserve_native_usage(&mut usage, &upstream, "anthropic");
        crate::record_deepseek_success(&state, &context, &usage);

        if wants_stream {
            return (
                StatusCode::OK,
                [
                    ("Content-Type", "text/event-stream"),
                    ("Cache-Control", "no-store"),
                ],
                Body::from(render_response_sse(&response)),
            )
                .into_response();
        }

        let body = serde_json::to_vec(&response).unwrap_or_default();
        return (StatusCode::OK, [("Content-Type", "application/json")], body).into_response();
    }

    let (status, message) = last_error.unwrap_or_else(|| {
        (
            StatusCode::BAD_GATEWAY,
            "All DeepSeek accounts failed".to_string(),
        )
    });
    (
        status,
        [("Content-Type", "application/json")],
        crate::source::v1::response::openai_error_body(
            &format!("All DeepSeek accounts failed; last error: {}", message),
            "server_error",
            None,
        ),
    )
        .into_response()
}

async fn fetch_models_json(
    client: &reqwest::Client,
    api_key: &str,
    base_url: &str,
) -> Result<serde_json::Value, String> {
    let resp = client
        .get(format!("{}/models", normalize_base_url(Some(base_url))))
        .header("Authorization", format!("Bearer {}", api_key.trim()))
        .header("Accept", "application/json")
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .map_err(|e| format!("DeepSeek models request failed: {}", e))?;

    let status = resp.status();
    let text = resp.text().await.map_err(|e| e.to_string())?;
    if !status.is_success() {
        return Err(format!("DeepSeek models returned {}: {}", status, text));
    }

    serde_json::from_str(&text).map_err(|e| format!("invalid DeepSeek models JSON: {}", e))
}

fn models_to_openai_json(value: &serde_json::Value) -> Result<Vec<u8>, String> {
    let Some(models) = value.get("data").and_then(|v| v.as_array()) else {
        return Err("DeepSeek models response was missing data".to_string());
    };

    let data = models
        .iter()
        .filter_map(|model| {
            let id = model.get("id").and_then(|v| v.as_str())?;
            Some(json!({
                "id": id,
                "object": "model",
                "created": 0,
                "owned_by": model.get("owned_by").and_then(|v| v.as_str()).unwrap_or("deepseek")
            }))
        })
        .collect::<Vec<_>>();

    serde_json::to_vec(&json!({
        "object": "list",
        "data": data
    }))
    .map_err(|e| e.to_string())
}

fn anthropic_messages_url(base_url: &str) -> String {
    let base = normalize_base_url(Some(base_url));
    if base.ends_with("/v1/messages") || base.ends_with("/messages") {
        return base;
    }
    if base.ends_with("/anthropic/v1") {
        return format!("{}/messages", base);
    }
    if base.ends_with("/anthropic") {
        return format!("{}/v1/messages", base);
    }
    if let Some(root) = base.strip_suffix("/v1") {
        return format!("{}/anthropic/v1/messages", root);
    }
    format!("{}/anthropic/v1/messages", base)
}

async fn send_anthropic_request(
    client: &reqwest::Client,
    api_key: &str,
    base_url: &str,
    payload: &serde_json::Value,
) -> Result<serde_json::Value, (StatusCode, String)> {
    let resp = client
        .post(anthropic_messages_url(base_url))
        .header("x-api-key", api_key.trim())
        .header("Authorization", format!("Bearer {}", api_key.trim()))
        .header("anthropic-version", "2023-06-01")
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .body(payload.to_string())
        .timeout(Duration::from_secs(180))
        .send()
        .await
        .map_err(|e| {
            (
                StatusCode::BAD_GATEWAY,
                format!("DeepSeek request failed: {}", e),
            )
        })?;

    let status = resp.status();
    let text = resp
        .text()
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, e.to_string()))?;
    if !status.is_success() {
        return Err((
            StatusCode::BAD_GATEWAY,
            format!("DeepSeek returned {}: {}", status, text),
        ));
    }

    serde_json::from_str(&text).map_err(|e| {
        (
            StatusCode::BAD_GATEWAY,
            format!("failed to parse DeepSeek response: {}", e),
        )
    })
}

fn build_anthropic_payload(
    request_value: &serde_json::Value,
    model: &str,
) -> Result<serde_json::Value, String> {
    let direct_messages = request_value.get("messages").and_then(|v| v.as_array());
    let chat_messages = if let Some(messages) = direct_messages {
        normalize_chat_messages(messages)?
    } else {
        build_messages_from_input(request_value)?
    };

    let mut system_parts = Vec::new();
    if direct_messages.is_some() {
        if let Some(instructions) = request_value
            .get("instructions")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            system_parts.push(instructions.to_string());
        }
    }

    let disable_thinking_for_tool_history =
        has_assistant_tool_calls_without_reasoning(&chat_messages)
            && request_value.get("thinking").is_none();
    let messages = chat_messages_to_anthropic(chat_messages, &mut system_parts);
    if messages.is_empty() {
        return Err("messages must not be empty".to_string());
    }

    let max_tokens = request_value
        .get("max_output_tokens")
        .or_else(|| request_value.get("max_tokens"))
        .and_then(|v| v.as_u64())
        .unwrap_or(4096);

    let mut payload = json!({
        "model": model,
        "max_tokens": max_tokens,
        "messages": messages,
        "stream": false
    });

    if !system_parts.is_empty() {
        payload["system"] = json!(system_parts.join("\n"));
    }
    if let Some(tools) = build_anthropic_tools(request_value)? {
        payload["tools"] = tools;
        if let Some(tool_choice) = build_anthropic_tool_choice(request_value) {
            payload["tool_choice"] = tool_choice;
        }
    }
    if let Some(temperature) = request_value.get("temperature").and_then(|v| v.as_f64()) {
        payload["temperature"] = json!(temperature);
    }
    if let Some(top_p) = request_value.get("top_p").and_then(|v| v.as_f64()) {
        payload["top_p"] = json!(top_p);
    }
    if let Some(stop) = build_stop_sequences(request_value.get("stop")) {
        payload["stop_sequences"] = stop;
    }
    if !disable_thinking_for_tool_history {
        if let Some(effort) = build_reasoning_effort(request_value) {
            payload["output_config"] = json!({ "effort": effort });
        }
    }
    if let Some(thinking) = request_value.get("thinking") {
        payload["thinking"] = thinking.clone();
    } else if disable_thinking_for_tool_history {
        payload["thinking"] = json!({ "type": "disabled" });
    }
    if let Some(metadata) = build_anthropic_metadata(request_value) {
        payload["metadata"] = metadata;
    }

    Ok(payload)
}

fn chat_messages_to_anthropic(
    messages: Vec<serde_json::Value>,
    system_parts: &mut Vec<String>,
) -> Vec<serde_json::Value> {
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
                if let Some(tool_calls) = message.get("tool_calls").and_then(|v| v.as_array()) {
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
                    for block in anthropic_content_blocks(&message["content"]) {
                        content.push(block);
                    }
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
                    let blocks = anthropic_content_blocks(&message["content"]);
                    if !blocks.is_empty() {
                        out.push(json!({
                            "role": "assistant",
                            "content": blocks
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

fn has_assistant_tool_calls_without_reasoning(messages: &[serde_json::Value]) -> bool {
    messages.iter().any(|message| {
        message_role(message) == Some("assistant")
            && message
                .get("tool_calls")
                .and_then(|value| value.as_array())
                .map(|tool_calls| !tool_calls.is_empty())
                .unwrap_or(false)
            && assistant_reasoning_text(message).is_none()
    })
}

fn assistant_reasoning_text(message: &serde_json::Value) -> Option<String> {
    message
        .get("reasoning_content")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn tool_call_to_anthropic(tool_call: &serde_json::Value, id: &str) -> Option<serde_json::Value> {
    let function = tool_call.get("function")?;
    let name = function.get("name").and_then(|v| v.as_str())?;
    let input = function
        .get("arguments")
        .and_then(|v| v.as_str())
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

fn tool_message_to_anthropic_result(tool: &serde_json::Value) -> Option<serde_json::Value> {
    let tool_use_id = tool.get("tool_call_id").and_then(|value| value.as_str())?;
    let content = stringify_tool_output(tool.get("content"));
    Some(json!({
        "type": "tool_result",
        "tool_use_id": tool_use_id,
        "content": content
    }))
}

fn tool_result_context_text(tool: &serde_json::Value) -> Option<String> {
    let tool_call_id = tool
        .get("tool_call_id")
        .and_then(|value| value.as_str())
        .unwrap_or("unknown");
    let content = stringify_tool_output(tool.get("content"));
    if content.trim().is_empty() {
        None
    } else {
        Some(format!("Tool result for {}:\n{}", tool_call_id, content))
    }
}

fn build_anthropic_tools(
    request_value: &serde_json::Value,
) -> Result<Option<serde_json::Value>, String> {
    let Some(tools) = request_value.get("tools").and_then(|v| v.as_array()) else {
        return Ok(None);
    };

    let mut mapped = Vec::new();
    for tool in tools {
        if tool.get("type").and_then(|v| v.as_str()) != Some("function") {
            continue;
        }
        let function = tool.get("function").unwrap_or(tool);
        let Some(name) = function.get("name").and_then(|v| v.as_str()) else {
            return Err("tool.name is required".to_string());
        };
        let mut mapped_tool = json!({
            "name": name,
            "input_schema": function
                .get("parameters")
                .cloned()
                .unwrap_or_else(|| json!({ "type": "object" }))
        });
        if let Some(description) = function.get("description").and_then(|v| v.as_str()) {
            mapped_tool["description"] = json!(description);
        }
        mapped.push(mapped_tool);
    }

    if mapped.is_empty() {
        Ok(None)
    } else {
        Ok(Some(serde_json::Value::Array(mapped)))
    }
}

fn build_anthropic_tool_choice(request_value: &serde_json::Value) -> Option<serde_json::Value> {
    let choice = request_value.get("tool_choice")?;
    if let Some(choice) = choice.as_str() {
        return match choice {
            "none" => Some(json!({ "type": "none" })),
            "required" | "any" => Some(json!({ "type": "any" })),
            "auto" => Some(json!({ "type": "auto" })),
            _ => None,
        };
    }

    let choice_type = choice.get("type").and_then(|v| v.as_str())?;
    match choice_type {
        "none" => Some(json!({ "type": "none" })),
        "required" | "any" => Some(json!({ "type": "any" })),
        "auto" => Some(json!({ "type": "auto" })),
        "function" => choice
            .get("function")
            .and_then(|function| function.get("name"))
            .and_then(|name| name.as_str())
            .map(|name| json!({ "type": "tool", "name": name })),
        "tool" => choice
            .get("name")
            .and_then(|name| name.as_str())
            .map(|name| json!({ "type": "tool", "name": name })),
        _ => None,
    }
}

fn build_stop_sequences(stop: Option<&serde_json::Value>) -> Option<serde_json::Value> {
    let stop = stop?;
    if let Some(stop) = stop
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return Some(json!([stop]));
    }
    if let Some(items) = stop.as_array() {
        let values = items
            .iter()
            .filter_map(|item| item.as_str())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(|value| json!(value))
            .collect::<Vec<_>>();
        if !values.is_empty() {
            return Some(serde_json::Value::Array(values));
        }
    }
    None
}

fn build_anthropic_metadata(request_value: &serde_json::Value) -> Option<serde_json::Value> {
    request_value
        .get("metadata")
        .and_then(|metadata| metadata.get("user_id"))
        .and_then(|value| value.as_str())
        .or_else(|| request_value.get("user").and_then(|value| value.as_str()))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|user_id| json!({ "user_id": user_id }))
}

fn build_reasoning_effort(request_value: &serde_json::Value) -> Option<&'static str> {
    request_value
        .get("reasoning")
        .and_then(|reasoning| reasoning.get("effort"))
        .and_then(|v| v.as_str())
        .or_else(|| {
            request_value
                .get("reasoning_effort")
                .and_then(|v| v.as_str())
        })
        .and_then(map_reasoning_effort)
}

fn map_reasoning_effort(effort: &str) -> Option<&'static str> {
    match effort.trim().to_ascii_lowercase().as_str() {
        "low" | "medium" | "high" => Some("high"),
        "xhigh" | "max" => Some("max"),
        _ => None,
    }
}

fn normalize_chat_messages(
    messages: &[serde_json::Value],
) -> Result<Vec<serde_json::Value>, String> {
    let mut out = Vec::new();
    for message in messages {
        let role = normalize_message_role(message.get("role").and_then(|v| v.as_str()));
        // Preserve the original content shape (string or array) so the
        // downstream Anthropic-format builder can produce text + image
        // blocks. The string form is left untouched for plain-text
        // messages; the array form is forwarded as-is.
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
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|value| !value.is_empty())
            {
                normalized["reasoning_content"] = json!(reasoning_content);
            }
            if let Some(tool_calls) = message.get("tool_calls").and_then(|v| v.as_array()) {
                normalized["tool_calls"] =
                    serde_json::Value::Array(normalize_direct_tool_calls(tool_calls)?);
            }
        } else if role == "tool" {
            let tool_call_id = message
                .get("tool_call_id")
                .and_then(|v| v.as_str())
                .ok_or_else(|| "tool messages require tool_call_id".to_string())?;
            normalized["tool_call_id"] = json!(tool_call_id);
        }

        // Skip messages that have no usable content.
        let has_content = match normalized.get("content") {
            Some(Value::String(s)) => !s.trim().is_empty(),
            Some(Value::Array(a)) => !a.is_empty(),
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
        "latest_reminder" => "latest_reminder",
        _ => "user",
    }
}

fn tool_call_id(tool_call: &serde_json::Value) -> Option<String> {
    tool_call
        .get("id")
        .and_then(|value| value.as_str())
        .map(str::to_string)
}

fn message_role(message: &serde_json::Value) -> Option<&str> {
    message.get("role").and_then(|value| value.as_str())
}

fn normalize_direct_tool_calls(
    tool_calls: &[serde_json::Value],
) -> Result<Vec<serde_json::Value>, String> {
    let mut normalized = Vec::new();
    for tool_call in tool_calls {
        let tool_type = tool_call
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("function");
        let function = tool_call
            .get("function")
            .ok_or_else(|| "assistant tool_calls require function payloads".to_string())?;
        let name = function
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "assistant tool_calls require function.name".to_string())?;
        let arguments = function
            .get("arguments")
            .and_then(|v| v.as_str())
            .unwrap_or("{}");
        let tool_call_id = tool_call
            .get("id")
            .and_then(|v| v.as_str())
            .map(|value| value.to_string())
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

fn build_messages_from_input(
    request_value: &serde_json::Value,
) -> Result<Vec<serde_json::Value>, String> {
    let mut messages = Vec::new();

    if let Some(instructions) = request_value
        .get("instructions")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        messages.push(json!({
            "role": "system",
            "content": instructions
        }));
    }

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
        return Err(
            "only string or array input is supported for /deepseek/v1/responses".to_string(),
        );
    };

    let mut pending = PendingAssistantTurn::default();

    for item in items {
        let item_type = item.get("type").and_then(|v| v.as_str()).unwrap_or("");
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
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| "function_call items require call_id".to_string())?;
                let name = item
                    .get("name")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| "function_call items require name".to_string())?;
                let arguments = item
                    .get("arguments")
                    .and_then(|v| v.as_str())
                    .unwrap_or("{}");
                if let Some(reasoning_content) = item
                    .get("reasoning_content")
                    .and_then(|v| v.as_str())
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
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| "function_call_output items require call_id".to_string())?;
                messages.push(json!({
                    "role": "tool",
                    "tool_call_id": call_id,
                    "content": stringify_tool_output(item.get("output").or_else(|| item.get("content")))
                }));
            }
            _ => {
                let role = normalize_message_role(item.get("role").and_then(|v| v.as_str()));
                let blocks = anthropic_content_blocks(item);
                let direct_tool_calls = item.get("tool_calls").and_then(|v| v.as_array());

                if role == "assistant" && direct_tool_calls.is_some() {
                    flush_pending_assistant_turn(&mut messages, &mut pending);
                    let mut assistant = json!({
                        "role": "assistant",
                        "content": if blocks.is_empty() { json!("") } else { json!(blocks) },
                        "tool_calls": serde_json::Value::Array(
                            normalize_direct_tool_calls(direct_tool_calls.unwrap())?
                        )
                    });
                    if let Some(reasoning_content) = item
                        .get("reasoning_content")
                        .and_then(|v| v.as_str())
                        .map(str::trim)
                        .filter(|value| !value.is_empty())
                    {
                        assistant["reasoning_content"] = json!(reasoning_content);
                    }
                    messages.push(assistant);
                    continue;
                }

                if role == "assistant" {
                    // The pending assistant turn only carries text; image
                    // blocks are intentionally dropped here because the
                    // OpenAI Responses API -> Anthropic adapter doesn't
                    // support them on assistant turns.
                    let text = blocks
                        .iter()
                        .filter_map(|b| {
                            if b.get("type").and_then(|v| v.as_str()) == Some("text") {
                                b.get("text").and_then(|v| v.as_str()).map(str::to_string)
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
                    let Some(tool_call_id) = item.get("tool_call_id").and_then(|v| v.as_str())
                    else {
                        return Err("tool items require tool_call_id".to_string());
                    };
                    // Tool result content must remain a string (or array
                    // of blocks). We forward text-only content as-is.
                    let tool_text = blocks
                        .iter()
                        .filter_map(|b| {
                            if b.get("type").and_then(|v| v.as_str()) == Some("text") {
                                b.get("text").and_then(|v| v.as_str()).map(str::to_string)
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
    tool_calls: Vec<serde_json::Value>,
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

fn flush_pending_assistant_turn(
    messages: &mut Vec<serde_json::Value>,
    pending: &mut PendingAssistantTurn,
) {
    if pending.tool_calls.is_empty() && pending.content.is_none() {
        pending.reasoning_content = None;
        return;
    }

    let mut assistant = json!({
        "role": "assistant",
        "content": pending.content.take().unwrap_or_default()
    });
    if !pending.tool_calls.is_empty() {
        assistant["tool_calls"] = serde_json::Value::Array(std::mem::take(&mut pending.tool_calls));
    }
    if let Some(reasoning_content) = pending.reasoning_content.take() {
        assistant["reasoning_content"] = json!(reasoning_content);
    }
    messages.push(assistant);
}

fn extract_text_value(value: Option<&serde_json::Value>) -> Option<String> {
    let value = value?;
    if let Some(content) = value.as_str() {
        let content = content.trim();
        if !content.is_empty() {
            return Some(content.to_string());
        }
    }

    if let Some(content) = value.get("content").and_then(|v| v.as_str()) {
        let content = content.trim();
        if !content.is_empty() {
            return Some(content.to_string());
        }
    }

    let parts = value
        .get("content")
        .and_then(|v| v.as_array())
        .or_else(|| value.as_array())?;
    let mut out = String::new();
    for part in parts {
        let part_type = part.get("type").and_then(|v| v.as_str()).unwrap_or("");
        let text = part
            .get("text")
            .and_then(|v| v.as_str())
            .or_else(|| part.get("input_text").and_then(|v| v.as_str()))
            .or_else(|| part.get("output_text").and_then(|v| v.as_str()))
            .or_else(|| part.get("content").and_then(|v| v.as_str()));
        if matches!(
            part_type,
            "" | "text" | "input_text" | "output_text" | "summary_text"
        ) {
            if let Some(text) = text.map(str::trim).filter(|value| !value.is_empty()) {
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

/// Build an Anthropic `content` array of blocks from a Codex/OpenAI
/// content value. Returns an empty vector if the value carries no usable
/// content (caller should typically skip the message in that case).
///
/// Supports text parts (`{type: "text"|"input_text"|"output_text", text}`)
/// and image parts in three shapes:
///   - `{type: "image_url", image_url: {url: "data:..." | "https://..."}}`
///   - `{type: "input_image", image_url: "data:..." | "https://..."}`
///   - `{image_url: "..."}` (no type field)
fn anthropic_content_blocks(value: &Value) -> Vec<Value> {
    // If the value is a message-shaped object (Responses API item with
    // `role` and `content` keys), classify the content field. Otherwise
    // classify the value itself.
    let target = if value.is_object() && value.get("content").is_some() {
        value.get("content").unwrap()
    } else {
        value
    };
    // Pass-through: if the value is already an array of Anthropic-shaped
    // blocks (e.g. produced by a prior pass through this function or
    // forwarded as-is by normalize_chat_messages), keep it as-is.
    if let Some(arr) = target.as_array() {
        let all_blocks = arr.iter().all(|item| {
            item.is_object()
                && matches!(
                    item.get("type").and_then(|v| v.as_str()),
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
            PartKind::Text(text) => {
                blocks.push(json!({ "type": "text", "text": text }));
            }
            PartKind::Image(url) => {
                if let Some(block) = anthropic_image_block(&url) {
                    blocks.push(block);
                }
            }
            PartKind::Other(_) => {
                // Unknown part types are skipped; Anthropic has a fixed
                // block vocabulary and we only forward shapes we know.
            }
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
                "data": payload,
            }
        }));
    }
    if url.starts_with("http://") || url.starts_with("https://") {
        return Some(json!({
            "type": "image",
            "source": {
                "type": "url",
                "url": url,
            }
        }));
    }
    None
}

fn extract_reasoning_text(item: &serde_json::Value) -> Option<String> {
    if let Some(reasoning_content) = item
        .get("reasoning_content")
        .and_then(|v| v.as_str())
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

fn stringify_tool_output(value: Option<&serde_json::Value>) -> String {
    let Some(value) = value else {
        return String::new();
    };
    if let Some(text) = value.as_str() {
        return text.to_string();
    }
    serde_json::to_string(value).unwrap_or_else(|_| String::new())
}

fn anthropic_to_openai_response(value: &serde_json::Value, model: &str) -> serde_json::Value {
    let mut output = Vec::new();
    let mut output_text_parts = Vec::new();
    let mut pending_text = String::new();

    let flush_text = |output: &mut Vec<serde_json::Value>, pending_text: &mut String| {
        if pending_text.trim().is_empty() {
            pending_text.clear();
            return;
        }
        let text = std::mem::take(pending_text);
        output.push(json!({
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
    };

    if let Some(content) = value.get("content").and_then(|v| v.as_array()) {
        for block in content {
            match block.get("type").and_then(|v| v.as_str()).unwrap_or("") {
                "thinking" => {
                    let thinking = block
                        .get("thinking")
                        .or_else(|| block.get("text"))
                        .and_then(|v| v.as_str())
                        .map(str::trim)
                        .filter(|value| !value.is_empty());
                    if let Some(thinking) = thinking {
                        output.push(json!({
                            "id": format!("rs_{}", Uuid::new_v4().simple()),
                            "type": "reasoning",
                            "summary": [{
                                "type": "summary_text",
                                "text": thinking
                            }],
                            "content": thinking
                        }));
                    }
                }
                "text" => {
                    if let Some(text) = block
                        .get("text")
                        .and_then(|v| v.as_str())
                        .filter(|value| !value.is_empty())
                    {
                        if !pending_text.is_empty() {
                            pending_text.push('\n');
                        }
                        pending_text.push_str(text);
                        output_text_parts.push(text.to_string());
                    }
                }
                "tool_use" => {
                    flush_text(&mut output, &mut pending_text);
                    let call_id = block
                        .get("id")
                        .and_then(|v| v.as_str())
                        .map(|value| value.to_string())
                        .unwrap_or_else(|| format!("call_{}", Uuid::new_v4().simple()));
                    let name = block.get("name").and_then(|v| v.as_str()).unwrap_or("tool");
                    let arguments = block
                        .get("input")
                        .map(|value| {
                            serde_json::to_string(value).unwrap_or_else(|_| "{}".to_string())
                        })
                        .unwrap_or_else(|| "{}".to_string());
                    output.push(json!({
                        "id": format!("fc_{}", Uuid::new_v4().simple()),
                        "type": "function_call",
                        "call_id": call_id,
                        "name": name,
                        "arguments": arguments,
                        "status": "completed"
                    }));
                }
                _ => {}
            }
        }
    }
    flush_text(&mut output, &mut pending_text);

    let output_text = output_text_parts.join("\n");
    if output.is_empty() {
        output.push(json!({
            "type": "message",
            "id": format!("msg_{}", Uuid::new_v4().simple()),
            "status": "completed",
            "role": "assistant",
            "content": [{
                "type": "output_text",
                "text": output_text,
                "annotations": []
            }]
        }));
    }

    let usage = value.get("usage").cloned().unwrap_or_default();
    let input_tokens = usage
        .get("input_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let output_tokens = usage
        .get("output_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let cache_read_tokens = usage
        .get("cache_read_input_tokens")
        .and_then(|v| v.as_u64())
        .or_else(|| {
            usage
                .get("input_tokens_details")
                .and_then(|v| v.get("cached_tokens"))
                .and_then(|v| v.as_u64())
        })
        .unwrap_or(0);
    let stop_reason = value
        .get("stop_reason")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    json!({
        "id": format!(
            "resp_{}",
            value
                .get("id")
                .and_then(|v| v.as_str())
                .map(|value| value.to_string())
                .unwrap_or_else(|| Uuid::new_v4().simple().to_string())
        ),
        "object": "response",
        "created_at": chrono::Utc::now().timestamp(),
        "status": if stop_reason == "max_tokens" { "incomplete" } else { "completed" },
        "model": value
            .get("model")
            .and_then(|v| v.as_str())
            .unwrap_or(model),
        "output": output,
        "output_text": output_text,
        "usage": {
            "input_tokens": input_tokens,
            "output_tokens": output_tokens,
            "total_tokens": input_tokens + output_tokens,
            "cache_creation_input_tokens": usage.get("cache_creation_input_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
            "cache_read_input_tokens": cache_read_tokens,
            "input_tokens_details": {
                "cached_tokens": cache_read_tokens
            },
            "output_tokens_details": {
                "reasoning_tokens": usage.get("reasoning_tokens").and_then(|v| v.as_u64()).unwrap_or(0)
            }
        }
    })
}

fn render_response_sse(response: &serde_json::Value) -> Vec<u8> {
    let mut chunks = Vec::new();

    let mut created = response.clone();
    if let Some(object) = created.as_object_mut() {
        object.insert("status".to_string(), json!("in_progress"));
    }
    chunks.extend_from_slice(
        sse_json(&json!({
            "type": "response.created",
            "response": created
        }))
        .as_slice(),
    );

    if let Some(output) = response.get("output").and_then(|v| v.as_array()) {
        for (index, item) in output.iter().enumerate() {
            chunks.extend_from_slice(
                sse_json(&json!({
                    "type": "response.output_item.done",
                    "output_index": index,
                    "item": item
                }))
                .as_slice(),
            );
        }
    }

    if let Some(text) = response.get("output_text").and_then(|v| v.as_str()) {
        if !text.is_empty() {
            chunks.extend_from_slice(
                sse_json(&json!({
                    "type": "response.output_text.delta",
                    "delta": text
                }))
                .as_slice(),
            );
        }
    }

    chunks.extend_from_slice(
        sse_json(&json!({
            "type": "response.completed",
            "response": response
        }))
        .as_slice(),
    );
    chunks.extend_from_slice(b"data: [DONE]\n\n");
    chunks
}

fn sse_json(value: &serde_json::Value) -> Vec<u8> {
    let data = serde_json::to_string(value).unwrap_or_else(|_| "{}".to_string());
    let mut out = String::new();
    out.push_str("data: ");
    out.push_str(&data);
    out.push_str("\n\n");
    out.into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anthropic_messages_url_uses_deepseek_anthropic_base() {
        assert_eq!(
            anthropic_messages_url("https://api.deepseek.com"),
            "https://api.deepseek.com/anthropic/v1/messages"
        );
        assert_eq!(
            anthropic_messages_url("https://api.deepseek.com/anthropic"),
            "https://api.deepseek.com/anthropic/v1/messages"
        );
        assert_eq!(
            anthropic_messages_url("https://api.deepseek.com/anthropic/v1"),
            "https://api.deepseek.com/anthropic/v1/messages"
        );
        assert_eq!(
            anthropic_messages_url("https://api.deepseek.com/v1"),
            "https://api.deepseek.com/anthropic/v1/messages"
        );
    }

    #[test]
    fn build_anthropic_payload_maps_responses_tool_history() {
        let request = json!({
            "model": "deepseek-v4-pro",
            "instructions": "You are helpful",
            "tools": [{
                "type": "function",
                "function": {
                    "name": "get_weather",
                    "description": "Look up weather",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "city": { "type": "string" }
                        },
                        "required": ["city"]
                    }
                }
            }],
            "reasoning": {
                "effort": "xhigh"
            },
            "max_output_tokens": 2048,
            "input": [
                {
                    "type": "message",
                    "role": "user",
                    "content": [{ "type": "input_text", "text": "Weather in Tokyo?" }]
                },
                {
                    "type": "reasoning",
                    "content": "Need the weather tool."
                },
                {
                    "type": "function_call",
                    "call_id": "call_weather",
                    "name": "get_weather",
                    "arguments": "{\"city\":\"Tokyo\"}"
                },
                {
                    "type": "function_call_output",
                    "call_id": "call_weather",
                    "output": "{\"temp_c\":24}"
                }
            ]
        });

        let payload = build_anthropic_payload(&request, "deepseek-v4-pro").unwrap();
        let messages = payload["messages"].as_array().unwrap();

        assert_eq!(payload["system"], "You are helpful");
        assert_eq!(payload["max_tokens"], 2048);
        assert_eq!(payload["output_config"]["effort"], "max");
        assert_eq!(payload["tools"][0]["name"], "get_weather");
        assert_eq!(payload["tools"][0]["input_schema"]["type"], "object");
        assert_eq!(messages[0]["role"], "user");
        assert_eq!(messages[0]["content"][0]["type"], "text");
        assert_eq!(messages[1]["role"], "assistant");
        assert_eq!(messages[1]["content"][0]["type"], "thinking");
        assert_eq!(
            messages[1]["content"][0]["thinking"],
            "Need the weather tool."
        );
        assert_eq!(messages[1]["content"][1]["type"], "tool_use");
        assert_eq!(messages[1]["content"][1]["id"], "call_weather");
        assert_eq!(messages[1]["content"][1]["name"], "get_weather");
        assert_eq!(messages[1]["content"][1]["input"]["city"], "Tokyo");
        assert_eq!(messages[2]["role"], "user");
        assert_eq!(messages[2]["content"][0]["type"], "tool_result");
        assert_eq!(messages[2]["content"][0]["tool_use_id"], "call_weather");
        assert_eq!(messages[2]["content"][0]["content"], "{\"temp_c\":24}");
    }

    #[test]
    fn build_anthropic_payload_drops_unanswered_tool_call_turns() {
        let request = json!({
            "model": "deepseek-v4-pro",
            "input": [
                {
                    "type": "message",
                    "role": "user",
                    "content": [{ "type": "input_text", "text": "List files" }]
                },
                {
                    "type": "function_call",
                    "call_id": "call_ls",
                    "name": "shell",
                    "arguments": "{\"cmd\":\"ls\"}"
                }
            ]
        });

        let payload = build_anthropic_payload(&request, "deepseek-v4-pro").unwrap();
        let messages = payload["messages"].as_array().unwrap();

        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["role"], "user");
        assert_eq!(messages[0]["content"][0]["text"], "List files");
    }

    #[test]
    fn build_anthropic_payload_filters_partially_answered_tool_calls() {
        let request = json!({
            "model": "deepseek-v4-pro",
            "input": [
                {
                    "type": "message",
                    "role": "user",
                    "content": [{ "type": "input_text", "text": "Run two commands" }]
                },
                {
                    "type": "function_call",
                    "call_id": "call_one",
                    "name": "shell",
                    "arguments": "{\"cmd\":\"pwd\"}"
                },
                {
                    "type": "function_call",
                    "call_id": "call_two",
                    "name": "shell",
                    "arguments": "{\"cmd\":\"ls\"}"
                },
                {
                    "type": "function_call_output",
                    "call_id": "call_one",
                    "output": "/tmp"
                }
            ]
        });

        let payload = build_anthropic_payload(&request, "deepseek-v4-pro").unwrap();
        let messages = payload["messages"].as_array().unwrap();

        assert_eq!(messages[1]["role"], "assistant");
        assert_eq!(payload["thinking"]["type"], "disabled");
        assert_eq!(messages[1]["content"].as_array().unwrap().len(), 1);
        assert_eq!(messages[1]["content"][0]["type"], "tool_use");
        assert_eq!(messages[1]["content"][0]["id"], "call_one");
        assert_eq!(messages[2]["role"], "user");
        assert_eq!(messages[2]["content"][0]["type"], "tool_result");
        assert_eq!(messages[2]["content"][0]["tool_use_id"], "call_one");
        assert!(!serde_json::to_string(&messages)
            .unwrap()
            .contains("call_two"));
    }

    #[test]
    fn build_anthropic_payload_preserves_reasoning_across_assistant_text_before_tool_call() {
        let request = json!({
            "model": "deepseek-v4-pro",
            "input": [
                {
                    "type": "message",
                    "role": "user",
                    "content": [{ "type": "input_text", "text": "Run a command" }]
                },
                {
                    "type": "reasoning",
                    "content": "Need shell output."
                },
                {
                    "type": "message",
                    "role": "assistant",
                    "content": [{ "type": "output_text", "text": "Checking." }]
                },
                {
                    "type": "function_call",
                    "call_id": "call_shell",
                    "name": "shell",
                    "arguments": "{\"cmd\":\"pwd\"}"
                },
                {
                    "type": "function_call_output",
                    "call_id": "call_shell",
                    "output": "/tmp"
                }
            ]
        });

        let payload = build_anthropic_payload(&request, "deepseek-v4-pro").unwrap();
        let messages = payload["messages"].as_array().unwrap();

        assert!(payload.get("thinking").is_none());
        assert_eq!(messages[1]["role"], "assistant");
        assert_eq!(messages[1]["content"][0]["type"], "thinking");
        assert_eq!(messages[1]["content"][0]["thinking"], "Need shell output.");
        assert_eq!(messages[1]["content"][1]["type"], "text");
        assert_eq!(messages[1]["content"][1]["text"], "Checking.");
        assert_eq!(messages[1]["content"][2]["type"], "tool_use");
        assert_eq!(messages[1]["content"][2]["id"], "call_shell");
    }

    #[test]
    fn build_anthropic_payload_passes_through_image_in_responses_input() {
        let request = json!({
            "model": "deepseek-v4-pro",
            "input": [{
                "type": "message",
                "role": "user",
                "content": [
                    { "type": "input_text", "text": "describe" },
                    { "type": "input_image", "image_url": "data:image/png;base64,AAAA" }
                ]
            }]
        });
        let payload = build_anthropic_payload(&request, "deepseek-v4-pro").unwrap();
        let content = payload["messages"][0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 2);
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[0]["text"], "describe");
        assert_eq!(content[1]["type"], "image");
        assert_eq!(content[1]["source"]["type"], "base64");
        assert_eq!(content[1]["source"]["media_type"], "image/png");
        assert_eq!(content[1]["source"]["data"], "AAAA");
    }

    #[test]
    fn build_anthropic_payload_passes_through_image_in_chat_messages() {
        let request = json!({
            "model": "deepseek-v4-pro",
            "messages": [{
                "role": "user",
                "content": [
                    { "type": "text", "text": "describe" },
                    { "type": "image_url", "image_url": { "url": "https://example.com/x.png" } }
                ]
            }]
        });
        let payload = build_anthropic_payload(&request, "deepseek-v4-pro").unwrap();
        let content = payload["messages"][0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 2);
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[1]["type"], "image");
        assert_eq!(content[1]["source"]["type"], "url");
        assert_eq!(content[1]["source"]["url"], "https://example.com/x.png");
    }

    #[test]
    fn build_anthropic_payload_maps_direct_messages_with_tools() {
        let request = json!({
            "model": "deepseek-v4-pro",
            "messages": [
                {
                    "role": "user",
                    "content": "Run two commands"
                },
                {
                    "role": "assistant",
                    "content": "",
                    "tool_calls": [
                        {
                            "id": "call_one",
                            "type": "function",
                            "function": {
                                "name": "shell",
                                "arguments": "{\"cmd\":\"pwd\"}"
                            }
                        },
                        {
                            "id": "call_two",
                            "type": "function",
                            "function": {
                                "name": "shell",
                                "arguments": "{\"cmd\":\"ls\"}"
                            }
                        }
                    ]
                },
                {
                    "role": "tool",
                    "tool_call_id": "call_one",
                    "content": "/tmp"
                }
            ]
        });

        let payload = build_anthropic_payload(&request, "deepseek-v4-pro").unwrap();
        let messages = payload["messages"].as_array().unwrap();

        assert_eq!(messages[1]["content"].as_array().unwrap().len(), 1);
        assert_eq!(messages[1]["content"][0]["type"], "tool_use");
        assert_eq!(messages[1]["content"][0]["id"], "call_one");
        assert_eq!(messages[2]["content"][0]["type"], "tool_result");
        assert_eq!(messages[2]["content"][0]["tool_use_id"], "call_one");
        assert!(!serde_json::to_string(&messages)
            .unwrap()
            .contains("call_two"));
    }

    #[test]
    fn build_anthropic_payload_maps_developer_role_to_system() {
        let request = json!({
            "model": "deepseek-v4-pro",
            "input": [
                {
                    "type": "message",
                    "role": "user",
                    "content": [{ "type": "input_text", "text": "hello" }]
                },
                {
                    "type": "message",
                    "role": "developer",
                    "content": [{ "type": "input_text", "text": "keep answers concise" }]
                }
            ]
        });

        let payload = build_anthropic_payload(&request, "deepseek-v4-pro").unwrap();
        let messages = payload["messages"].as_array().unwrap();

        assert_eq!(payload["system"], "keep answers concise");
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["role"], "user");
        assert_eq!(messages[0]["content"][0]["text"], "hello");
    }

    #[test]
    fn anthropic_to_openai_response_maps_text_thinking_and_tool_use() {
        let upstream = json!({
            "id": "msg-test",
            "model": "deepseek-v4-pro",
            "content": [
                {
                    "type": "thinking",
                    "thinking": "Need the weather tool first."
                },
                {
                    "type": "text",
                    "text": "Checking."
                },
                {
                    "type": "tool_use",
                    "id": "call_weather",
                    "name": "get_weather",
                    "input": { "city": "Tokyo" }
                }
            ],
            "stop_reason": "tool_use",
            "usage": {
                "input_tokens": 10,
                "output_tokens": 5,
                "cache_read_input_tokens": 2
            }
        });

        let response = anthropic_to_openai_response(&upstream, "deepseek-v4-pro");
        let output = response["output"].as_array().unwrap();

        assert_eq!(response["object"], "response");
        assert_eq!(response["model"], "deepseek-v4-pro");
        assert_eq!(response["output_text"], "Checking.");
        assert_eq!(response["usage"]["input_tokens"], 10);
        assert_eq!(response["usage"]["cache_read_input_tokens"], 2);
        assert_eq!(output[0]["type"], "reasoning");
        assert_eq!(output[1]["type"], "message");
        assert_eq!(output[2]["type"], "function_call");
        assert_eq!(output[2]["call_id"], "call_weather");
        assert_eq!(output[2]["name"], "get_weather");
        assert_eq!(output[2]["arguments"], "{\"city\":\"Tokyo\"}");
    }
}
