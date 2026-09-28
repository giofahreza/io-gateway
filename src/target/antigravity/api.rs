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
    "claude-sonnet-4-5-thinking",
    "claude-opus-4-5-thinking",
    "claude-sonnet-4-5",
    "gemini-3-flash",
    "gemini-3-pro-high",
    "gemini-3-pro-low",
    "gemini-3-pro-image",
    "gemini-2.5-pro",
    "gemini-2.5-flash",
    "gemini-2.5-flash-thinking",
    "gemini-2.5-flash-lite",
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
                    "No Antigravity accounts configured",
                    "server_error",
                    None,
                ),
            )
                .into_response()
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
                .into_response()
        }
    };

    match fetch_models(&state.client, &access_token).await {
        Ok(body) => (StatusCode::OK, [("Content-Type", "application/json")], body).into_response(),
        Err(err) => {
            let data = MODEL_FALLBACKS
                .iter()
                .map(|id| {
                    json!({
                        "id": id,
                        "object": "model",
                        "created": 0,
                        "owned_by": "antigravity"
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

    if let Err(err) = validate_request_history(&request_value) {
        crate::release_api_key_budget_before_dispatch(&state);
        return (
            StatusCode::BAD_REQUEST,
            [("Content-Type", "application/json")],
            crate::source::v1::response::openai_error_body(
                &format!("Invalid request history: {}", err),
                "invalid_request_error",
                None,
            ),
        )
            .into_response();
    }

    let accounts = super::accounts::candidate_accounts(&state);
    if accounts.is_empty() {
        crate::release_api_key_budget_before_dispatch(&state);
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            [("Content-Type", "application/json")],
            crate::source::v1::response::openai_error_body(
                "No Antigravity accounts configured",
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
        let context = crate::antigravity_usage_context(
            account,
            Some(model.clone()),
            "/agw/v1/responses",
            prompt_metrics.clone(),
        );

        let payload =
            match build_google_payload(&request_value, &model, account.project_id.as_deref()) {
                Ok(payload) => payload,
                Err(err) => {
                    crate::router_reservation_cancelled(
                        &state,
                        context.provider_name,
                        &context.key,
                    );
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
        crate::record_antigravity_request(&state, &context);

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

        let upstream = match send_generate_request(
            &state.client,
            &access_token,
            &payload,
            super::auth::ANTIGRAVITY_ENDPOINTS,
            |message| {
                let state = &state;
                let context = &context;
                let payload = &payload;
                async move {
                    // The preceding endpoint already received this prompt.
                    // Settle its hold and obtain a distinct one before the
                    // same account can send it to the fallback endpoint.
                    crate::record_antigravity_error(state, context, message);
                    crate::reserve_api_key_budgets_for_prepared_dispatch(
                        state,
                        context.provider_name,
                        &context.key,
                        &serde_json::to_vec(payload).unwrap_or_default(),
                    )
                    .await?;
                    crate::record_antigravity_request(state, context);
                    Ok(())
                }
            },
        )
        .await
        {
            Ok(value) => value,
            Err(GenerateRequestError::Policy(response)) => return response,
            Err(GenerateRequestError::Upstream(status, message)) => {
                crate::record_antigravity_error(&state, &context, &message);
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
        crate::record_antigravity_success(&state, &context, &usage);
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
            "All Antigravity accounts failed".to_string(),
        )
    });
    (
        status,
        [("Content-Type", "application/json")],
        crate::source::v1::response::openai_error_body(
            &format!("All Antigravity accounts failed; last error: {}", message),
            "server_error",
            None,
        ),
    )
        .into_response()
}

/// Handle Codex's `responses/compact` request shape for Antigravity-backed
/// models. Google has no equivalent endpoint, so reuse the normal generation
/// translator and return the generated response items in the compact
/// endpoint's `{ "output": [...] }` envelope.
pub async fn compact(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    let mut request = match serde_json::from_slice::<serde_json::Value>(&body) {
        Ok(value) => value,
        Err(_) => {
            // `/responses/compact` reaches this wrapper before `responses`,
            // so it must settle a managed-key hold itself on invalid JSON.
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
    if let Some(object) = request.as_object_mut() {
        object.insert("stream".to_string(), serde_json::Value::Bool(false));
    }
    let response = responses(
        State(state.clone()),
        headers,
        Bytes::from(serde_json::to_vec(&request).unwrap_or_default()),
    )
    .await
    .into_response();
    let status = response.status();
    let response_headers = response.headers().clone();
    let response_body =
        match axum::body::to_bytes(response.into_body(), state.cfg.max_request_body_bytes).await {
            Ok(body) => body,
            Err(err) => {
                return (
                    StatusCode::BAD_GATEWAY,
                    [("Content-Type", "application/json")],
                    crate::source::v1::response::openai_error_body(
                        &format!("failed to read compaction response: {}", err),
                        "server_error",
                        None,
                    ),
                )
                    .into_response();
            }
        };
    if !status.is_success() {
        return (status, response_headers, response_body).into_response();
    }
    let value = match serde_json::from_slice::<serde_json::Value>(&response_body) {
        Ok(value) => value,
        Err(err) => {
            return (
                StatusCode::BAD_GATEWAY,
                [("Content-Type", "application/json")],
                crate::source::v1::response::openai_error_body(
                    &format!("invalid compaction response: {}", err),
                    "server_error",
                    None,
                ),
            )
                .into_response();
        }
    };
    let output = value
        .get("output")
        .cloned()
        .unwrap_or_else(|| serde_json::json!([]));
    (
        StatusCode::OK,
        [("Content-Type", "application/json")],
        serde_json::to_vec(&serde_json::json!({"output": output})).unwrap_or_default(),
    )
        .into_response()
}

async fn fetch_models(client: &reqwest::Client, access_token: &str) -> Result<Vec<u8>, String> {
    let mut last_error = "failed to fetch models from Antigravity".to_string();

    for endpoint in super::auth::ANTIGRAVITY_ENDPOINTS {
        let resp = client
            .post(format!("{}/v1internal:fetchAvailableModels", endpoint))
            .header("Authorization", format!("Bearer {}", access_token))
            .header("Content-Type", "application/json")
            .header("User-Agent", super::auth::antigravity_user_agent())
            .header(
                "X-Goog-Api-Client",
                "google-cloud-sdk vscode_cloudshelleditor/0.1",
            )
            .header(
                "Client-Metadata",
                r#"{"ideType":"IDE_UNSPECIFIED","platform":"PLATFORM_UNSPECIFIED","pluginType":"GEMINI"}"#,
            )
            .body("{}")
            .timeout(Duration::from_secs(30))
            .send()
            .await;

        let Ok(resp) = resp else {
            last_error = "failed to reach Antigravity model endpoint".to_string();
            continue;
        };

        if !resp.status().is_success() {
            last_error = format!("fetchAvailableModels returned {}", resp.status());
            continue;
        }

        let value: serde_json::Value = match resp.json().await {
            Ok(value) => value,
            Err(_) => {
                last_error = "failed to parse Antigravity models response".to_string();
                continue;
            }
        };
        let mut data = Vec::new();

        if let Some(models) = value.get("models").and_then(|v| v.as_object()) {
            for (id, model_data) in models {
                if !looks_like_antigravity_model(id) {
                    continue;
                }
                data.push(json!({
                    "id": id,
                    "object": "model",
                    "created": 0,
                    "owned_by": "antigravity",
                    "description": model_data.get("displayName").and_then(|v| v.as_str()).unwrap_or(id)
                }));
            }
        }

        if !data.is_empty() {
            return serde_json::to_vec(&json!({
                "object": "list",
                "data": data
            }))
            .map_err(|e| e.to_string());
        }
    }

    Err(last_error)
}

enum GenerateRequestError {
    Upstream(StatusCode, String),
    Policy(axum::response::Response),
}

async fn send_generate_request<F, Fut>(
    client: &reqwest::Client,
    access_token: &str,
    payload: &serde_json::Value,
    endpoints: &[&str],
    mut before_retry: F,
) -> Result<serde_json::Value, GenerateRequestError>
where
    F: FnMut(String) -> Fut,
    Fut: std::future::Future<Output = Result<(), axum::response::Response>>,
{
    let mut last_error = "all Antigravity endpoints failed".to_string();

    for (endpoint_index, endpoint) in endpoints.iter().enumerate() {
        if endpoint_index > 0 {
            before_retry(last_error.clone())
                .await
                .map_err(GenerateRequestError::Policy)?;
        }
        let resp = client
            .post(format!("{}/v1internal:generateContent", endpoint))
            .header("Authorization", format!("Bearer {}", access_token))
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .header("User-Agent", super::auth::antigravity_user_agent())
            .header(
                "X-Goog-Api-Client",
                "google-cloud-sdk vscode_cloudshelleditor/0.1",
            )
            .header(
                "Client-Metadata",
                r#"{"ideType":"IDE_UNSPECIFIED","platform":"PLATFORM_UNSPECIFIED","pluginType":"GEMINI"}"#,
            )
            .body(payload.to_string())
            .timeout(Duration::from_secs(180))
            .send()
            .await;

        let Ok(resp) = resp else {
            last_error = "failed to reach Antigravity generateContent endpoint".to_string();
            continue;
        };

        let status = resp.status();
        let retry_after = resp
            .headers()
            .get("retry-after")
            .and_then(|value| value.to_str().ok())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string);
        let text = match resp.text().await {
            Ok(text) => text,
            Err(err) => {
                last_error = err.to_string();
                continue;
            }
        };
        if !status.is_success() {
            last_error = match retry_after {
                Some(retry_after) => format!(
                    "generateContent returned {}: {}; retry-after={}",
                    status, text, retry_after
                ),
                None => format!("generateContent returned {}: {}", status, text),
            };
            // A malformed transcript is deterministic. The second Google
            // endpoint and every other account will reject the same body.
            if status == StatusCode::BAD_REQUEST {
                return Err(GenerateRequestError::Upstream(
                    StatusCode::BAD_REQUEST,
                    last_error,
                ));
            }
            continue;
        }

        let value: serde_json::Value = match serde_json::from_str(&text) {
            Ok(value) => value,
            Err(err) => {
                last_error = err.to_string();
                continue;
            }
        };
        return Ok(value);
    }

    Err(GenerateRequestError::Upstream(
        StatusCode::BAD_GATEWAY,
        last_error,
    ))
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
    project_id: Option<&str>,
) -> Result<serde_json::Value, String> {
    let contents = build_google_contents(request_value)?;
    let instructions = request_value
        .get("instructions")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
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
        .and_then(|v| v.as_u64())
    {
        generation_config.insert("maxOutputTokens".to_string(), json!(max_output_tokens));
    }
    if let Some(thinking) = request_value
        .get("thinking_config")
        .or_else(|| request_value.get("thinkingConfig"))
        .filter(|value| value.is_object())
    {
        generation_config.insert("thinkingConfig".to_string(), thinking.clone());
    }
    if let Some(temperature) = request_value.get("temperature").and_then(|v| v.as_f64()) {
        generation_config.insert("temperature".to_string(), json!(temperature));
    }
    if let Some(top_p) = request_value.get("top_p").and_then(|v| v.as_f64()) {
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
        "project": project_id.unwrap_or("rising-fact-p41fc"),
        "model": model,
        "request": request,
        "userAgent": "antigravity",
        "requestId": format!("agent-{}", Uuid::new_v4())
    }))
}

/// Convert the Codex/OpenAI `tools` array into Google's
/// `functionDeclarations` shape. Each tool is expected to have either a
/// `function` wrapper (OpenAI chat-completions style) or a flat
/// `{name, description, parameters}` (Responses style). The returned
/// value is the inner `tools` array (the gateway wraps it in
/// `request.tools` before posting upstream).
fn build_google_tools(request_value: &serde_json::Value) -> Option<serde_json::Value> {
    let tools = request_value.get("tools")?.as_array()?;
    let mut declarations: Vec<serde_json::Value> = Vec::new();
    for tool in tools {
        // Skip image-generation tools — those are handled separately via
        // responseModalities in build_google_payload.
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
            // {"type": "function", "function": {"name": "shell"}} → ANY with allowed
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

/// Build a Google Generative AI `parts` array from a Codex/OpenAI
/// request. Supports text, image (data URL or remote URL), and a mix of
/// both. Accepts both `input` (string) and `messages[]` shapes.
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

    if role == "tool" || role == "function" {
        let call_id = message
            .get("tool_call_id")
            .and_then(|value| value.as_str())
            .unwrap_or("");
        let name = message
            .get("name")
            .and_then(|value| value.as_str())
            .map(str::to_string)
            .or_else(|| lookup_function_name(entries, call_id))
            .unwrap_or_else(|| "tool".to_string());
        let output = message_content_text(message.get("content")).unwrap_or_default();
        entries.push((
            "user".to_string(),
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
    if let Some(function_call) = message.get("function_call") {
        let Some(name) = function_call.get("name").and_then(|value| value.as_str()) else {
            return;
        };
        let arguments = function_call
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| json!({}));
        let args_value = if let Some(arguments) = arguments.as_str() {
            serde_json::from_str::<Value>(arguments).unwrap_or_else(|_| json!({ "raw": arguments }))
        } else {
            arguments
        };
        let synthetic_call_id = format!("legacy_call_{}", entries.len());
        parts.push(function_call_part(
            name,
            args_value,
            &synthetic_call_id,
            thought_signature_from_value(function_call),
        ));
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
            // Google wants `args` as a JSON object (google.protobuf.Struct),
            // not a string. Codex SDK sends the arguments as a JSON string,
            // so we parse it. If parsing fails, fall back to the raw value.
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
                "user".to_string(),
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
                "tool" | "function" => "user",
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
            let Some(fc) = part.get("functionCall") else {
                continue;
            };
            if part.get("_call_id").and_then(|v| v.as_str()) == Some(call_id) {
                if let Some(name) = fc.get("name").and_then(|v| v.as_str()) {
                    return Some(name.to_string());
                }
            }
        }
    }
    None
}

/// Validate the ordered Codex transcript before converting it to Google's
/// stricter function-call history format. In particular, do not silently
/// invent a tool name for an orphaned output or send an unanswered call.
fn validate_request_history(request_value: &Value) -> Result<(), String> {
    use std::collections::HashMap;

    let mut calls: HashMap<String, String> = HashMap::new();
    let mut outputs: HashMap<String, ()> = HashMap::new();
    let mut legacy_calls: HashMap<String, usize> = HashMap::new();

    if let Some(items) = request_value.get("input").and_then(Value::as_array) {
        for item in items {
            match item.get("type").and_then(Value::as_str).unwrap_or("") {
                "function_call" => {
                    let call_id = item
                        .get("call_id")
                        .or_else(|| item.get("id"))
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    let name = item.get("name").and_then(Value::as_str).unwrap_or("");
                    register_function_call(&mut calls, call_id, name)?;
                }
                "function_call_output" => {
                    let call_id = item.get("call_id").and_then(Value::as_str).unwrap_or("");
                    register_function_output(&calls, &mut outputs, call_id)?;
                }
                _ => {}
            }
        }
    }

    if let Some(messages) = request_value.get("messages").and_then(Value::as_array) {
        for message in messages {
            let role = message.get("role").and_then(Value::as_str).unwrap_or("");
            if role == "assistant" {
                if let Some(function_call) = message.get("function_call") {
                    let name = function_call
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .trim();
                    if name.is_empty() {
                        return Err("function_call is missing name".to_string());
                    }
                    *legacy_calls.entry(name.to_string()).or_default() += 1;
                }
            }
            if let Some(tool_calls) = message.get("tool_calls").and_then(Value::as_array) {
                for tool_call in tool_calls {
                    let call_id = tool_call.get("id").and_then(Value::as_str).unwrap_or("");
                    let function = tool_call.get("function");
                    let name = function
                        .and_then(|value| value.get("name"))
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    register_function_call(&mut calls, call_id, name)?;
                }
            }
            if role == "tool" {
                let call_id = message
                    .get("tool_call_id")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                register_function_output(&calls, &mut outputs, call_id)?;
            }
            if role == "function" {
                let name = message
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .trim();
                if name.is_empty() {
                    return Err("function message is missing name".to_string());
                }
                if let Some(count) = legacy_calls.get_mut(name) {
                    if *count > 0 {
                        *count -= 1;
                        continue;
                    }
                }
                let call_id = message
                    .get("tool_call_id")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if !call_id.is_empty() {
                    register_function_output(&calls, &mut outputs, call_id)?;
                } else {
                    register_function_output_by_name(&calls, &mut outputs, name)?;
                }
            }
        }
    }

    if let Some((name, _count)) = legacy_calls.iter().find(|(_, count)| **count > 0) {
        return Err(format!("function_call '{}' has no matching output", name));
    }

    if let Some((call_id, _)) = calls
        .iter()
        .find(|(call_id, _)| !outputs.contains_key(*call_id))
    {
        return Err(format!(
            "function_call '{}' has no matching output",
            call_id
        ));
    }
    Ok(())
}

fn register_function_call(
    calls: &mut std::collections::HashMap<String, String>,
    call_id: &str,
    name: &str,
) -> Result<(), String> {
    let call_id = call_id.trim();
    let name = name.trim();
    if call_id.is_empty() {
        return Err("function_call is missing call_id".to_string());
    }
    if name.is_empty() {
        return Err(format!("function_call '{}' is missing name", call_id));
    }
    if calls
        .insert(call_id.to_string(), name.to_string())
        .is_some()
    {
        return Err(format!("duplicate function_call '{}/call_id'", call_id));
    }
    Ok(())
}

fn register_function_output(
    calls: &std::collections::HashMap<String, String>,
    outputs: &mut std::collections::HashMap<String, ()>,
    call_id: &str,
) -> Result<(), String> {
    let call_id = call_id.trim();
    if call_id.is_empty() {
        return Err("function_call_output is missing call_id".to_string());
    }
    if !calls.contains_key(call_id) {
        return Err(format!(
            "function_call_output '{}' has no matching function_call",
            call_id
        ));
    }
    if outputs.insert(call_id.to_string(), ()).is_some() {
        return Err(format!("duplicate function_call_output '{}'", call_id));
    }
    Ok(())
}

fn register_function_output_by_name(
    calls: &std::collections::HashMap<String, String>,
    outputs: &mut std::collections::HashMap<String, ()>,
    name: &str,
) -> Result<(), String> {
    let Some(call_id) = calls
        .iter()
        .find(|(call_id, call_name)| call_name == &name && !outputs.contains_key(*call_id))
        .map(|(call_id, _)| call_id.as_str())
    else {
        return Err(format!(
            "function message '{}' has no matching function_call",
            name
        ));
    };
    register_function_output(calls, outputs, call_id)
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

/// Backwards-compatible wrapper that builds a single-turn `parts`
/// array. Internally we now build a full `contents` array so we can
/// preserve multi-turn tool-call history; the request builder just
/// flattens the last user turn's parts for callers that only need a
/// single part list.
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
        PartKind::Other(_) => {}
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
        .and_then(|v| v.as_array())
        .map(|tools| {
            tools.iter().any(|tool| {
                tool.get("type")
                    .and_then(|v| v.as_str())
                    .map(|kind| kind == "image_generation")
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false)
}

fn looks_like_antigravity_model(model_id: &str) -> bool {
    let lower = model_id.to_ascii_lowercase();
    lower.contains("claude") || lower.contains("gemini") || lower.contains("gpt")
}

fn google_to_openai_response(value: &serde_json::Value, model: &str) -> serde_json::Value {
    let response = value.get("response").unwrap_or(value);
    let parts = response
        .get("candidates")
        .and_then(|v| v.as_array())
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
        if let Some(text) = part.get("text").and_then(|v| v.as_str()) {
            if !text.trim().is_empty() {
                output_text.push_str(text);
            }
        }

        if let Some(inline_data) = part.get("inlineData").and_then(|v| v.as_object()) {
            let mime_type = inline_data
                .get("mimeType")
                .and_then(|v| v.as_str())
                .unwrap_or("image/png");
            let data = inline_data
                .get("data")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if mime_type.starts_with("image/") && !data.is_empty() {
                images.push(json!({
                    "mime_type": mime_type,
                    "b64_json": data
                }));
            }
        }

        if let Some(fc) = part.get("functionCall").and_then(|v| v.as_object()) {
            let name = fc
                .get("name")
                .and_then(|v| v.as_str())
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
            if let Some(signature) = part.get("thoughtSignature").and_then(|v| v.as_str()) {
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
            "input_tokens": usage.get("promptTokenCount").and_then(|v| v.as_u64()).unwrap_or(0),
            "output_tokens": usage.get("candidatesTokenCount").and_then(|v| v.as_u64()).unwrap_or(0),
            "total_tokens": usage.get("totalTokenCount").and_then(|v| v.as_u64()).unwrap_or(
                usage.get("promptTokenCount").and_then(|v| v.as_u64()).unwrap_or(0)
                    + usage.get("candidatesTokenCount").and_then(|v| v.as_u64()).unwrap_or(0)
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

    if let Some(output) = response.get("output").and_then(|v| v.as_array()) {
        for (idx, item) in output.iter().enumerate() {
            let mut in_progress = item.clone();
            if let Some(object) = in_progress.as_object_mut() {
                if object.contains_key("status") {
                    object.insert("status".to_string(), json!("in_progress"));
                }
            }
            chunks.extend_from_slice(
                sse_json(&json!({
                    "type": "response.output_item.added",
                    "output_index": idx,
                    "item": in_progress
                }))
                .as_slice(),
            );

            match item.get("type").and_then(|v| v.as_str()) {
                Some("message") => {
                    let item_id = item.get("id").and_then(|v| v.as_str()).unwrap_or_default();
                    if let Some(content) = item.get("content").and_then(|v| v.as_array()) {
                        for (content_index, part) in content.iter().enumerate() {
                            if part.get("type").and_then(|v| v.as_str()) != Some("output_text") {
                                continue;
                            }
                            let text = part.get("text").and_then(|v| v.as_str()).unwrap_or("");
                            chunks.extend_from_slice(
                                sse_json(&json!({
                                    "type": "response.content_part.added",
                                    "item_id": item_id,
                                    "output_index": idx,
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
                                        "output_index": idx,
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
                                    "output_index": idx,
                                    "content_index": content_index,
                                    "text": text
                                }))
                                .as_slice(),
                            );
                            chunks.extend_from_slice(
                                sse_json(&json!({
                                    "type": "response.content_part.done",
                                    "item_id": item_id,
                                    "output_index": idx,
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
                        .and_then(|v| v.as_str())
                    {
                        chunks.extend_from_slice(
                            sse_json(&json!({
                                "type": "response.image_generation_call.partial_image",
                                "output_index": idx,
                                "partial_image_b64": data
                            }))
                            .as_slice(),
                        );
                    }
                }
                Some("function_call") => {
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
                                "type": "response.function_call_arguments.delta",
                                "output_index": idx,
                                "delta": arguments
                            }))
                            .as_slice(),
                        );
                        chunks.extend_from_slice(
                            sse_json(&json!({
                                "type": "response.function_call_arguments.done",
                                "output_index": idx,
                                "arguments": arguments
                            }))
                            .as_slice(),
                        );
                        chunks.extend_from_slice(
                            sse_json(&json!({
                                "type": "response.output_item.done",
                                "output_index": idx,
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

            if item.get("type").and_then(|v| v.as_str()) != Some("function_call") {
                chunks.extend_from_slice(
                    sse_json(&json!({
                        "type": "response.output_item.done",
                        "output_index": idx,
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
        let file_name = format!("agw-{}.{}", Uuid::new_v4().simple(), ext);
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
        let payload =
            build_google_payload(&raw, "gemini-2.5-flash", Some("fixture-project")).unwrap();
        assert_eq!(
            payload["request"]["contents"][1]["parts"][0]["functionCall"]["args"]["context"],
            "query"
        );
        assert!(
            crate::quota_usage::prepared_bounds("antigravity", &payload)
                .unwrap()
                .input_measurable
        );
        let mut with_media = payload;
        with_media["request"]["contents"][1]["parts"][0]["fileData"] =
            json!({"fileUri":"https://example.test/a.png"});
        assert!(
            !crate::quota_usage::prepared_bounds("antigravity", &with_media)
                .unwrap()
                .input_measurable
        );
    }

    #[test]
    fn quota_prepared_function_schema_remains_measurable() {
        let payload = build_google_payload(
            &json!({
                "input":"describe the fields",
                "tools":[{"type":"function","function":{"name":"describe","parameters":{
                    "type":"object", "properties":{
                        "file_id":{"type":"string"}, "previous_response_id":{"type":"string"}
                    }
                }}}]
            }),
            "gemini-2.5-flash",
            Some("fixture-project"),
        )
        .unwrap();
        assert!(payload
            .pointer("/request/tools/0/functionDeclarations/0/parameters")
            .is_some());
        let bounds = crate::quota_usage::prepared_bounds("antigravity", &payload).unwrap();
        assert!(bounds.input_measurable);
        assert_eq!(bounds.input_upper_bound, payload.to_string().len() as u64);
    }

    #[test]
    fn quota_prepared_google_request_preserves_explicit_thinking_bound() {
        let payload = build_google_payload(
            &json!({"input":"hi","max_output_tokens":20,"thinkingConfig":{"thinkingBudget":10}}),
            "gemini-2.5-flash",
            Some("fixture-project"),
        )
        .unwrap();
        assert_eq!(
            payload["request"]["generationConfig"]["thinkingConfig"]["thinkingBudget"],
            10
        );
        let bounds = crate::quota_usage::prepared_bounds("antigravity", &payload).unwrap();
        assert_eq!(bounds.output_upper_bound, Some(30));
        assert_eq!(
            bounds.input_upper_bound,
            serde_json::to_vec(&payload).unwrap().len() as u64
        );
    }

    async fn endpoint_retry_budget_fixture(budget_limit: u64) {
        use crate::api_key_policy_store::{
            input_token_budget_summary, reserve_input_tokens, settle_input_token_reservation,
            ApiKeyInputTokenReservationRequest, ApiKeyInputTokenReservationResult,
            ApiKeyInputTokenSettlement,
        };
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };

        let calls = Arc::new(AtomicUsize::new(0));
        let handler_calls = calls.clone();
        let router = axum::Router::new().route(
            "/v1internal:generateContent",
            axum::routing::post(move || {
                let calls = handler_calls.clone();
                async move {
                    if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                        (
                            StatusCode::BAD_GATEWAY,
                            axum::Json(json!({"error":"retry endpoint"})),
                        )
                    } else {
                        (
                            StatusCode::OK,
                            axum::Json(
                                json!({"candidates":[],"usageMetadata":{"promptTokenCount":3}}),
                            ),
                        )
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let directory =
            std::env::temp_dir().join(format!("io-gateway-antigravity-retry-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        let cfg: crate::Config = serde_json::from_value(json!({
            "listen":"127.0.0.1:0", "upstream_base": endpoint,
            "proxy_api_key":"", "tokens":[], "auth_dir":directory,
        }))
        .unwrap();
        let budget = crate::api_keys::ApiKeyInputTokenBudget {
            limit: budget_limit,
            period: crate::api_keys::ApiKeyBudgetPeriod::Lifetime,
        };
        let reserve = |id: &str| {
            reserve_input_tokens(
                &cfg,
                &ApiKeyInputTokenReservationRequest {
                    reservation_id: id.to_owned(),
                    request_id: "request".to_owned(),
                    api_key_id: "key".to_owned(),
                    budget: budget.clone(),
                    reserved_input_tokens: 100,
                },
            )
            .unwrap()
        };
        assert!(matches!(
            reserve("first"),
            ApiKeyInputTokenReservationResult::Reserved { .. }
        ));

        let result = send_generate_request(
            &reqwest::Client::new(),
            "mock-access-token",
            &json!({"input":"hello"}),
            &[&endpoint, &endpoint],
            |_message| {
                settle_input_token_reservation(
                    &cfg,
                    "first",
                    ApiKeyInputTokenSettlement::Commit {
                        actual_input_tokens: None,
                    },
                )
                .unwrap();
                let allowed = matches!(
                    reserve("second"),
                    ApiKeyInputTokenReservationResult::Reserved { .. }
                );
                async move {
                    if allowed {
                        Ok(())
                    } else {
                        Err(StatusCode::TOO_MANY_REQUESTS.into_response())
                    }
                }
            },
        )
        .await;
        if budget_limit < 200 {
            assert!(
                matches!(result, Err(GenerateRequestError::Policy(ref response)) if response.status() == StatusCode::TOO_MANY_REQUESTS)
            );
            assert_eq!(
                calls.load(Ordering::SeqCst),
                1,
                "fallback must not reach upstream without its own budget"
            );
        } else {
            assert!(result.is_ok());
            assert_eq!(calls.load(Ordering::SeqCst), 2);
            settle_input_token_reservation(
                &cfg,
                "second",
                ApiKeyInputTokenSettlement::Commit {
                    actual_input_tokens: Some(3),
                },
            )
            .unwrap();
        }
        let summary = input_token_budget_summary(&cfg, "key", &budget).unwrap();
        assert_eq!(
            summary.committed_input_tokens,
            if budget_limit < 200 { 100 } else { 200 }
        );
        assert_eq!(summary.reserved_input_tokens, 0);
        server.abort();
        let _ = server.await;
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn endpoint_fallback_is_denied_when_only_one_dispatch_fits_budget() {
        endpoint_retry_budget_fixture(100).await;
    }

    #[tokio::test]
    async fn endpoint_fallback_commits_each_dispatched_prompt_separately() {
        endpoint_retry_budget_fixture(200).await;
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

        let response = google_to_openai_response(&upstream, "gemini-3-pro-high");
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
        let payload = build_google_payload(&request, "gemini-2.5-pro", Some("proj-1")).unwrap();
        let parts = payload["request"]["contents"][0]["parts"]
            .as_array()
            .unwrap();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0]["text"], "describe");
        assert_eq!(parts[1]["inline_data"]["mime_type"], "image/png");
        assert_eq!(parts[1]["inline_data"]["data"], "AAAA");
    }

    #[test]
    fn build_google_payload_passes_through_image_with_input_string() {
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
        let payload = build_google_payload(&request, "gemini-2.5-pro", Some("proj-1")).unwrap();
        let parts = payload["request"]["contents"][0]["parts"]
            .as_array()
            .unwrap();
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
        let payload = build_google_payload(&request, "gemini-2.5-pro", Some("proj-1")).unwrap();
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
            "model": "gemini-pro-agent",
            "input": [
                {
                    "type": "message",
                    "role": "user",
                    "content": [{ "type": "input_text", "text": "Run httrack once" }]
                },
                {
                    "type": "function_call",
                    "call_id": "call_1",
                    "name": "exec_command",
                    "arguments": "{\"cmd\":\"httrack https://example.com -O .\"}"
                },
                {
                    "type": "function_call_output",
                    "call_id": "call_1",
                    "output": "Process running with session ID 76254"
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

        let payload = build_google_payload(&request, "gemini-pro-agent", Some("proj-1")).unwrap();
        let contents = payload["request"]["contents"].as_array().unwrap();
        assert_eq!(contents.len(), 3);
        assert_eq!(contents[0]["role"], "user");
        assert_eq!(contents[0]["parts"][0]["text"], "Run httrack once");
        assert_eq!(contents[1]["role"], "model");
        assert_eq!(
            contents[1]["parts"][0]["functionCall"]["name"],
            "exec_command"
        );
        assert_eq!(
            contents[1]["parts"][0]["functionCall"]["args"]["cmd"],
            "httrack https://example.com -O ."
        );
        assert!(contents[1]["parts"][0].get("_call_id").is_none());
        assert_eq!(contents[2]["role"], "user");
        assert_eq!(
            contents[2]["parts"][0]["functionResponse"]["name"],
            "exec_command"
        );
        assert_eq!(
            contents[2]["parts"][0]["functionResponse"]["response"]["result"],
            "Process running with session ID 76254"
        );
    }

    #[test]
    fn legacy_function_message_becomes_user_function_response() {
        let request = json!({
            "model": "gemini-pro-agent",
            "messages": [
                {
                    "role": "assistant",
                    "tool_calls": [{
                        "id": "call_legacy",
                        "type": "function",
                        "function": {"name": "exec_command", "arguments": "{\"cmd\":\"pwd\"}"}
                    }]
                },
                {
                    "role": "function",
                    "name": "exec_command",
                    "content": "..."
                }
            ]
        });
        let payload = build_google_payload(&request, "gemini-pro-agent", Some("proj-1")).unwrap();
        let contents = payload["request"]["contents"].as_array().unwrap();
        assert_eq!(contents[1]["role"], "user");
        assert!(contents.iter().all(|entry| entry["role"] != "function"));
        assert_eq!(
            contents[1]["parts"][0]["functionResponse"]["name"],
            "exec_command"
        );
    }

    #[test]
    fn legacy_function_history_is_validated_by_function_name() {
        let request = json!({
            "model": "gemini-pro-agent",
            "messages": [
                {
                    "role": "assistant",
                    "function_call": {
                        "name": "exec_command",
                        "arguments": "{\"cmd\":\"pwd\"}"
                    }
                },
                {
                    "role": "function",
                    "name": "exec_command",
                    "content": "ok"
                }
            ]
        });

        assert!(validate_request_history(&request).is_ok());
        let payload = build_google_payload(&request, "gemini-pro-agent", Some("proj-1")).unwrap();
        let contents = payload["request"]["contents"].as_array().unwrap();
        assert_eq!(contents[0]["role"], "model");
        assert_eq!(
            contents[0]["parts"][0]["functionCall"]["name"],
            "exec_command"
        );
        assert_eq!(contents[1]["role"], "user");
        assert_eq!(
            contents[1]["parts"][0]["functionResponse"]["name"],
            "exec_command"
        );
    }

    #[test]
    fn rejects_unanswered_legacy_function_call() {
        let request = json!({
            "model": "gemini-pro-agent",
            "messages": [{
                "role": "assistant",
                "function_call": {"name": "exec_command", "arguments": "{}"}
            }]
        });

        let error = validate_request_history(&request).unwrap_err();
        assert!(error.contains("has no matching output"));
    }

    #[test]
    fn build_google_payload_forwards_function_declarations() {
        let request = json!({
            "model": "claude-sonnet-4-6",
            "input": "what is the weather",
            "tools": [{
                "type": "function",
                "function": {
                    "name": "shell",
                    "description": "Run a shell command",
                    "parameters": {
                        "type": "object",
                        "properties": { "command": { "type": "string" } }
                    }
                }
            }],
            "tool_choice": "auto"
        });
        let payload = build_google_payload(&request, "claude-sonnet-4-6", Some("proj-1")).unwrap();
        let tools = payload["request"]["tools"]
            .as_array()
            .expect("tools forwarded");
        assert_eq!(tools.len(), 1);
        let decls = tools[0]["functionDeclarations"]
            .as_array()
            .expect("functionDeclarations");
        assert_eq!(decls.len(), 1);
        assert_eq!(decls[0]["name"], "shell");
        assert_eq!(decls[0]["description"], "Run a shell command");
        assert_eq!(
            decls[0]["parameters"]["properties"]["command"]["type"],
            "string"
        );
        assert_eq!(
            payload["request"]["toolConfig"]["functionCallingConfig"]["mode"],
            "AUTO"
        );
    }

    #[test]
    fn rejects_orphaned_function_output_before_conversion() {
        let request = json!({
            "model": "gemini-pro-agent",
            "input": [{
                "type": "function_call_output",
                "call_id": "call_missing",
                "output": "done"
            }]
        });
        let error = validate_request_history(&request).unwrap_err();
        assert!(error.contains("has no matching function_call"));
    }

    #[test]
    fn rejects_unanswered_function_call_before_conversion() {
        let request = json!({
            "model": "gemini-pro-agent",
            "input": [{
                "type": "function_call",
                "call_id": "call_1",
                "name": "exec_command",
                "arguments": "{}"
            }]
        });
        let error = validate_request_history(&request).unwrap_err();
        assert!(error.contains("has no matching output"));
    }

    #[test]
    fn accepts_ordered_function_call_history() {
        let request = json!({
            "model": "gemini-pro-agent",
            "input": [
                { "type": "function_call", "call_id": "call_1", "name": "shell" },
                { "type": "function_call_output", "call_id": "call_1", "output": "ok" }
            ]
        });
        assert!(validate_request_history(&request).is_ok());
    }

    #[test]
    fn build_google_payload_skips_image_generation_tools() {
        let request = json!({
            "model": "gemini-3-pro-image",
            "input": "draw a cat",
            "tools": [{
                "type": "image_generation",
                "function": { "name": "draw", "parameters": {} }
            }]
        });
        let payload = build_google_payload(&request, "gemini-3-pro-image", Some("proj-1")).unwrap();
        // image_generation is handled via responseModalities, so the
        // google functionDeclarations list should be empty (no tools key).
        assert!(payload["request"].get("tools").is_none());
    }

    #[test]
    fn google_to_openai_response_emits_function_call_items() {
        let upstream = json!({
            "response": {
                "candidates": [{
                    "content": {
                        "parts": [
                            { "text": "I will run shell" },
                            { "functionCall": { "name": "shell", "args": "{\"command\":\"ls\"}" } }
                        ]
                    }
                }],
                "usageMetadata": {
                    "promptTokenCount": 10,
                    "candidatesTokenCount": 8,
                    "totalTokenCount": 18
                }
            }
        });
        let response = google_to_openai_response(&upstream, "claude-sonnet-4-6");
        let output = response["output"].as_array().expect("output");
        assert_eq!(output.len(), 2);
        assert_eq!(output[0]["type"], "message");
        assert_eq!(output[0]["content"][0]["text"], "I will run shell");
        assert_eq!(output[1]["type"], "function_call");
        assert_eq!(output[1]["name"], "shell");
        assert_eq!(output[1]["arguments"], "{\"command\":\"ls\"}");
        assert!(output[1]["call_id"].as_str().unwrap().starts_with("call_"));
    }
}
