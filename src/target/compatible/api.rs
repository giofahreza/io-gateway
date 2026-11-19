use super::accounts::CompatibleAccount;
use axum::{
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use bytes::Bytes;
use serde_json::{json, Value};

pub fn endpoint(base: &str, protocol: &str, resource: &str) -> String {
    let base = base.trim_end_matches('/');
    // Accept either a base URL or the full resource URL without duplicating suffixes.
    for suffix in ["/chat/completions", "/messages", "/models"] {
        if let Some(root) = base.strip_suffix(suffix) {
            return endpoint(root, protocol, resource);
        }
    }
    if protocol == "anthropic" && !base.ends_with("/v1") {
        format!("{base}/v1/{resource}")
    } else {
        format!("{base}/{resource}")
    }
}

fn error(state: &crate::AppState, status: StatusCode, message: impl Into<String>) -> Response {
    crate::source_error_response(
        state.request_source_api,
        status,
        message.into(),
        "api_error",
        None,
        "api_error",
    )
}

fn prepare(
    account: &CompatibleAccount,
    path: &str,
    raw: &Value,
    model: &str,
) -> Result<(String, Value, bool), String> {
    let messages = path == "anthropic/v1/messages";
    let native = if messages {
        account.anthropic_base_url.is_some()
    } else {
        account.base_url.is_some() && path == "chat/completions"
    };
    let anthropic = if messages {
        account.anthropic_base_url.is_some()
    } else {
        account.base_url.is_none()
    };
    let mut payload = if native {
        raw.clone()
    } else if anthropic {
        crate::target::claude::api::responses_to_anthropic_messages(raw, model, false)?
    } else if messages {
        crate::target::glm::anthropic::anthropic_messages_to_chat_completions(raw, model)?
    } else {
        crate::target::glm::api::build_chat_completions_payload(raw, model)?
    };
    payload["model"] = json!(model);
    // Native protocol streams are forwarded as they arrive. Translated streams use the
    // existing Responses/Anthropic event encoders after collecting one complete response.
    payload["stream"] = json!(native && raw["stream"].as_bool().unwrap_or(false));
    if payload["stream"] == true && !anthropic {
        payload["stream_options"] = json!({"include_usage":true});
    }
    let (base, protocol, resource) = if anthropic {
        (
            account
                .anthropic_base_url
                .as_deref()
                .ok_or("Anthropic URL is missing")?,
            "anthropic",
            "messages",
        )
    } else {
        (
            account.base_url.as_deref().ok_or("OpenAI URL is missing")?,
            "openai",
            "chat/completions",
        )
    };
    Ok((endpoint(base, protocol, resource), payload, anthropic))
}

pub(crate) fn policy_metrics(
    state: &crate::AppState,
    path: &str,
    body: &Bytes,
) -> Option<crate::PromptMetrics> {
    let raw: Value = serde_json::from_slice(body).ok()?;
    let model = raw["model"].as_str()?;
    let accounts = state.compatible_accounts.lock().unwrap().clone();
    let mut result = None;
    for account in accounts
        .iter()
        .filter(|a| a.enabled && a.models.iter().any(|m| m == model))
    {
        if let Ok((_, payload, _)) = prepare(account, path, &raw, model) {
            let metrics = crate::prompt_metrics_from_request_value(&payload);
            result = Some(match result {
                Some(current) => crate::more_conservative_prompt_metrics(current, metrics),
                None => metrics,
            });
        }
    }
    result
}

pub async fn dispatch(
    state: crate::AppState,
    headers: HeaderMap,
    path: &str,
    body: Bytes,
) -> Response {
    if !crate::check_api_key(&state, &headers) {
        return error(&state, StatusCode::UNAUTHORIZED, "Invalid gateway API key");
    }
    if !matches!(
        path,
        "chat/completions" | "responses" | "anthropic/v1/messages"
    ) {
        crate::release_api_key_budget_before_dispatch(&state);
        return error(
            &state,
            StatusCode::BAD_REQUEST,
            "Compatible APIs support chat completions, responses, and messages",
        );
    }
    let raw: Value = match serde_json::from_slice(&body) {
        Ok(value) => value,
        Err(_) => return error(&state, StatusCode::BAD_REQUEST, "Invalid JSON body"),
    };
    let Some(model) = raw["model"].as_str() else {
        return error(&state, StatusCode::BAD_REQUEST, "Model is required");
    };
    let stream = raw["stream"].as_bool().unwrap_or(false);
    let accounts = super::accounts::candidate_accounts(&state, model);
    if accounts.is_empty() {
        crate::release_api_key_budget_before_dispatch(&state);
        return error(
            &state,
            StatusCode::SERVICE_UNAVAILABLE,
            "No enabled compatible account offers this model",
        );
    }
    for (index, account) in accounts.iter().enumerate() {
        let (url, payload, anthropic) = match prepare(account, path, &raw, model) {
            Ok(v) => v,
            Err(e) => {
                crate::release_api_key_budget_before_dispatch(&state);
                return error(&state, StatusCode::BAD_REQUEST, e);
            }
        };
        let prompt = crate::prompt_metrics_from_request_value(&payload);
        let context =
            crate::compatible_usage_context(account, Some(model.into()), path, prompt.clone());
        if let Err(response) = crate::api_key_quota_runtime::reserve_api_key_budgets_for_prepared_dispatch_with_protocol(
            &state,
            context.provider_name,
            &context.key,
            &serde_json::to_vec(&payload).unwrap_or_default(),
            if anthropic {
                crate::quota_usage::PreparedProtocol::AnthropicMessages
            } else {
                crate::quota_usage::PreparedProtocol::ChatCompletions
            },
        )
        .await
        {
            return response;
        }
        crate::record_request_started(&state, &context);
        if path == "responses" && stream && !anthropic {
            match crate::target::glm::api::stream_chat_completions(
                &state,
                &account.api_key,
                &url,
                &payload,
                &context,
                model,
                &headers,
            )
            .await
            {
                Ok(response) => return response,
                Err((status, message)) => {
                    let message = message.replace(&account.api_key, "[REDACTED]");
                    crate::record_request_error(&state, &context, &message);
                    if index + 1 < accounts.len()
                        && crate::should_retry_account_error(status, &message)
                    {
                        continue;
                    }
                    return error(&state, status, message);
                }
            }
        }
        let mut request = state
            .client
            .post(url)
            .bearer_auth(&account.api_key)
            .header("Content-Type", "application/json")
            .timeout(std::time::Duration::from_secs(180));
        if anthropic {
            request = request.header("x-api-key", &account.api_key).header(
                "anthropic-version",
                headers
                    .get("anthropic-version")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("2023-06-01"),
            );
            if let Some(beta) = headers.get("anthropic-beta") {
                request = request.header("anthropic-beta", beta);
            }
        }
        let upstream = match request.json(&payload).send().await {
            Ok(response) => response,
            Err(_) => {
                crate::record_request_error(&state, &context, "Compatible API connection failed");
                if index + 1 < accounts.len() {
                    continue;
                }
                return error(
                    &state,
                    StatusCode::BAD_GATEWAY,
                    "Compatible API connection failed",
                );
            }
        };
        let status = upstream.status();
        if !status.is_success() {
            // Upstream errors can echo the Authorization header or key; redact before logging.
            let text = upstream
                .text()
                .await
                .unwrap_or_default()
                .replace(&account.api_key, "[REDACTED]");
            let message = format!(
                "Compatible API returned {status}: {}",
                text.chars().take(2000).collect::<String>()
            );
            crate::record_request_error(&state, &context, &message);
            if index + 1 < accounts.len() && crate::should_retry_account_error(status, &message) {
                continue;
            }
            return error(&state, status, message);
        }
        if payload["stream"] == true {
            if anthropic {
                let mut out = HeaderMap::new();
                out.insert("content-type", "text/event-stream".parse().unwrap());
                out.insert("cache-control", "no-store".parse().unwrap());
                return crate::target::glm::anthropic::stream_messages(
                    state, context, upstream, out,
                )
                .await;
            }
            return crate::target::glm::api::stream_chat_completions_passthrough(
                &state,
                &context,
                upstream,
                model.to_string(),
            )
            .await;
        }
        let value: Value = match upstream.json().await {
            Ok(value) => value,
            Err(_) => {
                crate::record_request_error(
                    &state,
                    &context,
                    "Compatible API returned invalid JSON",
                );
                return error(
                    &state,
                    StatusCode::BAD_GATEWAY,
                    "Compatible API returned invalid JSON",
                );
            }
        };
        let valid = if anthropic {
            value.get("content").is_some_and(Value::is_array)
        } else {
            value.get("choices").is_some_and(Value::is_array)
        };
        if !valid || value.get("error").is_some_and(|error| !error.is_null()) {
            let message = "Compatible API returned an invalid completion";
            crate::record_request_error(&state, &context, message);
            if index + 1 < accounts.len() {
                continue;
            }
            return error(&state, StatusCode::BAD_GATEWAY, message);
        }
        let mut usage = crate::usage_metrics_from_response_value(&value);
        crate::quota_usage::preserve_native_usage(
            &mut usage,
            &value,
            if anthropic { "anthropic" } else { "openai" },
        );
        crate::record_usage_success(&state, &context, &usage);
        if path == "chat/completions" && !anthropic || path == "anthropic/v1/messages" && anthropic
        {
            return Json(value).into_response();
        }
        let response = if anthropic {
            crate::target::claude::api::anthropic_message_to_response(&value, model)
        } else {
            crate::target::glm::api::chat_completion_to_responses(&value, model)
        };
        if path == "responses" {
            return if stream {
                crate::target::claude::api::responses_sse(response)
            } else {
                Json(response).into_response()
            };
        }
        if path == "anthropic/v1/messages" {
            let message =
                crate::target::glm::anthropic::responses_to_anthropic_message(&response, model);
            return if stream {
                crate::target::glm::anthropic::anthropic_stream_response(&message)
            } else {
                Json(message).into_response()
            };
        }
        let chat = response_to_chat(&response, model);
        if stream {
            let chunk = json!({"id":chat["id"],"object":"chat.completion.chunk","created":chat["created"],"model":model,
                "choices":[{"index":0,"delta":chat["choices"][0]["message"],"finish_reason":chat["choices"][0]["finish_reason"]}],"usage":chat["usage"]});
            return (
                [
                    ("content-type", "text/event-stream"),
                    ("cache-control", "no-store"),
                ],
                format!("data: {chunk}\n\ndata: [DONE]\n\n"),
            )
                .into_response();
        }
        return Json(chat).into_response();
    }
    error(
        &state,
        StatusCode::BAD_GATEWAY,
        "All compatible accounts failed",
    )
}

fn response_to_chat(response: &Value, model: &str) -> Value {
    let mut message = json!({"role":"assistant","content":response["output_text"].as_str().map(str::to_string).unwrap_or_else(|| response["output"].as_array().into_iter().flatten()
        .flat_map(|item|item["content"].as_array().into_iter().flatten()).filter_map(|part|part["text"].as_str()).collect::<Vec<_>>().join(""))});
    let tools:Vec<_>=response["output"].as_array().into_iter().flatten().filter(|item|item["type"]=="function_call")
        .map(|item|json!({"id":item["call_id"],"type":"function","function":{"name":item["name"],"arguments":item["arguments"]}})).collect();
    let finish = if tools.is_empty() {
        "stop"
    } else {
        message["tool_calls"] = json!(tools);
        "tool_calls"
    };
    json!({"id":response["id"],"object":"chat.completion","created":chrono::Utc::now().timestamp(),"model":model,
        "choices":[{"index":0,"message":message,"finish_reason":finish}],
        "usage":{"prompt_tokens":response["usage"]["input_tokens"],"completion_tokens":response["usage"]["output_tokens"],"total_tokens":response["usage"]["total_tokens"]}})
}
