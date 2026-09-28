use crate::source::v1::multimodal::openai_chat_content;
use axum::{
    body::Body,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
};
use bytes::Bytes;
use serde_json::json;
use std::time::Duration;
use uuid::Uuid;

const MODEL_FALLBACKS: &[(&str, &str, &str)] = &[
    (
        "qwen3-coder-plus",
        "Qwen3 Coder Plus",
        "Advanced code generation and understanding model",
    ),
    (
        "qwen3-coder-flash",
        "Qwen3 Coder Flash",
        "Fast code generation model",
    ),
    ("vision-model", "Qwen3 Vision Model", "Vision model"),
];

const QWEN_MODELS_API_URL: &str = "https://chat.qwen.ai/api/models";
const QWEN_CHAT_COMPLETIONS_API_URL: &str = "https://qwen.aikit.club/v1/chat/completions";

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
                    "No Qwen accounts configured",
                    "server_error",
                    None,
                ),
            )
                .into_response();
        }
    };

    let access_token = match super::auth::ensure_access_token(&state, &account).await {
        Ok(token) => token,
        Err(err) => {
            return (
                StatusCode::BAD_GATEWAY,
                [("Content-Type", "application/json")],
                crate::source::v1::response::openai_error_body(&err, "server_error", None),
            )
                .into_response();
        }
    };

    match fetch_models(
        &state.client,
        &access_token,
        &super::auth::base_url(&state, &account),
    )
    .await
    {
        Ok(body) => (StatusCode::OK, [("Content-Type", "application/json")], body).into_response(),
        Err(err) => {
            let data = MODEL_FALLBACKS
                .iter()
                .map(|(id, display_name, description)| {
                    json!({
                        "id": id,
                        "object": "model",
                        "created": 0,
                        "owned_by": "qwen",
                        "display_name": display_name,
                        "description": description
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

    let payload = match build_chat_payload(&request_value, &model) {
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
                "No Qwen accounts configured",
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
        let context = crate::qwen_usage_context(
            account,
            Some(model.clone()),
            "/qwen/v1/responses",
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
        crate::record_qwen_request(&state, &context);

        let access_token = match super::auth::ensure_access_token(&state, account).await {
            Ok(token) => token,
            Err(err) => {
                if attempt_idx + 1 < accounts.len() {
                    crate::record_request_pre_dispatch_retry_error(&state, &context, err.as_str());
                } else {
                    crate::record_request_pre_dispatch_error(&state, &context, err.as_str());
                }
                last_error = Some((StatusCode::BAD_GATEWAY, err));
                if attempt_idx + 1 < accounts.len() {
                    continue;
                }
                break;
            }
        };

        let upstream = match send_chat_request(
            &state.client,
            &access_token,
            &super::auth::base_url(&state, account),
            &payload,
        )
        .await
        {
            Ok(value) => value,
            Err((status, message)) => {
                crate::record_qwen_error(&state, &context, &message);
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

        let response = chat_to_openai_response(&upstream, &model);
        let mut usage = crate::usage_metrics_from_response_value(&response);
        crate::quota_usage::preserve_native_usage(&mut usage, &upstream, "openai");
        crate::apply_estimated_usage_fallback(
            &mut usage,
            &context.prompt,
            response
                .get("output_text")
                .and_then(|value| value.as_str())
                .unwrap_or_default(),
        );
        crate::record_qwen_success(&state, &context, &usage);
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
            "All Qwen accounts failed".to_string(),
        )
    });
    (
        status,
        [("Content-Type", "application/json")],
        crate::source::v1::response::openai_error_body(
            &format!("All Qwen accounts failed; last error: {}", message),
            "server_error",
            None,
        ),
    )
        .into_response()
}

async fn fetch_models(
    client: &reqwest::Client,
    access_token: &str,
    _base_url: &str,
) -> Result<Vec<u8>, String> {
    let request = client
        .get(QWEN_MODELS_API_URL)
        .header("Accept", "application/json")
        .timeout(Duration::from_secs(30));
    let resp = super::auth::qwen_headers(request, access_token)
        .send()
        .await
        .map_err(|e| e.to_string())?;

    if !resp.status().is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("Qwen models endpoint returned {}", body));
    }

    let value: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
    if value.get("data").and_then(|v| v.as_array()).is_some() {
        return serde_json::to_vec(&value).map_err(|e| e.to_string());
    }

    let Some(models) = value.get("models").and_then(|v| v.as_array()) else {
        return Err("Qwen models response was missing data".to_string());
    };

    let data = models
        .iter()
        .filter_map(|model| {
            let id = model.get("id").and_then(|v| v.as_str())?;
            Some(json!({
                "id": id,
                "object": "model",
                "created": 0,
                "owned_by": "qwen"
            }))
        })
        .collect::<Vec<_>>();

    serde_json::to_vec(&json!({
        "object": "list",
        "data": data
    }))
    .map_err(|e| e.to_string())
}

async fn send_chat_request(
    client: &reqwest::Client,
    access_token: &str,
    _base_url: &str,
    payload: &serde_json::Value,
) -> Result<serde_json::Value, (StatusCode, String)> {
    let request = client
        .post(QWEN_CHAT_COMPLETIONS_API_URL)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .bearer_auth(access_token)
        .body(payload.to_string())
        .timeout(Duration::from_secs(180));
    let resp = request
        .send()
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, e.to_string()))?;

    let status = resp.status();
    let text = resp
        .text()
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, e.to_string()))?;
    if !status.is_success() {
        return Err((
            StatusCode::BAD_GATEWAY,
            format!("Qwen returned {}: {}", status, text),
        ));
    }

    serde_json::from_str(&text).map_err(|e| {
        (
            StatusCode::BAD_GATEWAY,
            format!("failed to parse Qwen response: {}", e),
        )
    })
}

fn build_chat_payload(
    request_value: &serde_json::Value,
    model: &str,
) -> Result<serde_json::Value, String> {
    let messages = if let Some(messages) = request_value.get("messages").and_then(|v| v.as_array())
    {
        if messages.is_empty() {
            return Err("messages must not be empty".to_string());
        }
        let mut out = Vec::new();
        for message in messages {
            let role = message
                .get("role")
                .and_then(|v| v.as_str())
                .unwrap_or("user");
            let raw_content = message
                .get("content")
                .cloned()
                .unwrap_or_else(|| serde_json::Value::String(String::new()));
            let content = openai_chat_content(Some(&raw_content))
                .unwrap_or_else(|| serde_json::Value::String(String::new()));
            let mut entry = json!({ "role": role, "content": content });
            if let Some(name) = message.get("name").and_then(|v| v.as_str()) {
                entry["name"] = json!(name);
            }
            if let Some(tool_call_id) = message.get("tool_call_id").and_then(|v| v.as_str()) {
                entry["tool_call_id"] = json!(tool_call_id);
            }
            if let Some(tool_calls) = message.get("tool_calls").and_then(|v| v.as_array()) {
                entry["tool_calls"] = serde_json::Value::Array(tool_calls.clone());
            }
            out.push(entry);
        }
        out
    } else {
        build_messages_from_input(request_value)?
    };

    let mut payload = json!({
        "model": model,
        "messages": messages,
        "stream": false
    });

    if let Some(max_output_tokens) = request_value
        .get("max_output_tokens")
        .and_then(|v| v.as_u64())
    {
        payload["max_tokens"] = json!(max_output_tokens);
    }
    if let Some(enable_thinking) = request_value
        .get("enable_thinking")
        .and_then(serde_json::Value::as_bool)
    {
        payload["enable_thinking"] = json!(enable_thinking);
    }
    if let Some(temperature) = request_value.get("temperature").and_then(|v| v.as_f64()) {
        payload["temperature"] = json!(temperature);
    }
    if let Some(top_p) = request_value.get("top_p").and_then(|v| v.as_f64()) {
        payload["top_p"] = json!(top_p);
    }
    if let Some(stop) = request_value.get("stop") {
        payload["stop"] = stop.clone();
    }
    // Forward tool definitions so the upstream model can invoke them.
    // Without this, qwen has no idea what tools exist and just hallucinates
    // that the task is done.
    if let Some(tools) = build_chat_tools(request_value) {
        payload["tools"] = tools;
    }
    if let Some(choice) = build_chat_tool_choice(request_value) {
        payload["tool_choice"] = choice;
    }

    Ok(payload)
}

fn build_chat_tools(raw: &serde_json::Value) -> Option<serde_json::Value> {
    let tools = raw.get("tools")?.as_array()?;
    let mut out = Vec::new();
    for tool in tools {
        if let Some(function) = tool.get("function") {
            let mut mapped = json!({
                "type": "function",
                "function": {
                    "name": function.get("name").cloned().unwrap_or(serde_json::Value::String(String::new())),
                    "description": function.get("description").cloned().unwrap_or(serde_json::Value::String(String::new())),
                    "parameters": function.get("parameters").cloned().unwrap_or(json!({ "type": "object" }))
                }
            });
            if let Some(strict) = function.get("strict").and_then(|v| v.as_bool()) {
                mapped["function"]["strict"] = json!(strict);
            }
            out.push(mapped);
        } else if let Some(name) = tool.get("name").and_then(|v| v.as_str()) {
            out.push(json!({
                "type": "function",
                "function": {
                    "name": name,
                    "description": tool.get("description").cloned().unwrap_or(serde_json::Value::String(String::new())),
                    "parameters": tool.get("parameters").cloned().unwrap_or(json!({ "type": "object" }))
                }
            }));
        }
    }
    if out.is_empty() {
        None
    } else {
        Some(serde_json::Value::Array(out))
    }
}

fn build_chat_tool_choice(raw: &serde_json::Value) -> Option<serde_json::Value> {
    let choice = raw.get("tool_choice")?;
    if let Some(value) = choice.as_str() {
        return match value {
            "auto" | "none" | "required" => Some(json!(value)),
            other => Some(json!(other)),
        };
    }
    if let Some(function) = choice.get("function") {
        if let Some(name) = function.get("name").and_then(|v| v.as_str()) {
            return Some(json!({ "type": "function", "function": { "name": name } }));
        }
    }
    Some(choice.clone())
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
        return Err("only string or array input is supported for /qwen/v1/responses".to_string());
    };

    for item in items {
        let raw_content = item
            .get("content")
            .cloned()
            .unwrap_or_else(|| serde_json::Value::String(String::new()));
        if let Some(content) = openai_chat_content(Some(&raw_content)) {
            let role = item
                .get("role")
                .and_then(|value| value.as_str())
                .unwrap_or("user");
            messages.push(json!({
                "role": role,
                "content": content
            }));
        }
    }

    if messages.is_empty() {
        return Err("input did not contain any text content".to_string());
    }

    Ok(messages)
}

fn extract_text_from_input_item(item: &serde_json::Value) -> Option<String> {
    if let Some(content) = item.get("content").and_then(|v| v.as_str()) {
        let content = content.trim();
        if !content.is_empty() {
            return Some(content.to_string());
        }
    }

    let parts = item.get("content").and_then(|v| v.as_array())?;
    let mut out = String::new();
    for part in parts {
        let part_type = part
            .get("type")
            .and_then(|value| value.as_str())
            .unwrap_or("");
        let text = part
            .get("text")
            .and_then(|value| value.as_str())
            .or_else(|| part.get("input_text").and_then(|value| value.as_str()));
        if matches!(part_type, "text" | "input_text" | "output_text") {
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

fn chat_to_openai_response(value: &serde_json::Value, model: &str) -> serde_json::Value {
    let first_choice = value
        .get("choices")
        .and_then(|v| v.as_array())
        .and_then(|choices| choices.first());

    let message = first_choice.and_then(|choice| choice.get("message"));

    let content = message
        .and_then(|message| message.get("content"))
        .map(extract_content_text)
        .map(strip_proxy_footer)
        .unwrap_or_default();

    let mut output: Vec<serde_json::Value> = Vec::new();
    if !content.is_empty() {
        output.push(json!({
            "type": "message",
            "id": format!("msg_{}", Uuid::new_v4().simple()),
            "status": "completed",
            "role": "assistant",
            "content": [{
                "type": "output_text",
                "text": content,
                "annotations": []
            }]
        }));
    }

    if let Some(message) = message {
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
                    "arguments": arguments,
                    "status": "completed"
                }));
            }
        }
    }

    let usage = value.get("usage").cloned().unwrap_or_default();

    json!({
        "id": format!(
            "resp_{}",
            value
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or(&Uuid::new_v4().simple().to_string())
        ),
        "object": "response",
        "created_at": chrono::Utc::now().timestamp(),
        "status": "completed",
        "model": model,
        "output": output,
        "output_text": content,
        "usage": {
            "input_tokens": usage.get("prompt_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
            "output_tokens": usage.get("completion_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
            "total_tokens": usage.get("total_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
            "input_tokens_details": {
                "cached_tokens": usage.get("prompt_tokens_details").and_then(|v| v.get("cached_tokens")).and_then(|v| v.as_u64()).unwrap_or(0),
            },
            "output_tokens_details": {
                "reasoning_tokens": usage.get("completion_tokens_details").and_then(|v| v.get("reasoning_tokens")).and_then(|v| v.as_u64()).unwrap_or(0),
            }
        }
    })
}

fn extract_content_text(content: &serde_json::Value) -> String {
    if let Some(text) = content.as_str() {
        return text.to_string();
    }

    let Some(parts) = content.as_array() else {
        return String::new();
    };

    let mut out = String::new();
    for part in parts {
        let text = part
            .get("text")
            .and_then(|value| value.as_str())
            .or_else(|| part.get("content").and_then(|value| value.as_str()));
        if let Some(text) = text.map(str::trim).filter(|value| !value.is_empty()) {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(text);
        }
    }
    out
}

fn strip_proxy_footer(content: String) -> String {
    let trimmed = content.trim_end();
    if let Some(index) = trimmed.rfind("\n\n<details>") {
        let footer = &trimmed[index..];
        if footer.contains("Response ID:") && footer.contains("Request ID:") {
            return trimmed[..index].trim_end().to_string();
        }
    }

    content
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

    if let Some(output) = response.get("output").and_then(|v| v.as_array()) {
        for item in output {
            if item.get("type").and_then(|v| v.as_str()) == Some("function_call") {
                let call_id = item
                    .get("call_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default();
                let name = item
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default();
                let arguments = item
                    .get("arguments")
                    .and_then(|v| v.as_str())
                    .unwrap_or("{}");
                if !call_id.is_empty() {
                    chunks.extend_from_slice(
                        sse_json(&json!({
                            "type": "response.output_item.added",
                            "output_index": 0,
                            "item": {
                                "id": call_id,
                                "type": "function_call",
                                "call_id": call_id,
                                "name": name,
                                "arguments": "",
                                "status": "in_progress"
                            }
                        }))
                        .as_slice(),
                    );
                    chunks.extend_from_slice(
                        sse_json(&json!({
                            "type": "response.function_call_arguments.delta",
                            "output_index": 0,
                            "delta": arguments
                        }))
                        .as_slice(),
                    );
                    chunks.extend_from_slice(
                        sse_json(&json!({
                            "type": "response.function_call_arguments.done",
                            "output_index": 0,
                            "arguments": arguments
                        }))
                        .as_slice(),
                    );
                    chunks.extend_from_slice(
                        sse_json(&json!({
                            "type": "response.output_item.done",
                            "output_index": 0,
                            "item": {
                                "id": call_id,
                                "type": "function_call",
                                "call_id": call_id,
                                "name": name,
                                "arguments": arguments,
                                "status": "completed"
                            }
                        }))
                        .as_slice(),
                    );
                }
            }
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
    use super::{build_chat_payload, chat_to_openai_response, strip_proxy_footer};
    use serde_json::json;

    #[test]
    fn quota_prepared_qwen_request_preserves_disabled_thinking() {
        let payload = build_chat_payload(
            &json!({"input":"hi","max_output_tokens":20,"enable_thinking":false}),
            "qwen3-coder-plus",
        )
        .unwrap();
        assert_eq!(payload["enable_thinking"], false);
        let bounds = crate::quota_usage::prepared_bounds("qwen", &payload).unwrap();
        assert_eq!(bounds.output_upper_bound, Some(20));
        assert_eq!(
            bounds.input_upper_bound,
            serde_json::to_vec(&payload).unwrap().len() as u64
        );
    }

    #[test]
    fn strip_proxy_footer_removes_qwen_api_debug_block() {
        let content = "ok\n\n<details>\n<summary></summary>\n\n```\nResponse ID: abc\nRequest ID: def\n```\n</details>".to_string();
        assert_eq!(strip_proxy_footer(content), "ok");
    }

    #[test]
    fn strip_proxy_footer_keeps_normal_content() {
        let content = "normal response\n\n<details>\nno proxy ids here\n</details>".to_string();
        assert_eq!(strip_proxy_footer(content.clone()), content);
    }

    #[test]
    fn estimated_usage_fallback_populates_missing_qwen_usage() {
        let request = json!({
            "model": "qwen3-coder-plus",
            "input": "write a rust function that sums two numbers"
        });
        let upstream = json!({
            "id": "chatcmpl_test",
            "choices": [{
                "message": {
                    "content": "fn sum(a: i32, b: i32) -> i32 { a + b }"
                }
            }]
        });

        let response = chat_to_openai_response(&upstream, "qwen3-coder-plus");
        let mut usage = crate::usage_metrics_from_response_value(&response);
        let prompt_metrics = crate::prompt_metrics_from_request_value(&request);
        crate::apply_estimated_usage_fallback(
            &mut usage,
            &prompt_metrics,
            response
                .get("output_text")
                .and_then(|value| value.as_str())
                .unwrap_or_default(),
        );

        assert!(usage.input_tokens > 0);
        assert!(usage.output_tokens > 0);
        assert_eq!(usage.total_tokens, usage.input_tokens + usage.output_tokens);
        let estimated_usage = usage
            .raw_usage
            .as_ref()
            .and_then(|value| value.get("estimated_usage"))
            .cloned()
            .unwrap_or_default();
        assert_eq!(estimated_usage.get("provider"), Some(&json!("qwen")));
        // Input accounting is now the conservative full serialized request,
        // rather than the old narrow prompt-text character count.
        assert_eq!(
            estimated_usage.get("input_chars"),
            Some(&json!(prompt_metrics.input_chars))
        );
        assert_eq!(
            estimated_usage.get("input_tokens"),
            Some(&json!(usage.input_tokens))
        );
        assert_eq!(
            estimated_usage.get("output_tokens"),
            Some(&json!(usage.output_tokens))
        );
        assert_eq!(
            estimated_usage.get("total_tokens"),
            Some(&json!(usage.total_tokens))
        );
        assert!(
            estimated_usage
                .get("output_chars")
                .and_then(|value| value.as_u64())
                .unwrap_or_default()
                > 0
        );
    }

    #[test]
    fn estimated_usage_fallback_preserves_upstream_qwen_usage() {
        let request = json!({
            "model": "qwen3-coder-plus",
            "input": "say hello"
        });
        let upstream = json!({
            "id": "chatcmpl_test",
            "choices": [{
                "message": {
                    "content": "hello"
                }
            }],
            "usage": {
                "prompt_tokens": 11,
                "completion_tokens": 7,
                "total_tokens": 18,
                "prompt_tokens_details": {
                    "cached_tokens": 2
                },
                "completion_tokens_details": {
                    "reasoning_tokens": 1
                }
            }
        });

        let response = chat_to_openai_response(&upstream, "qwen3-coder-plus");
        let mut usage = crate::usage_metrics_from_response_value(&response);
        let raw_usage_before = usage.raw_usage.clone();
        crate::apply_estimated_usage_fallback(
            &mut usage,
            &crate::prompt_metrics_from_request_value(&request),
            response
                .get("output_text")
                .and_then(|value| value.as_str())
                .unwrap_or_default(),
        );

        assert_eq!(usage.input_tokens, 11);
        assert_eq!(usage.output_tokens, 7);
        assert_eq!(usage.total_tokens, 18);
        assert_eq!(usage.cache_tokens, 2);
        assert_eq!(usage.reasoning_tokens, 1);
        assert_eq!(usage.raw_usage, raw_usage_before);
    }

    #[test]
    fn build_chat_payload_passes_through_image_in_responses_input() {
        let request = json!({
            "model": "qwen3-coder-plus",
            "input": [{
                "type": "message",
                "role": "user",
                "content": [
                    { "type": "input_text", "text": "describe" },
                    { "type": "input_image", "image_url": "data:image/png;base64,AAAA" }
                ]
            }]
        });
        let payload = build_chat_payload(&request, "qwen3-coder-plus").unwrap();
        let content = payload["messages"][0]["content"]
            .as_array()
            .expect("multimodal must be array");
        assert_eq!(content.len(), 2);
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[0]["text"], "describe");
        assert_eq!(content[1]["type"], "image_url");
        assert_eq!(content[1]["image_url"]["url"], "data:image/png;base64,AAAA");
    }

    #[test]
    fn build_chat_payload_passes_through_image_in_chat_messages() {
        let request = json!({
            "model": "qwen3-coder-plus",
            "messages": [{
                "role": "user",
                "content": [
                    { "type": "text", "text": "see" },
                    { "type": "image_url", "image_url": { "url": "https://example.com/x.png" } }
                ]
            }]
        });
        let payload = build_chat_payload(&request, "qwen3-coder-plus").unwrap();
        let content = payload["messages"][0]["content"]
            .as_array()
            .expect("multimodal must be array");
        assert_eq!(content.len(), 2);
        assert_eq!(content[1]["image_url"]["url"], "https://example.com/x.png");
    }

    #[test]
    fn build_chat_payload_collapses_text_only_to_string() {
        let request = json!({
            "model": "qwen3-coder-plus",
            "input": [{
                "type": "message",
                "role": "user",
                "content": [
                    { "type": "input_text", "text": "hello " },
                    { "type": "input_text", "text": "world" }
                ]
            }]
        });
        let payload = build_chat_payload(&request, "qwen3-coder-plus").unwrap();
        assert_eq!(payload["messages"][0]["content"], "hello \nworld");
    }

    #[test]
    fn build_chat_payload_forwards_tools_and_tool_choice() {
        let request = json!({
            "model": "qwen3-coder-plus",
            "input": "what is the weather",
            "tools": [{
                "type": "function",
                "function": {
                    "name": "shell",
                    "description": "Run a shell command",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "command": { "type": "string" }
                        }
                    }
                }
            }],
            "tool_choice": "auto"
        });
        let payload = build_chat_payload(&request, "qwen3-coder-plus").unwrap();
        let tools = payload["tools"].as_array().expect("tools forwarded");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["type"], "function");
        assert_eq!(tools[0]["function"]["name"], "shell");
        assert_eq!(tools[0]["function"]["description"], "Run a shell command");
        assert_eq!(
            tools[0]["function"]["parameters"]["properties"]["command"]["type"],
            "string"
        );
        assert_eq!(payload["tool_choice"], "auto");
    }

    #[test]
    fn build_chat_payload_forwards_response_style_tool_definitions() {
        let request = json!({
            "model": "qwen3-coder-plus",
            "input": "do it",
            "tools": [{
                "type": "function",
                "name": "shell",
                "description": "shell",
                "parameters": { "type": "object" }
            }]
        });
        let payload = build_chat_payload(&request, "qwen3-coder-plus").unwrap();
        let tools = payload["tools"].as_array().expect("tools forwarded");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["type"], "function");
        assert_eq!(tools[0]["function"]["name"], "shell");
    }

    #[test]
    fn chat_to_openai_response_emits_function_call_items() {
        let upstream = json!({
            "id": "chatcmpl_abc",
            "choices": [{
                "message": {
                    "content": "I'll run the shell now.",
                    "tool_calls": [{
                        "id": "call_42",
                        "type": "function",
                        "function": {
                            "name": "shell",
                            "arguments": "{\"command\":\"ls\"}"
                        }
                    }]
                }
            }],
            "usage": {
                "prompt_tokens": 5,
                "completion_tokens": 7,
                "total_tokens": 12
            }
        });
        let response = chat_to_openai_response(&upstream, "qwen3-coder-plus");
        let output = response["output"].as_array().expect("output array");
        assert_eq!(output.len(), 2);
        assert_eq!(output[0]["type"], "message");
        assert_eq!(output[0]["content"][0]["text"], "I'll run the shell now.");
        assert_eq!(output[1]["type"], "function_call");
        assert_eq!(output[1]["call_id"], "call_42");
        assert_eq!(output[1]["name"], "shell");
        assert_eq!(output[1]["arguments"], "{\"command\":\"ls\"}");
        assert_eq!(response["output_text"], "I'll run the shell now.");
    }
}
