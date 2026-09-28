use crate::source::v1::multimodal::{classify_content, is_data_url, split_data_url, PartKind};
use axum::{
    body::Body,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
};
use base64::Engine;
use bytes::Bytes;
use serde_json::{json, Value};
use std::time::Duration;
use uuid::Uuid;

const DUMMY_THOUGHT_SIGNATURE: &str = "skip_thought_signature_validator";

const MODEL_FALLBACKS: &[&str] = &[
    "gemini-2.5-pro",
    "gemini-2.5-flash",
    "gemini-2.5-flash-lite",
    "gemini-2.5-flash-image-preview",
    "gemini-3-pro",
    "gemini-3-flash",
];

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
                    "No Gemini accounts configured",
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

    let project_id = super::auth::request_project_id(&account);
    match fetch_models(&state.client, &access_token, &project_id).await {
        Ok(body) => (StatusCode::OK, [("Content-Type", "application/json")], body).into_response(),
        Err(err) => {
            let data = MODEL_FALLBACKS
                .iter()
                .map(|id| {
                    json!({
                        "id": id,
                        "object": "model",
                        "created": 0,
                        "owned_by": "gemini"
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

    let model = match request_value.get("model").and_then(|value| value.as_str()) {
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
                "No Gemini accounts configured",
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
        let context = crate::gemini_usage_context(
            account,
            Some(model.clone()),
            "/gemini/v1/responses",
            prompt_metrics.clone(),
        );

        let payload = match build_google_payload(
            &request_value,
            &model,
            &super::auth::request_project_id(account),
        ) {
            Ok(payload) => payload,
            Err(err) => {
                crate::router_reservation_cancelled(&state, context.provider_name, &context.key);
                crate::release_api_key_budget_before_dispatch(&state);
                return (
                    StatusCode::BAD_REQUEST,
                    [("Content-Type", "application/json")],
                    crate::source::v1::response::openai_error_body(
                        &err,
                        "invalid_request_error",
                        None,
                    ),
                )
                    .into_response();
            }
        };

        if let Err(response) = crate::api_key_quota_runtime::reserve_api_key_budgets_for_prepared_dispatch_with_protocol(
            &state, context.provider_name, &context.key,
            &serde_json::to_vec(&payload).unwrap_or_default(),
            crate::quota_usage::PreparedProtocol::GoogleGenerateContent,
        ).await {
            return response;
        }
        crate::record_gemini_request(&state, &context);

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

        let upstream = match send_generate_request(&state.client, &access_token, &payload).await {
            Ok(value) => value,
            Err((status, message)) => {
                crate::record_gemini_error(&state, &context, &message);
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

        let mut openai_response = google_to_openai_response(&upstream, &model);
        let mut usage = crate::usage_metrics_from_response_value(&openai_response);
        crate::quota_usage::preserve_native_usage(&mut usage, &upstream, "google");
        crate::record_gemini_success(&state, &context, &usage);
        attach_temp_downloads(&mut openai_response);

        if wants_stream {
            let sse = render_response_sse(&openai_response);
            return (
                StatusCode::OK,
                [
                    ("Content-Type", "text/event-stream"),
                    ("Cache-Control", "no-store"),
                ],
                Body::from(sse),
            )
                .into_response();
        }

        let body = serde_json::to_vec(&openai_response).unwrap_or_default();
        return (StatusCode::OK, [("Content-Type", "application/json")], body).into_response();
    }

    let (status, message) = last_error.unwrap_or_else(|| {
        (
            StatusCode::BAD_GATEWAY,
            "All Gemini accounts failed".to_string(),
        )
    });
    (
        status,
        [("Content-Type", "application/json")],
        crate::source::v1::response::openai_error_body(
            &format!("All Gemini accounts failed; last error: {}", message),
            "server_error",
            None,
        ),
    )
        .into_response()
}

async fn fetch_models(
    client: &reqwest::Client,
    access_token: &str,
    project_id: &str,
) -> Result<Vec<u8>, String> {
    let body = if project_id.trim().is_empty() {
        json!({})
    } else {
        json!({ "project": project_id })
    };
    let resp = super::auth::gemini_headers(
        client
            .post(format!(
                "{}/v1internal:fetchAvailableModels",
                super::auth::gemini_cli_base_url()
            ))
            .header("Content-Type", "application/json")
            .body(body.to_string())
            .timeout(Duration::from_secs(30)),
        access_token,
    )
    .send()
    .await
    .map_err(|e| e.to_string())?;

    if !resp.status().is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("fetchAvailableModels returned {}", body));
    }

    let value: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
    let mut data = Vec::new();

    if let Some(models) = value.get("models").and_then(|value| value.as_object()) {
        for (id, model_data) in models {
            if !looks_like_gemini_model(id, model_data) {
                continue;
            }
            data.push(json!({
                "id": id,
                "object": "model",
                "created": 0,
                "owned_by": "gemini",
                "description": model_data.get("displayName").and_then(|value| value.as_str()).unwrap_or(id)
            }));
        }
    }

    if data.is_empty() {
        return Err("Gemini models response did not contain Gemini models".to_string());
    }

    serde_json::to_vec(&json!({
        "object": "list",
        "data": data
    }))
    .map_err(|e| e.to_string())
}

async fn send_generate_request(
    client: &reqwest::Client,
    access_token: &str,
    payload: &serde_json::Value,
) -> Result<serde_json::Value, (StatusCode, String)> {
    let resp = super::auth::gemini_headers(
        client
            .post(format!(
                "{}/v1internal:generateContent",
                super::auth::gemini_cli_base_url()
            ))
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .body(payload.to_string())
            .timeout(Duration::from_secs(180)),
        access_token,
    )
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
            format!("Gemini returned {}: {}", status, text),
        ));
    }

    serde_json::from_str(&text).map_err(|e| {
        (
            StatusCode::BAD_GATEWAY,
            format!("failed to parse Gemini response: {}", e),
        )
    })
}

/// Strip the helper `_call_id` field we use to correlate a
/// functionCall part with the functionResponse tool result. Google's
/// API does not accept arbitrary metadata in parts, and the upstream
/// model only needs the structured `functionCall` body to keep the
/// multi-turn shape correct.
fn sanitize_google_contents(contents: Vec<serde_json::Value>) -> Vec<serde_json::Value> {
    contents
        .into_iter()
        .map(|mut entry| {
            if let Some(parts) = entry.get_mut("parts").and_then(|v| v.as_array_mut()) {
                for part in parts.iter_mut() {
                    if let Some(obj) = part.as_object_mut() {
                        obj.remove("_call_id");
                    }
                }
            }
            entry
        })
        .collect()
}

fn build_google_payload(
    request_value: &serde_json::Value,
    model: &str,
    project_id: &str,
) -> Result<serde_json::Value, String> {
    let contents = build_google_contents(request_value)?;
    let instructions = request_value
        .get("instructions")
        .and_then(|value| value.as_str())
        .map(|value| value.to_string());
    let image_mode = is_image_request(request_value, model);

    let contents = sanitize_google_contents(contents);
    let mut request = json!({
        "contents": contents,
    });

    if let Some(instructions) = instructions {
        request["systemInstruction"] = json!({
            "parts": [{
                "text": instructions
            }]
        });
    }

    let mut generation_config = serde_json::Map::new();
    if image_mode {
        generation_config.insert("responseModalities".to_string(), json!(["TEXT", "IMAGE"]));
    }
    if let Some(max_output_tokens) = request_value
        .get("max_output_tokens")
        .and_then(|value| value.as_u64())
    {
        generation_config.insert("maxOutputTokens".to_string(), json!(max_output_tokens));
    }
    // Keep explicit thinking bounds in the final native payload. An omitted
    // or dynamic budget remains unsupported by strict output-token quotas.
    if let Some(thinking) = request_value
        .get("thinking_config")
        .or_else(|| request_value.get("thinkingConfig"))
        .filter(|value| value.is_object())
    {
        generation_config.insert("thinkingConfig".to_string(), thinking.clone());
    }
    if let Some(temperature) = request_value
        .get("temperature")
        .and_then(|value| value.as_f64())
    {
        generation_config.insert("temperature".to_string(), json!(temperature));
    }
    if let Some(top_p) = request_value.get("top_p").and_then(|value| value.as_f64()) {
        generation_config.insert("topP".to_string(), json!(top_p));
    }
    if !generation_config.is_empty() {
        request["generationConfig"] = serde_json::Value::Object(generation_config);
    }
    if let Some(google_tools) = build_google_tools(request_value) {
        request["tools"] = google_tools;
        if let Some(tool_config) = build_google_tool_config(request_value) {
            request["toolConfig"] = tool_config;
        }
    }

    Ok(json!({
        "project": project_id,
        "model": model,
        "request": request
    }))
}

fn build_google_tools(request_value: &serde_json::Value) -> Option<serde_json::Value> {
    let tools = request_value.get("tools")?.as_array()?;
    let mut declarations: Vec<serde_json::Value> = Vec::new();
    for tool in tools {
        if tool
            .get("type")
            .and_then(|v| v.as_str())
            .map(|kind| kind == "image_generation")
            .unwrap_or(false)
        {
            continue;
        }
        let function = tool.get("function").unwrap_or(tool);
        let name = function.get("name").and_then(|v| v.as_str())?;
        let description = function
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let parameters = function
            .get("parameters")
            .cloned()
            .unwrap_or_else(|| json!({ "type": "object", "properties": {} }));
        declarations.push(json!({
            "name": name,
            "description": description,
            "parameters": parameters,
        }));
    }
    if declarations.is_empty() {
        return None;
    }
    Some(json!([{ "functionDeclarations": declarations }]))
}

fn build_google_tool_config(request_value: &serde_json::Value) -> Option<serde_json::Value> {
    let mode = match request_value.get("tool_choice") {
        Some(serde_json::Value::String(s)) => match s.as_str() {
            "auto" => "AUTO",
            "none" => "NONE",
            "required" => "ANY",
            other => other,
        },
        Some(serde_json::Value::Object(obj)) => {
            if obj.get("type").and_then(|v| v.as_str()) == Some("function") {
                "ANY"
            } else {
                "AUTO"
            }
        }
        _ => "AUTO",
    };
    Some(json!({
        "functionCallingConfig": { "mode": mode }
    }))
}

fn chat_tool_call_to_function_call_part(tc: &Value) -> Option<Value> {
    let function = tc.get("function")?;
    let name = function.get("name").and_then(|v| v.as_str())?;
    let arguments = function
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let args_value = if let Some(s) = arguments.as_str() {
        serde_json::from_str::<serde_json::Value>(s).unwrap_or_else(|_| json!({ "raw": s }))
    } else {
        arguments.clone()
    };
    let call_id = tc.get("id").and_then(|v| v.as_str()).unwrap_or("");
    Some(function_call_part(
        name,
        args_value,
        call_id,
        thought_signature_from_value(tc),
    ))
}

/// Build the Google `contents` array for a Codex/OpenAI request.
///
/// Codex Responses input is already an ordered transcript. Preserve it
/// as the source of truth and translate tool calls into Google's native
/// `functionCall` / `functionResponse` parts, otherwise agent loops lose
/// their tool state and can repeat the same command indefinitely.
fn build_google_contents(request_value: &Value) -> Result<Vec<Value>, String> {
    let mut entries: Vec<(String, Vec<Value>)> = Vec::new();

    if let Some(items) = request_value.get("input").and_then(|v| v.as_array()) {
        for item in items {
            append_input_item(&mut entries, item);
        }
    } else if let Some(prompt) = request_value.get("input").and_then(|v| v.as_str()) {
        let trimmed = prompt.trim();
        if !trimmed.is_empty() {
            entries.push(("user".to_string(), vec![json!({ "text": trimmed })]));
        }
    }

    if let Some(messages) = request_value.get("messages").and_then(|v| v.as_array()) {
        for message in messages {
            append_chat_message(&mut entries, message);
        }
    }

    if entries.is_empty() {
        return Err(
            "only string input or messages[] with content is supported for /agw/v1/responses"
                .to_string(),
        );
    }

    // Coalesce adjacent entries with the same role.
    let mut contents: Vec<Value> = Vec::new();
    for (role, parts) in entries {
        match contents.last_mut() {
            Some(last) if last["role"] == role => {
                if let Some(existing) = last.get_mut("parts").and_then(|v| v.as_array_mut()) {
                    existing.extend(parts);
                }
            }
            _ => {
                contents.push(json!({ "role": role, "parts": parts }));
            }
        }
    }

    Ok(contents)
}

fn append_chat_message(entries: &mut Vec<(String, Vec<Value>)>, message: &Value) {
    let role = message
        .get("role")
        .and_then(|value| value.as_str())
        .unwrap_or("user");

    if role == "tool" {
        let call_id = message
            .get("tool_call_id")
            .and_then(|value| value.as_str())
            .unwrap_or("");
        let name = lookup_function_name(entries, call_id).unwrap_or_else(|| "tool".to_string());
        let output = message_content_text(message.get("content")).unwrap_or_default();
        entries.push((
            "function".to_string(),
            vec![json!({
                "functionResponse": {
                    "name": name,
                    "response": { "result": output }
                }
            })],
        ));
        return;
    }

    let role = match role {
        "system" | "developer" => return,
        "assistant" => "model",
        "user" | "" => "user",
        other => other,
    };
    let mut parts: Vec<Value> = Vec::new();
    if let Some(content) = message.get("content") {
        for part in classify_content(Some(content)) {
            push_google_part(&mut parts, part);
        }
    }
    if let Some(tool_calls) = message.get("tool_calls").and_then(|value| value.as_array()) {
        for tool_call in tool_calls {
            if let Some(part) = chat_tool_call_to_function_call_part(tool_call) {
                parts.push(part);
            }
        }
    }
    if !parts.is_empty() {
        entries.push((role.to_string(), parts));
    }
}

fn append_input_item(entries: &mut Vec<(String, Vec<Value>)>, item: &Value) {
    let item_type = item.get("type").and_then(|v| v.as_str()).unwrap_or("");
    match item_type {
        "function_call" => {
            let call_id = item
                .get("call_id")
                .or_else(|| item.get("id"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let name = item.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let arguments = item.get("arguments").cloned().unwrap_or_else(|| json!({}));
            let args_value = if let Some(s) = arguments.as_str() {
                serde_json::from_str::<serde_json::Value>(s).unwrap_or_else(|_| json!({ "raw": s }))
            } else {
                arguments.clone()
            };
            if name.is_empty() {
                return;
            }
            entries.push((
                "model".to_string(),
                vec![function_call_part(
                    name,
                    args_value,
                    call_id,
                    thought_signature_from_value(item),
                )],
            ));
        }
        "function_call_output" => {
            let call_id = item.get("call_id").and_then(|v| v.as_str()).unwrap_or("");
            let output = item.get("output").cloned().unwrap_or_else(|| json!(""));
            let output_str = if let Some(s) = output.as_str() {
                s.to_string()
            } else {
                output.to_string()
            };
            // Google's functionResponse requires a `name` we can pair
            // with the corresponding functionCall. When the prior
            // model turn carried a `_call_id` we look back through the
            // entries to find the matching functionCall name. If we
            // can't find it, fall back to "tool".
            let name = lookup_function_name(entries, call_id).unwrap_or_else(|| "tool".to_string());
            entries.push((
                "function".to_string(),
                vec![json!({
                    "functionResponse": {
                        "name": name,
                        "response": { "result": output_str }
                    }
                })],
            ));
        }
        "message" | "" => {
            let role = item.get("role").and_then(|v| v.as_str()).unwrap_or("user");
            let role = match role {
                "system" | "developer" => return,
                "assistant" => "model",
                "user" | "" => "user",
                "tool" => "user",
                other => other,
            };
            let mut parts: Vec<Value> = Vec::new();
            if let Some(content) = item.get("content") {
                for part in classify_content(Some(content)) {
                    push_google_part(&mut parts, part);
                }
            }
            if parts.is_empty() {
                return;
            }
            entries.push((role.to_string(), parts));
        }
        _ => {}
    }
}

fn lookup_function_name(entries: &[(String, Vec<Value>)], call_id: &str) -> Option<String> {
    if call_id.is_empty() {
        return None;
    }
    for (_role, parts) in entries.iter().rev() {
        for part in parts.iter().rev() {
            let fc = part.get("functionCall")?;
            if part.get("_call_id").and_then(|v| v.as_str()) == Some(call_id) {
                if let Some(name) = fc.get("name").and_then(|v| v.as_str()) {
                    return Some(name.to_string());
                }
            }
        }
    }
    None
}

fn function_call_part(
    name: &str,
    args_value: Value,
    call_id: &str,
    thought_signature: Option<&str>,
) -> Value {
    let mut part = json!({
        "functionCall": { "name": name, "args": args_value },
        "_call_id": call_id,
    });
    attach_thought_signature(&mut part, thought_signature);
    part
}

fn thought_signature_from_value(value: &Value) -> Option<&str> {
    value
        .get("thoughtSignature")
        .or_else(|| value.get("thought_signature"))
        .and_then(|signature| signature.as_str())
        .filter(|signature| !signature.trim().is_empty())
}

fn attach_thought_signature(part: &mut Value, signature: Option<&str>) {
    let signature = signature.unwrap_or(DUMMY_THOUGHT_SIGNATURE);
    if let Some(object) = part.as_object_mut() {
        object.insert("thoughtSignature".to_string(), json!(signature));
    }
}

fn message_content_text(content: Option<&Value>) -> Option<String> {
    let mut out = String::new();
    for part in classify_content(content) {
        if let PartKind::Text(text) = part {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                continue;
            }
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(trimmed);
        }
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

fn push_google_part(out: &mut Vec<Value>, part: PartKind) {
    match part {
        PartKind::Text(text) => {
            let trimmed = text.trim();
            if !trimmed.is_empty() {
                out.push(json!({ "text": trimmed }));
            }
        }
        PartKind::Image(url) => {
            if let Some(block) = google_inline_data_part(&url) {
                out.push(block);
            }
        }
        PartKind::Other(_) => {
            // Unknown part types are dropped; Google's `parts` vocabulary is fixed.
        }
    }
}

fn google_inline_data_part(url: &str) -> Option<Value> {
    if is_data_url(url) {
        let (mime_type, payload) = split_data_url(url)?;
        return Some(json!({
            "inline_data": {
                "mime_type": mime_type,
                "data": payload,
            }
        }));
    }
    if url.starts_with("http://") || url.starts_with("https://") {
        // Google's Generative API supports `file_data` for remote files.
        return Some(json!({
            "file_data": {
                "file_uri": url,
            }
        }));
    }
    None
}

fn is_image_request(request_value: &serde_json::Value, model: &str) -> bool {
    if model.to_ascii_lowercase().contains("image") {
        return true;
    }

    request_value
        .get("tools")
        .and_then(|value| value.as_array())
        .map(|tools| {
            tools.iter().any(|tool| {
                tool.get("type")
                    .and_then(|value| value.as_str())
                    .map(|kind| kind == "image_generation")
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false)
}

fn looks_like_gemini_model(model_id: &str, model_data: &serde_json::Value) -> bool {
    let lower = model_id.to_ascii_lowercase();
    if lower.starts_with("gemini") || lower == "rev19-uic3-1p" {
        return true;
    }
    model_data
        .get("displayName")
        .and_then(|value| value.as_str())
        .map(|value| value.to_ascii_lowercase().contains("gemini"))
        .unwrap_or(false)
}

fn google_to_openai_response(value: &serde_json::Value, model: &str) -> serde_json::Value {
    let response = value.get("response").unwrap_or(value);
    let parts = response
        .get("candidates")
        .and_then(|value| value.as_array())
        .and_then(|candidates| candidates.first())
        .and_then(|candidate| candidate.get("content"))
        .and_then(|content| content.get("parts"))
        .and_then(|parts| parts.as_array())
        .cloned()
        .unwrap_or_default();

    let usage = response.get("usageMetadata").cloned().unwrap_or_default();
    let mut output_text = String::new();
    let mut images: Vec<serde_json::Value> = Vec::new();
    let mut function_calls: Vec<serde_json::Value> = Vec::new();

    for part in parts {
        if let Some(text) = part.get("text").and_then(|value| value.as_str()) {
            if !text.trim().is_empty() {
                output_text.push_str(text);
            }
        }

        if let Some(inline_data) = part.get("inlineData").and_then(|value| value.as_object()) {
            let mime_type = inline_data
                .get("mimeType")
                .and_then(|value| value.as_str())
                .unwrap_or("image/png");
            let data = inline_data
                .get("data")
                .and_then(|value| value.as_str())
                .unwrap_or("");
            if mime_type.starts_with("image/") && !data.is_empty() {
                images.push(json!({
                    "mime_type": mime_type,
                    "b64_json": data
                }));
            }
        }

        if let Some(fc) = part.get("functionCall").and_then(|value| value.as_object()) {
            let name = fc
                .get("name")
                .and_then(|value| value.as_str())
                .unwrap_or("tool")
                .to_string();
            let args = fc.get("args").cloned().unwrap_or_else(|| json!({}));
            let args_str = if let Some(s) = args.as_str() {
                s.to_string()
            } else {
                args.to_string()
            };
            let call_id = format!("call_{}", Uuid::new_v4().simple());
            let mut function_call = json!({
                "id": call_id,
                "type": "function_call",
                "call_id": call_id,
                "name": name,
                "arguments": args_str,
                "status": "completed",
            });
            if let Some(signature) = part
                .get("thoughtSignature")
                .and_then(|value| value.as_str())
            {
                function_call["thoughtSignature"] = json!(signature);
                function_call["thought_signature"] = json!(signature);
            }
            function_calls.push(function_call);
        }
    }

    let mut output = Vec::new();
    if !output_text.is_empty() || (images.is_empty() && function_calls.is_empty()) {
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
    for call in function_calls {
        output.push(call);
    }

    for image in &images {
        output.push(json!({
            "type": "image_generation_call",
            "id": format!("ig_{}", Uuid::new_v4().simple()),
            "status": "completed",
            "result": image
        }));
    }

    json!({
        "id": format!("resp_{}", Uuid::new_v4().simple()),
        "object": "response",
        "created_at": chrono::Utc::now().timestamp(),
        "status": "completed",
        "model": model,
        "output": output,
        "output_text": output_text,
        "images": images,
        "usage": {
            "input_tokens": usage.get("promptTokenCount").and_then(|value| value.as_u64()).unwrap_or(0),
            "output_tokens": usage.get("candidatesTokenCount").and_then(|value| value.as_u64()).unwrap_or(0),
            "total_tokens": usage.get("totalTokenCount").and_then(|value| value.as_u64()).unwrap_or(
                usage.get("promptTokenCount").and_then(|value| value.as_u64()).unwrap_or(0)
                    + usage.get("candidatesTokenCount").and_then(|value| value.as_u64()).unwrap_or(0)
            ),
            "input_tokens_details": {
                "cached_tokens": usage.get("cachedContentTokenCount").and_then(|v| v.as_u64()).unwrap_or(0),
            },
            "output_tokens_details": {
                "reasoning_tokens": usage.get("thoughtsTokenCount").and_then(|v| v.as_u64()).unwrap_or(0),
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

    if let Some(output) = response.get("output").and_then(|value| value.as_array()) {
        for (output_index, item) in output.iter().enumerate() {
            let mut in_progress = item.clone();
            if let Some(object) = in_progress.as_object_mut() {
                if object.contains_key("status") {
                    object.insert("status".to_string(), json!("in_progress"));
                }
            }
            chunks.extend_from_slice(
                sse_json(&json!({
                    "type": "response.output_item.added",
                    "output_index": output_index,
                    "item": in_progress
                }))
                .as_slice(),
            );

            match item.get("type").and_then(|value| value.as_str()) {
                Some("message") => {
                    let item_id = item
                        .get("id")
                        .and_then(|value| value.as_str())
                        .unwrap_or_default();
                    if let Some(content) = item.get("content").and_then(|value| value.as_array()) {
                        for (content_index, part) in content.iter().enumerate() {
                            if part.get("type").and_then(|value| value.as_str())
                                != Some("output_text")
                            {
                                continue;
                            }
                            let text = part
                                .get("text")
                                .and_then(|value| value.as_str())
                                .unwrap_or("");
                            chunks.extend_from_slice(
                                sse_json(&json!({
                                    "type": "response.content_part.added",
                                    "item_id": item_id,
                                    "output_index": output_index,
                                    "content_index": content_index,
                                    "part": {
                                        "type": "output_text",
                                        "text": "",
                                        "annotations": part.get("annotations").cloned().unwrap_or_else(|| json!([]))
                                    }
                                }))
                                .as_slice(),
                            );
                            for delta in text_delta_chunks(text) {
                                chunks.extend_from_slice(
                                    sse_json(&json!({
                                        "type": "response.output_text.delta",
                                        "item_id": item_id,
                                        "output_index": output_index,
                                        "content_index": content_index,
                                        "delta": delta
                                    }))
                                    .as_slice(),
                                );
                            }
                            chunks.extend_from_slice(
                                sse_json(&json!({
                                    "type": "response.output_text.done",
                                    "item_id": item_id,
                                    "output_index": output_index,
                                    "content_index": content_index,
                                    "text": text
                                }))
                                .as_slice(),
                            );
                            chunks.extend_from_slice(
                                sse_json(&json!({
                                    "type": "response.content_part.done",
                                    "item_id": item_id,
                                    "output_index": output_index,
                                    "content_index": content_index,
                                    "part": part
                                }))
                                .as_slice(),
                            );
                        }
                    }
                }
                Some("image_generation_call") => {
                    if let Some(data) = item
                        .get("result")
                        .and_then(|result| result.get("b64_json"))
                        .and_then(|value| value.as_str())
                    {
                        chunks.extend_from_slice(
                            sse_json(&json!({
                                "type": "response.image_generation_call.partial_image",
                                "output_index": output_index,
                                "partial_image_b64": data
                            }))
                            .as_slice(),
                        );
                    }
                }
                Some("function_call") => {
                    let call_id = item
                        .get("call_id")
                        .and_then(|value| value.as_str())
                        .unwrap_or_default();
                    let name = item
                        .get("name")
                        .and_then(|value| value.as_str())
                        .unwrap_or_default();
                    let arguments = item
                        .get("arguments")
                        .and_then(|value| value.as_str())
                        .unwrap_or("{}");
                    if !call_id.is_empty() {
                        chunks.extend_from_slice(
                            sse_json(&json!({
                                "type": "response.function_call_arguments.delta",
                                "output_index": output_index,
                                "delta": arguments
                            }))
                            .as_slice(),
                        );
                        chunks.extend_from_slice(
                            sse_json(&json!({
                                "type": "response.function_call_arguments.done",
                                "output_index": output_index,
                                "arguments": arguments
                            }))
                            .as_slice(),
                        );
                        chunks.extend_from_slice(
                            sse_json(&json!({
                                "type": "response.output_item.done",
                                "output_index": output_index,
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
                _ => {}
            }

            if item.get("type").and_then(|value| value.as_str()) != Some("function_call") {
                chunks.extend_from_slice(
                    sse_json(&json!({
                        "type": "response.output_item.done",
                        "output_index": output_index,
                        "item": item
                    }))
                    .as_slice(),
                );
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

fn text_delta_chunks(text: &str) -> impl Iterator<Item = &str> {
    text.split_inclusive('\n').filter(|chunk| !chunk.is_empty())
}

fn attach_temp_downloads(response: &mut serde_json::Value) {
    let Some(root) = response.as_object_mut() else {
        return;
    };

    let Some(images) = root
        .get_mut("images")
        .and_then(|value| value.as_array_mut())
    else {
        return;
    };

    if images.is_empty() {
        return;
    }

    let download_dir = crate::generated_temp_download_dir();
    if std::fs::create_dir_all(&download_dir).is_err() {
        return;
    }

    for image in images.iter_mut() {
        let Some(image_obj) = image.as_object_mut() else {
            continue;
        };

        let mime_type = image_obj
            .get("mime_type")
            .and_then(|value| value.as_str())
            .unwrap_or("application/octet-stream");
        let b64 = image_obj
            .get("b64_json")
            .and_then(|value| value.as_str())
            .unwrap_or("");
        if b64.is_empty() {
            continue;
        }

        let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(b64) else {
            continue;
        };

        let ext = image_extension(mime_type);
        let file_name = format!("gemini-{}.{}", Uuid::new_v4().simple(), ext);
        let file_path = download_dir.join(&file_name);
        if std::fs::write(&file_path, bytes).is_err() {
            continue;
        }

        image_obj.insert(
            "download_url".to_string(),
            serde_json::Value::String(format!("/temp-files/{}", file_name)),
        );
        image_obj.insert(
            "file_path".to_string(),
            serde_json::Value::String(file_path.to_string_lossy().into_owned()),
        );
        image_obj.insert(
            "download_name".to_string(),
            serde_json::Value::String(file_name),
        );
    }

    let enriched_images = images.clone();
    if let Some(output_items) = root
        .get_mut("output")
        .and_then(|value| value.as_array_mut())
    {
        let mut image_iter = enriched_images.into_iter();
        for item in output_items.iter_mut() {
            let is_image_item = item
                .get("type")
                .and_then(|value| value.as_str())
                .map(|kind| kind == "image_generation_call")
                .unwrap_or(false);
            if !is_image_item {
                continue;
            }
            let Some(next_image) = image_iter.next() else {
                break;
            };
            if let Some(item_obj) = item.as_object_mut() {
                item_obj.insert("result".to_string(), next_image);
            }
        }
    }
}

fn image_extension(mime_type: &str) -> &'static str {
    match mime_type {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/webp" => "webp",
        "image/gif" => "gif",
        _ => "bin",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quota_prepared_function_arguments_remain_measurable() {
        let raw = json!({"input":[
            {"type":"message","role":"user","content":"lookup"},
            {"type":"function_call","call_id":"call_lookup","name":"lookup",
             "arguments":{"context":"query","file_id":"label"}},
            {"type":"function_call_output","call_id":"call_lookup","output":"found"}
        ]});
        let payload = build_google_payload(&raw, "gemini-2.5-flash", "fixture-project").unwrap();
        assert_eq!(
            payload["request"]["contents"][1]["parts"][0]["functionCall"]["args"]["context"],
            "query"
        );
        assert!(
            crate::quota_usage::prepared_bounds("gemini", &payload)
                .unwrap()
                .input_measurable
        );
        let mut with_media = payload;
        with_media["request"]["contents"][1]["parts"][0]["inlineData"] =
            json!({"mimeType":"image/png","data":"AAAA"});
        assert!(
            !crate::quota_usage::prepared_bounds("gemini", &with_media)
                .unwrap()
                .input_measurable
        );
    }

    #[test]
    fn quota_prepared_function_schema_remains_measurable() {
        let payload = build_google_payload(
            &json!({
                "input":"describe the fields",
                "tools":[{"type":"function","name":"describe","parameters":{
                    "type":"object", "properties":{
                        "image_url":{"type":"string"}, "context":{"type":"string"}
                    }
                }}]
            }),
            "gemini-2.5-flash",
            "fixture-project",
        )
        .unwrap();
        assert!(payload
            .pointer("/request/tools/0/functionDeclarations/0/parameters")
            .is_some());
        let bounds = crate::quota_usage::prepared_bounds("gemini", &payload).unwrap();
        assert!(bounds.input_measurable);
        assert_eq!(bounds.input_upper_bound, payload.to_string().len() as u64);
    }

    #[test]
    fn quota_prepared_google_request_preserves_explicit_thinking_bound() {
        let payload = build_google_payload(
            &json!({"input":"hi","max_output_tokens":20,"thinking_config":{"thinkingBudget":10}}),
            "gemini-2.5-flash",
            "fixture-project",
        )
        .unwrap();
        assert_eq!(
            payload["request"]["generationConfig"]["thinkingConfig"]["thinkingBudget"],
            10
        );
        let bounds = crate::quota_usage::prepared_bounds("gemini", &payload).unwrap();
        assert_eq!(bounds.output_upper_bound, Some(30));
        assert_eq!(
            bounds.input_upper_bound,
            serde_json::to_vec(&payload).unwrap().len() as u64
        );
    }

    #[test]
    fn google_response_serializes_control_characters_as_valid_json() {
        let upstream = json!({
            "candidates": [{
                "content": {
                    "parts": [{
                        "text": "hello\nworld\t\u{0008}"
                    }]
                }
            }],
            "usageMetadata": {
                "promptTokenCount": 3,
                "candidatesTokenCount": 4,
                "totalTokenCount": 7
            }
        });

        let response = google_to_openai_response(&upstream, "gemini-2.5-pro");
        let body = serde_json::to_vec(&response).unwrap();
        serde_json::from_slice::<serde_json::Value>(&body).unwrap();

        let sse = String::from_utf8(render_response_sse(&response)).unwrap();
        let item_added = sse.find("response.output_item.added").unwrap();
        let part_added = sse.find("response.content_part.added").unwrap();
        let text_delta = sse.find("response.output_text.delta").unwrap();
        let item_done = sse.rfind("response.output_item.done").unwrap();
        assert!(item_added < part_added);
        assert!(part_added < text_delta);
        assert!(text_delta < item_done);
        for line in sse.lines() {
            let Some(data) = line.strip_prefix("data: ") else {
                continue;
            };
            if data == "[DONE]" {
                continue;
            }
            serde_json::from_str::<serde_json::Value>(data).unwrap();
        }
    }

    #[test]
    fn build_google_payload_passes_through_image_in_messages() {
        let request = json!({
            "model": "gemini-2.5-pro",
            "messages": [{
                "role": "user",
                "content": [
                    { "type": "text", "text": "describe" },
                    { "type": "image_url", "image_url": { "url": "data:image/png;base64,AAAA" } }
                ]
            }]
        });
        let payload = build_google_payload(&request, "gemini-2.5-pro", "proj-1").unwrap();
        let parts = payload["request"]["contents"][0]["parts"]
            .as_array()
            .unwrap();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0]["text"], "describe");
        assert_eq!(parts[1]["inline_data"]["mime_type"], "image/png");
        assert_eq!(parts[1]["inline_data"]["data"], "AAAA");
    }

    #[test]
    fn build_google_payload_passes_through_image_url() {
        let request = json!({
            "model": "gemini-2.5-pro",
            "input": "describe",
            "messages": [{
                "role": "user",
                "content": [
                    { "type": "input_image", "image_url": "https://example.com/x.png" }
                ]
            }]
        });
        let payload = build_google_payload(&request, "gemini-2.5-pro", "proj-1").unwrap();
        let parts = payload["request"]["contents"][0]["parts"]
            .as_array()
            .unwrap();
        // The string `input` produces a text part, and the message contributes the image.
        assert!(parts.iter().any(|p| p.get("text").is_some()));
        assert!(parts.iter().any(|p| p.get("file_data").is_some()));
    }

    #[test]
    fn build_google_payload_passes_through_responses_input_array() {
        let request = json!({
            "model": "gemini-2.5-pro",
            "input": [{
                "role": "user",
                "content": [
                    { "type": "input_text", "text": "describe" },
                    { "type": "input_image", "image_url": "data:image/png;base64,AAAA" }
                ]
            }]
        });
        let payload = build_google_payload(&request, "gemini-2.5-pro", "proj-1").unwrap();
        let parts = payload["request"]["contents"][0]["parts"]
            .as_array()
            .unwrap();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0]["text"], "describe");
        assert_eq!(parts[1]["inline_data"]["mime_type"], "image/png");
        assert_eq!(parts[1]["inline_data"]["data"], "AAAA");
    }

    #[test]
    fn build_google_payload_maps_responses_tool_history() {
        let request = json!({
            "model": "gemini-2.5-pro",
            "input": [
                {
                    "type": "message",
                    "role": "user",
                    "content": [{ "type": "input_text", "text": "Run ls once" }]
                },
                {
                    "type": "function_call",
                    "call_id": "call_1",
                    "name": "exec_command",
                    "arguments": "{\"cmd\":\"ls\"}"
                },
                {
                    "type": "function_call_output",
                    "call_id": "call_1",
                    "output": "file.txt\n"
                }
            ],
            "tools": [{
                "type": "function",
                "name": "exec_command",
                "parameters": {
                    "type": "object",
                    "properties": { "cmd": { "type": "string" } }
                }
            }]
        });

        let payload = build_google_payload(&request, "gemini-2.5-pro", "proj-1").unwrap();
        let contents = payload["request"]["contents"].as_array().unwrap();
        assert_eq!(contents.len(), 3);
        assert_eq!(contents[0]["role"], "user");
        assert_eq!(contents[0]["parts"][0]["text"], "Run ls once");
        assert_eq!(contents[1]["role"], "model");
        assert_eq!(
            contents[1]["parts"][0]["functionCall"]["name"],
            "exec_command"
        );
        assert_eq!(contents[1]["parts"][0]["functionCall"]["args"]["cmd"], "ls");
        assert!(contents[1]["parts"][0].get("_call_id").is_none());
        assert_eq!(contents[2]["role"], "function");
        assert_eq!(
            contents[2]["parts"][0]["functionResponse"]["name"],
            "exec_command"
        );
        assert_eq!(
            contents[2]["parts"][0]["functionResponse"]["response"]["result"],
            "file.txt\n"
        );
    }

    #[test]
    fn render_response_sse_emits_function_call_argument_events() {
        let upstream = json!({
            "candidates": [{
                "content": {
                    "parts": [{
                        "functionCall": {
                            "name": "exec_command",
                            "args": { "cmd": "pwd" }
                        }
                    }]
                }
            }]
        });

        let response = google_to_openai_response(&upstream, "gemini-2.5-pro");
        let sse = String::from_utf8(render_response_sse(&response)).unwrap();
        assert!(sse.contains("response.output_item.added"));
        assert!(sse.contains("response.function_call_arguments.delta"));
        assert!(sse.contains("response.function_call_arguments.done"));
        assert!(sse.contains("\"name\":\"exec_command\""));
        for line in sse.lines() {
            let Some(data) = line.strip_prefix("data: ") else {
                continue;
            };
            if data == "[DONE]" {
                continue;
            }
            serde_json::from_str::<serde_json::Value>(data).unwrap();
        }
    }
}
