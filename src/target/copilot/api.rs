use axum::{
    body::Body,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
};
use bytes::Bytes;
use futures_util::StreamExt;
use serde_json::{json, Map, Value};
use std::time::Duration;
use uuid::Uuid;

use super::accounts::{CopilotAccount, CopilotModelInfo};

const REQUEST_TIMEOUT_SECS: u64 = 180;
const MODEL_FALLBACKS: &[(&str, &str)] = &[
    ("gpt-3.5-turbo", "GPT 3.5 Turbo"),
    ("gpt-3.5-turbo-0613", "GPT 3.5 Turbo"),
    ("gpt-4-o-preview", "GPT-4o"),
    ("gpt-4.1", "GPT-4.1"),
    ("gpt-4.1-2025-04-14", "GPT-4.1"),
    ("gpt-4o", "GPT-4o"),
    ("gpt-4o-2024-05-13", "GPT-4o"),
    ("gpt-4o-2024-08-06", "GPT-4o"),
    ("gpt-4o-2024-11-20", "GPT-4o"),
    ("gpt-4o-mini", "GPT-4o mini"),
    ("gpt-4o-mini-2024-07-18", "GPT-4o mini"),
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
                    "No GitHub Copilot accounts configured",
                    "server_error",
                    None,
                ),
            )
                .into_response()
        }
    };

    let copilot_token = match super::auth::ensure_copilot_token(&state, &account).await {
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

    let models = match fetch_models(&state.client, &account.account_type, &copilot_token).await {
        Ok(models) if !models.is_empty() => models,
        _ if !account.models.is_empty() => account.models.clone(),
        Err(err) => {
            let data = fallback_models(Some(err));
            let body = serde_json::to_vec(&json!({
                "object": "list",
                "data": data,
                "models": data
            }))
            .unwrap_or_default();
            return (StatusCode::OK, [("Content-Type", "application/json")], body).into_response();
        }
        _ => MODEL_FALLBACKS
            .iter()
            .map(|(id, name)| CopilotModelInfo {
                id: (*id).to_string(),
                name: Some((*name).to_string()),
                vendor: Some("copilot".to_string()),
                preview: None,
                model_picker_category: None,
                policy_state: None,
            })
            .collect(),
    };

    let data = models_to_openai_entries(&models);
    let body = serde_json::to_vec(&json!({
        "object": "list",
        "data": data,
        "models": data
    }))
    .unwrap_or_default();
    (StatusCode::OK, [("Content-Type", "application/json")], body).into_response()
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

    let mut raw: Value = match serde_json::from_slice(&body) {
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
    let model = match raw.get("model").and_then(|value| value.as_str()) {
        Some(model) if !model.trim().is_empty() => model.trim().to_string(),
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
    if !super::accounts::is_app_accessible_model(&model) {
        crate::release_api_key_budget_before_dispatch(&state);
        return (
            StatusCode::NOT_FOUND,
            [("Content-Type", "application/json")],
            crate::source::v1::response::openai_error_body(
                &format!("The model '{}' does not exist", model),
                "invalid_request_error",
                Some("model_not_found"),
            ),
        )
            .into_response();
    }
    sanitize_responses_payload(&mut raw);
    let accounts = super::accounts::candidate_accounts(&state);
    if accounts.is_empty() {
        crate::release_api_key_budget_before_dispatch(&state);
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            [("Content-Type", "application/json")],
            crate::source::v1::response::openai_error_body(
                "No GitHub Copilot accounts configured",
                "server_error",
                None,
            ),
        )
            .into_response();
    }
    let wants_stream = crate::source::wants_stream(&headers, &body);
    if should_use_chat_completions_bridge(&raw) {
        if let Err(err) = responses_to_chat_completions_payload(&raw, &model, wants_stream) {
            crate::release_api_key_budget_before_dispatch(&state);
            return (
                StatusCode::BAD_REQUEST,
                [("Content-Type", "application/json")],
                crate::source::v1::response::openai_error_body(&err, "invalid_request_error", None),
            )
                .into_response();
        }
    }
    let request_body = match serde_json::to_vec(&raw) {
        Ok(value) => Bytes::from(value),
        Err(err) => {
            let message = format!("Copilot request serialize failed: {}", err);
            crate::release_api_key_budget_before_dispatch(&state);
            return (
                StatusCode::BAD_REQUEST,
                [("Content-Type", "application/json")],
                crate::source::v1::response::openai_error_body(
                    &message,
                    "invalid_request_error",
                    None,
                ),
            )
                .into_response();
        }
    };
    let prompt_metrics = crate::prompt_metrics_from_request_value(&raw);
    let prepared_policy_body = if should_use_chat_completions_bridge(&raw) {
        responses_to_chat_completions_payload(&raw, &model, wants_stream)
            .and_then(|payload| serde_json::to_vec(&payload).map_err(|error| error.to_string()))
            .unwrap_or_default()
    } else {
        request_body.to_vec()
    };
    let mut last_error: Option<(StatusCode, String)> = None;

    for (attempt_idx, account) in accounts.iter().enumerate() {
        let context = crate::copilot_usage_context(
            account,
            Some(model.clone()),
            "/copilot/v1/responses",
            prompt_metrics.clone(),
        );
        if let Err(response) = crate::api_key_quota_runtime::reserve_api_key_budgets_for_prepared_dispatch_with_protocol(
            &state, context.provider_name, &context.key, &prepared_policy_body,
            if should_use_chat_completions_bridge(&raw) { crate::quota_usage::PreparedProtocol::ChatCompletions } else { crate::quota_usage::PreparedProtocol::OpenAiResponses },
        ).await {
            return response;
        }
        crate::record_copilot_request(&state, &context);

        let copilot_token = match super::auth::ensure_copilot_token(&state, account).await {
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

        if should_use_chat_completions_bridge(&raw) {
            match chat_completions_bridge_response(
                &state,
                &context,
                account,
                &copilot_token,
                &raw,
                &model,
                wants_stream,
            )
            .await
            {
                Ok(response) => return response,
                Err(message) => {
                    crate::record_copilot_error(&state, &context, &message);
                    last_error = Some((StatusCode::BAD_GATEWAY, message));
                    if attempt_idx + 1 < accounts.len() {
                        continue;
                    }
                    break;
                }
            }
        }

        let resp = match post_copilot_responses(
            &state.client,
            account,
            &copilot_token,
            &request_body,
            wants_stream,
            responses_payload_has_images(&raw),
        )
        .await
        {
            Ok(resp) => resp,
            Err(message) => {
                crate::record_copilot_error(&state, &context, &message);
                last_error = Some((StatusCode::BAD_GATEWAY, message));
                if attempt_idx + 1 < accounts.len() {
                    continue;
                }
                break;
            }
        };

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            let message = format!("Copilot returned {}: {}", status, text);
            if should_try_chat_completions_fallback(status, &text, wants_stream, &raw) {
                // The failed native request may have incurred usage. Settle
                // that attempt before reserving a separate chat fallback.
                crate::record_copilot_error(&state, &context, &message);
                let fallback_body =
                    responses_to_chat_completions_payload(&raw, &model, wants_stream)
                        .and_then(|payload| {
                            serde_json::to_vec(&payload).map_err(|error| error.to_string())
                        })
                        .unwrap_or_default();
                if let Err(response) = crate::api_key_quota_runtime::reserve_api_key_budgets_for_prepared_dispatch_with_protocol(
                    &state, context.provider_name, &context.key, &fallback_body,
                    crate::quota_usage::PreparedProtocol::ChatCompletions,
                ).await {
                    return response;
                }
                crate::record_copilot_request(&state, &context);
                match chat_completions_bridge_response(
                    &state,
                    &context,
                    account,
                    &copilot_token,
                    &raw,
                    &model,
                    wants_stream,
                )
                .await
                {
                    Ok(response) => return response,
                    Err(fallback_message) => {
                        let combined = format!("{} | chat fallback: {}", message, fallback_message);
                        crate::record_copilot_error(&state, &context, &combined);
                        if attempt_idx + 1 < accounts.len()
                            && crate::should_retry_account_error(status, &combined)
                        {
                            last_error = Some((status, combined));
                            continue;
                        }
                        return (
                            status,
                            [("Content-Type", "application/json")],
                            crate::source::v1::response::openai_error_body(
                                &combined,
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
            crate::record_copilot_error(&state, &context, &message);
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

        if wants_stream {
            return stream_native_responses(&state, &context, resp).await;
        }

        let text = match resp.text().await {
            Ok(text) => text,
            Err(err) => {
                let message = format!("Copilot body read failed: {}", err);
                crate::record_copilot_error(&state, &context, &message);
                last_error = Some((StatusCode::BAD_GATEWAY, message));
                if attempt_idx + 1 < accounts.len() {
                    continue;
                }
                break;
            }
        };
        let value: Value = match serde_json::from_str(&text) {
            Ok(value) => value,
            Err(err) => {
                let message = format!("invalid Copilot response: {}", err);
                crate::record_copilot_error(&state, &context, &message);
                return (
                    StatusCode::BAD_GATEWAY,
                    [("Content-Type", "application/json")],
                    crate::source::v1::response::openai_error_body(&message, "server_error", None),
                )
                    .into_response();
            }
        };
        let usage = crate::usage_metrics_from_response_value(&value);
        crate::record_copilot_success(&state, &context, &usage);
        return (
            StatusCode::OK,
            [("Content-Type", "application/json")],
            Bytes::from(text),
        )
            .into_response();
    }

    let (status, message) = last_error.unwrap_or_else(|| {
        (
            StatusCode::BAD_GATEWAY,
            "All GitHub Copilot accounts failed".to_string(),
        )
    });
    (
        status,
        [("Content-Type", "application/json")],
        crate::source::v1::response::openai_error_body(
            &format!(
                "All GitHub Copilot accounts failed; last error: {}",
                message
            ),
            "server_error",
            None,
        ),
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
        return (
            StatusCode::UNAUTHORIZED,
            [("Content-Type", "application/json")],
            anthropic_error_body("authentication_error", "Invalid proxy API key"),
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
                anthropic_error_body("invalid_request_error", "Invalid request body"),
            )
                .into_response();
        }
    };
    let model = match raw.get("model").and_then(|value| value.as_str()) {
        Some(model) if !model.trim().is_empty() => model.trim().to_string(),
        _ => {
            crate::release_api_key_budget_before_dispatch(&state);
            return (
                StatusCode::BAD_REQUEST,
                [("Content-Type", "application/json")],
                anthropic_error_body("invalid_request_error", "model is required"),
            )
                .into_response();
        }
    };
    if !super::accounts::is_app_accessible_model(&model) {
        crate::release_api_key_budget_before_dispatch(&state);
        return (
            StatusCode::NOT_FOUND,
            [("Content-Type", "application/json")],
            anthropic_error_body(
                "not_found_error",
                &format!("The model '{}' does not exist", model),
            ),
        )
            .into_response();
    }
    let responses_payload = match anthropic_messages_to_responses(&raw, &model) {
        Ok(payload) => payload,
        Err(err) => {
            crate::release_api_key_budget_before_dispatch(&state);
            return (
                StatusCode::BAD_REQUEST,
                [("Content-Type", "application/json")],
                anthropic_error_body("invalid_request_error", &err),
            )
                .into_response();
        }
    };

    if should_use_chat_completions_bridge(&responses_payload) {
        if let Err(err) = responses_to_chat_completions_payload(&responses_payload, &model, false) {
            crate::release_api_key_budget_before_dispatch(&state);
            return (
                StatusCode::BAD_REQUEST,
                [("Content-Type", "application/json")],
                anthropic_error_body("invalid_request_error", &err),
            )
                .into_response();
        }
    }

    let accounts = super::accounts::candidate_accounts(&state);
    if accounts.is_empty() {
        crate::release_api_key_budget_before_dispatch(&state);
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            [("Content-Type", "application/json")],
            anthropic_error_body("api_error", "No GitHub Copilot accounts configured"),
        )
            .into_response();
    }
    let prompt_metrics = crate::prompt_metrics_from_request_value(&raw);
    let prepared_policy_body = if should_use_chat_completions_bridge(&responses_payload) {
        responses_to_chat_completions_payload(&responses_payload, &model, false)
            .and_then(|payload| serde_json::to_vec(&payload).map_err(|error| error.to_string()))
            .unwrap_or_default()
    } else {
        serde_json::to_vec(&responses_payload).unwrap_or_default()
    };
    let mut last_error: Option<(StatusCode, String)> = None;

    for (attempt_idx, account) in accounts.iter().enumerate() {
        let context = crate::copilot_usage_context(
            account,
            Some(model.clone()),
            "/copilot/anthropic/v1/messages",
            prompt_metrics.clone(),
        );
        if let Err(response) = crate::api_key_quota_runtime::reserve_api_key_budgets_for_prepared_dispatch_with_protocol(
            &state, context.provider_name, &context.key, &prepared_policy_body,
            if should_use_chat_completions_bridge(&responses_payload) { crate::quota_usage::PreparedProtocol::ChatCompletions } else { crate::quota_usage::PreparedProtocol::OpenAiResponses },
        ).await {
            return response;
        }
        crate::record_copilot_request(&state, &context);

        let copilot_token = match super::auth::ensure_copilot_token(&state, account).await {
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

        if should_use_chat_completions_bridge(&responses_payload) {
            let response_value = match chat_completions_response_value(
                &state.client,
                account,
                &copilot_token,
                &responses_payload,
                &model,
                false,
            )
            .await
            {
                Ok(value) => value,
                Err(message) => {
                    crate::record_copilot_error(&state, &context, &message);
                    last_error = Some((StatusCode::BAD_GATEWAY, message));
                    if attempt_idx + 1 < accounts.len() {
                        continue;
                    }
                    break;
                }
            };
            let usage = crate::usage_metrics_from_response_value(&response_value);
            crate::record_copilot_success(&state, &context, &usage);
            let anthropic = responses_to_anthropic_message(&response_value, &model);
            if raw
                .get("stream")
                .and_then(|value| value.as_bool())
                .unwrap_or(false)
            {
                return anthropic_stream_response(&anthropic);
            }

            return (
                StatusCode::OK,
                [("Content-Type", "application/json")],
                serde_json::to_vec(&anthropic).unwrap_or_default(),
            )
                .into_response();
        }

        let body = Bytes::from(serde_json::to_vec(&responses_payload).unwrap_or_default());
        let resp = match post_copilot_responses(
            &state.client,
            account,
            &copilot_token,
            &body,
            false,
            responses_payload_has_images(&responses_payload),
        )
        .await
        {
            Ok(resp) => resp,
            Err(message) => {
                crate::record_copilot_error(&state, &context, &message);
                last_error = Some((StatusCode::BAD_GATEWAY, message));
                if attempt_idx + 1 < accounts.len() {
                    continue;
                }
                break;
            }
        };

        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            let message = format!("Copilot returned {}: {}", status, text);
            crate::record_copilot_error(&state, &context, &message);
            if attempt_idx + 1 < accounts.len()
                && crate::should_retry_account_error(status, &message)
            {
                last_error = Some((status, message));
                continue;
            }
            return (
                status,
                [("Content-Type", "application/json")],
                anthropic_error_body(
                    if status.is_client_error() {
                        "invalid_request_error"
                    } else {
                        "api_error"
                    },
                    &message,
                ),
            )
                .into_response();
        }
        let response_value: Value = match serde_json::from_str(&text) {
            Ok(value) => value,
            Err(err) => {
                let message = format!("invalid Copilot response: {}", err);
                crate::record_copilot_error(&state, &context, &message);
                return (
                    StatusCode::BAD_GATEWAY,
                    [("Content-Type", "application/json")],
                    anthropic_error_body("api_error", &message),
                )
                    .into_response();
            }
        };
        let usage = crate::usage_metrics_from_response_value(&response_value);
        crate::record_copilot_success(&state, &context, &usage);
        let anthropic = responses_to_anthropic_message(&response_value, &model);
        if raw
            .get("stream")
            .and_then(|value| value.as_bool())
            .unwrap_or(false)
        {
            return anthropic_stream_response(&anthropic);
        }

        return (
            StatusCode::OK,
            [("Content-Type", "application/json")],
            serde_json::to_vec(&anthropic).unwrap_or_default(),
        )
            .into_response();
    }

    let (status, message) = last_error.unwrap_or_else(|| {
        (
            StatusCode::BAD_GATEWAY,
            "All GitHub Copilot accounts failed".to_string(),
        )
    });
    (
        status,
        [("Content-Type", "application/json")],
        anthropic_error_body(
            "api_error",
            &format!(
                "All GitHub Copilot accounts failed; last error: {}",
                message
            ),
        ),
    )
        .into_response()
}

pub async fn fetch_models(
    client: &reqwest::Client,
    account_type: &str,
    copilot_token: &str,
) -> Result<Vec<CopilotModelInfo>, String> {
    let url = format!("{}/models", super::auth::copilot_base_url(account_type));
    let resp = client
        .get(url)
        .headers(super::auth::copilot_headers(copilot_token, false, "user"))
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .map_err(|err| format!("Copilot models request failed: {}", err))?;
    let status = resp.status();
    let text = resp
        .text()
        .await
        .map_err(|err| format!("Copilot models body read failed: {}", err))?;
    if !status.is_success() {
        return Err(format!("Copilot models returned {}: {}", status, text));
    }
    let value: Value = serde_json::from_str(&text)
        .map_err(|err| format!("Copilot models JSON parse failed: {}", err))?;
    Ok(value
        .get("data")
        .and_then(|value| value.as_array())
        .into_iter()
        .flatten()
        .filter_map(|item| {
            let id = item.get("id").and_then(|value| value.as_str())?.trim();
            if id.is_empty() {
                return None;
            }
            if !super::accounts::is_app_accessible_model(id) {
                return None;
            }
            Some(CopilotModelInfo {
                id: id.to_string(),
                name: item
                    .get("name")
                    .and_then(|value| value.as_str())
                    .map(|value| value.to_string()),
                vendor: item
                    .get("vendor")
                    .and_then(|value| value.as_str())
                    .map(|value| value.to_string()),
                preview: item.get("preview").and_then(|value| value.as_bool()),
                model_picker_category: item
                    .get("model_picker_category")
                    .and_then(|value| value.as_str())
                    .map(|value| value.to_string()),
                policy_state: item
                    .get("policy")
                    .and_then(|value| value.get("state"))
                    .and_then(|value| value.as_str())
                    .map(|value| value.to_string()),
            })
        })
        .collect())
}

fn models_to_openai_entries(models: &[CopilotModelInfo]) -> Vec<Value> {
    models
        .iter()
        .filter(|model| super::accounts::is_app_accessible_model(&model.id))
        .map(|model| {
            json!({
                "id": prefixed_model_id(&model.id),
                "object": "model",
                "created": 0,
                "owned_by": "copilot",
                "display_name": model.name.as_deref().unwrap_or(&model.id),
                "upstream_model": model.id,
                "vendor": model.vendor,
                "preview": model.preview,
                "model_picker_category": model.model_picker_category,
                "policy_state": model.policy_state,
                "billing_tier": super::accounts::model_billing_tier(&model.id, model.model_picker_category.as_deref()),
                "premium": super::accounts::model_is_premium(&model.id, model.model_picker_category.as_deref()),
                "utility_model": super::accounts::is_utility_model(&model.id),
                "app_accessible": super::accounts::is_app_accessible_model(&model.id)
            })
        })
        .collect()
}

fn fallback_models(warning: Option<String>) -> Vec<Value> {
    MODEL_FALLBACKS
        .iter()
        .map(|(id, name)| {
            let mut value = json!({
                "id": prefixed_model_id(id),
                "object": "model",
                "created": 0,
                "owned_by": "copilot",
                "display_name": name,
                "upstream_model": id
            });
            if let Some(warning) = warning.as_ref() {
                value["warning"] = Value::String(warning.clone());
            }
            value
        })
        .collect()
}

fn prefixed_model_id(model: &str) -> String {
    if model.starts_with("cop:") {
        model.to_string()
    } else {
        format!("cop:{}", model)
    }
}

async fn post_copilot_responses(
    client: &reqwest::Client,
    account: &CopilotAccount,
    copilot_token: &str,
    body: &Bytes,
    stream: bool,
    vision: bool,
) -> Result<reqwest::Response, String> {
    let url = format!(
        "{}/responses",
        super::auth::copilot_base_url(&account.account_type)
    );
    let mut request = client
        .post(url)
        .headers(super::auth::copilot_headers(copilot_token, vision, "agent"))
        .header(
            "Accept",
            if stream {
                "text/event-stream"
            } else {
                "application/json"
            },
        )
        .timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS))
        .body(body.clone());

    request = request.header("Accept-Encoding", "identity");
    request
        .send()
        .await
        .map_err(|err| format!("Copilot responses request failed: {}", err))
}

fn should_use_chat_completions_bridge(raw: &Value) -> bool {
    !responses_payload_has_images(raw)
}

async fn chat_completions_bridge_response(
    state: &crate::AppState,
    context: &crate::UsageContext,
    account: &CopilotAccount,
    copilot_token: &str,
    raw: &Value,
    model: &str,
    wants_stream: bool,
) -> Result<axum::response::Response, String> {
    if wants_stream {
        let chat_payload = responses_to_chat_completions_payload(raw, model, true)?;
        let chat_body = Bytes::from(
            serde_json::to_vec(&chat_payload)
                .map_err(|err| format!("Copilot chat payload serialize failed: {}", err))?,
        );
        let initiator = chat_payload_initiator(&chat_payload);
        let resp = post_copilot_chat_completions(
            &state.client,
            account,
            copilot_token,
            &chat_body,
            responses_payload_has_images(raw),
            true,
            initiator,
        )
        .await?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp
                .text()
                .await
                .map_err(|err| format!("Copilot chat body read failed: {}", err))?;
            return Err(format!("Copilot chat returned {}: {}", status, text));
        }
        return Ok(
            stream_chat_completions_as_responses(state, context, resp, model.to_string()).await,
        );
    }

    let value =
        chat_completions_response_value(&state.client, account, copilot_token, raw, model, false)
            .await?;
    let usage = crate::usage_metrics_from_response_value(&value);
    crate::record_copilot_success(state, context, &usage);
    Ok((
        StatusCode::OK,
        [("Content-Type", "application/json")],
        serde_json::to_vec(&value).unwrap_or_default(),
    )
        .into_response())
}

async fn chat_completions_response_value(
    client: &reqwest::Client,
    account: &CopilotAccount,
    copilot_token: &str,
    raw: &Value,
    model: &str,
    stream: bool,
) -> Result<Value, String> {
    let chat_payload = responses_to_chat_completions_payload(raw, model, stream)?;
    let chat_body = Bytes::from(
        serde_json::to_vec(&chat_payload)
            .map_err(|err| format!("Copilot chat payload serialize failed: {}", err))?,
    );
    let initiator = chat_payload_initiator(&chat_payload);
    let resp = post_copilot_chat_completions(
        client,
        account,
        copilot_token,
        &chat_body,
        responses_payload_has_images(raw),
        stream,
        initiator,
    )
    .await?;
    let status = resp.status();
    let text = resp
        .text()
        .await
        .map_err(|err| format!("Copilot chat body read failed: {}", err))?;
    if !status.is_success() {
        return Err(format!("Copilot chat returned {}: {}", status, text));
    }
    let chat: Value = serde_json::from_str(&text)
        .map_err(|err| format!("invalid Copilot chat response: {}", err))?;
    Ok(chat_completion_to_response(&chat, model))
}

async fn post_copilot_chat_completions(
    client: &reqwest::Client,
    account: &CopilotAccount,
    copilot_token: &str,
    body: &Bytes,
    vision: bool,
    stream: bool,
    initiator: &str,
) -> Result<reqwest::Response, String> {
    let url = format!(
        "{}/chat/completions",
        super::auth::copilot_base_url(&account.account_type)
    );
    client
        .post(url)
        .headers(super::auth::copilot_headers(
            copilot_token,
            vision,
            initiator,
        ))
        .header(
            "Accept",
            if stream {
                "text/event-stream"
            } else {
                "application/json"
            },
        )
        .header("Accept-Encoding", "identity")
        .timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS))
        .body(body.clone())
        .send()
        .await
        .map_err(|err| format!("Copilot chat request failed: {}", err))
}

fn should_try_chat_completions_fallback(
    status: StatusCode,
    body: &str,
    wants_stream: bool,
    raw: &Value,
) -> bool {
    if !status.is_client_error() || responses_payload_has_images(raw) {
        return false;
    }
    let _ = wants_stream;
    body.contains("unsupported_api_for_model") || body.contains("model_not_supported")
}

fn responses_to_chat_completions_payload(
    raw: &Value,
    model: &str,
    stream: bool,
) -> Result<Value, String> {
    let mut messages = Vec::new();
    if let Some(instructions) = raw.get("instructions").and_then(|value| value.as_str()) {
        if !instructions.trim().is_empty() {
            messages.push(json!({"role": "system", "content": instructions}));
        }
    }
    if let Some(input_messages) = raw.get("messages").and_then(|value| value.as_array()) {
        for message in input_messages {
            append_chat_message(&mut messages, message);
        }
    } else if let Some(input) = raw.get("input") {
        append_responses_input_as_chat(&mut messages, input);
    }
    if messages.is_empty() {
        return Err("input is required for chat fallback".to_string());
    }

    let mut out = Map::new();
    out.insert("model".to_string(), Value::String(model.to_string()));
    out.insert(
        "messages".to_string(),
        Value::Array(sanitize_chat_messages(messages)),
    );
    if let Some(value) = raw.get("max_completion_tokens") {
        out.insert("max_completion_tokens".to_string(), value.clone());
    } else if let Some(value) = raw
        .get("max_output_tokens")
        .or_else(|| raw.get("max_tokens"))
    {
        // Reasoning output is part of the total ceiling. The chat API's
        // legacy max_tokens field is not that ceiling for these models.
        let native_cap = if crate::quota_usage::known_reasoning_copilot_model(model) {
            "max_completion_tokens"
        } else {
            "max_tokens"
        };
        out.insert(native_cap.to_string(), value.clone());
    }
    copy_if_present(raw, &mut out, "temperature");
    copy_if_present(raw, &mut out, "top_p");
    copy_if_present(raw, &mut out, "stop");
    copy_if_present(raw, &mut out, "parallel_tool_calls");
    if let Some(tools) = chat_tools_from_responses_tools(raw.get("tools")) {
        out.insert("tools".to_string(), Value::Array(tools));
    }
    if let Some(tool_choice) = chat_tool_choice_from_responses(raw.get("tool_choice")) {
        out.insert("tool_choice".to_string(), tool_choice);
    }
    if raw.get("reasoning").is_some() {
        out.insert("thinking".to_string(), json!({"type": "enabled"}));
    }
    out.insert("stream".to_string(), Value::Bool(stream));
    Ok(Value::Object(out))
}

fn append_responses_input_as_chat(messages: &mut Vec<Value>, input: &Value) {
    match input {
        Value::String(text) => {
            if !text.trim().is_empty() {
                messages.push(json!({"role": "user", "content": text}));
            }
        }
        Value::Array(items) => {
            for item in items {
                append_response_input_item_as_chat(messages, item);
            }
        }
        Value::Object(_) => append_response_input_item_as_chat(messages, input),
        _ => {}
    }
}

fn append_response_input_item_as_chat(messages: &mut Vec<Value>, item: &Value) {
    match item
        .get("type")
        .and_then(|value| value.as_str())
        .unwrap_or("")
    {
        "function_call" => {
            let call_id = item
                .get("call_id")
                .or_else(|| item.get("id"))
                .and_then(|value| value.as_str())
                .unwrap_or("");
            let name = item
                .get("name")
                .and_then(|value| value.as_str())
                .unwrap_or("");
            if call_id.is_empty() || name.is_empty() {
                return;
            }
            let arguments = item
                .get("arguments")
                .and_then(|value| value.as_str())
                .map(str::to_string)
                .unwrap_or_else(|| {
                    item.get("arguments")
                        .map(|value| value.to_string())
                        .unwrap_or_else(|| "{}".to_string())
                });
            messages.push(json!({
                "role": "assistant",
                "content": Value::Null,
                "tool_calls": [{
                    "id": call_id,
                    "type": "function",
                    "function": {
                        "name": name,
                        "arguments": arguments
                    }
                }]
            }));
        }
        "function_call_output" | "tool_result" => {
            let call_id = item
                .get("call_id")
                .or_else(|| item.get("tool_call_id"))
                .and_then(|value| value.as_str())
                .unwrap_or("");
            if call_id.is_empty() {
                return;
            }
            let output = item
                .get("output")
                .or_else(|| item.get("content"))
                .map(chat_tool_output_text)
                .unwrap_or_default();
            messages.push(json!({
                "role": "tool",
                "tool_call_id": call_id,
                "content": output
            }));
        }
        "reasoning" | "compaction" => {}
        _ => append_chat_message(messages, item),
    }
}

fn append_chat_message(messages: &mut Vec<Value>, message: &Value) {
    match message
        .get("type")
        .and_then(|value| value.as_str())
        .unwrap_or("")
    {
        "function_call" | "function_call_output" | "tool_result" | "reasoning" | "compaction" => {
            append_response_input_item_as_chat(messages, message);
            return;
        }
        _ => {}
    }

    let role = message
        .get("role")
        .and_then(|value| value.as_str())
        .unwrap_or("user");
    let content = message
        .get("content")
        .or_else(|| message.get("text"))
        .or_else(|| message.get("input_text"))
        .or_else(|| message.get("output_text"));
    let content = content
        .map(chat_content_from_value)
        .unwrap_or(Value::String(String::new()));
    let tool_calls = message.get("tool_calls").and_then(|value| value.as_array());
    if content_is_empty(&content) && tool_calls.map(|calls| calls.is_empty()).unwrap_or(true) {
        return;
    }

    let role = match role {
        "assistant" => "assistant",
        "system" | "developer" => "system",
        "tool" => "tool",
        _ => "user",
    };
    let mut entry = json!({
        "role": role,
        "content": content
    });
    if let Some(name) = message.get("name").and_then(|value| value.as_str()) {
        entry["name"] = json!(name);
    }
    if let Some(tool_call_id) = message.get("tool_call_id").and_then(|value| value.as_str()) {
        entry["tool_call_id"] = json!(tool_call_id);
    }
    if let Some(tool_calls) = tool_calls {
        entry["tool_calls"] = Value::Array(tool_calls.clone());
    }
    messages.push(entry);
}

fn chat_content_from_value(value: &Value) -> Value {
    match value {
        Value::String(_) => value.clone(),
        Value::Array(parts) => {
            let mut out = Vec::new();
            let mut has_image = false;
            for part in parts {
                if let Some(text) = part
                    .get("text")
                    .or_else(|| part.get("input_text"))
                    .or_else(|| part.get("output_text"))
                    .and_then(|value| value.as_str())
                {
                    if !text.trim().is_empty() {
                        out.push(json!({"type": "text", "text": text}));
                    }
                    continue;
                }
                if let Some(image_url) = chat_image_url_from_part(part) {
                    has_image = true;
                    out.push(json!({
                        "type": "image_url",
                        "image_url": { "url": image_url }
                    }));
                }
            }
            if has_image {
                Value::Array(out)
            } else {
                Value::String(
                    out.iter()
                        .filter_map(|part| part.get("text").and_then(|value| value.as_str()))
                        .collect::<Vec<_>>()
                        .join("\n"),
                )
            }
        }
        _ => Value::String(value.to_string()),
    }
}

fn chat_image_url_from_part(part: &Value) -> Option<String> {
    if part.get("type").and_then(|value| value.as_str()) != Some("input_image") {
        return None;
    }
    if let Some(url) = part.get("image_url").and_then(|value| value.as_str()) {
        return Some(url.to_string());
    }
    part.get("image_url")
        .and_then(|value| value.get("url"))
        .and_then(|value| value.as_str())
        .map(str::to_string)
}

fn content_is_empty(value: &Value) -> bool {
    match value {
        Value::String(text) => text.trim().is_empty(),
        Value::Array(items) => items.is_empty(),
        Value::Null => true,
        _ => false,
    }
}

fn chat_tool_output_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|part| {
                part.get("text")
                    .or_else(|| part.get("input_text"))
                    .or_else(|| part.get("output_text"))
                    .and_then(|value| value.as_str())
                    .map(str::to_string)
                    .or_else(|| Some(part.to_string()))
            })
            .collect::<Vec<_>>()
            .join("\n\n"),
        value => value.to_string(),
    }
}

fn chat_tools_from_responses_tools(value: Option<&Value>) -> Option<Vec<Value>> {
    let tools = value?.as_array()?;
    let out = tools
        .iter()
        .filter_map(|tool| {
            if tool.get("type").and_then(|value| value.as_str()) != Some("function") {
                return None;
            }
            let function = tool.get("function");
            let name = function
                .and_then(|value| value.get("name"))
                .or_else(|| tool.get("name"))
                .and_then(|value| value.as_str())?;
            Some(json!({
                "type": "function",
                "function": {
                    "name": name,
                    "description": function
                        .and_then(|value| value.get("description"))
                        .or_else(|| tool.get("description"))
                        .and_then(|value| value.as_str())
                        .unwrap_or(""),
                    "parameters": function
                        .and_then(|value| value.get("parameters"))
                        .or_else(|| tool.get("parameters"))
                        .cloned()
                        .unwrap_or_else(|| json!({ "type": "object" }))
                }
            }))
        })
        .collect::<Vec<_>>();
    (!out.is_empty()).then_some(out)
}

fn chat_tool_choice_from_responses(value: Option<&Value>) -> Option<Value> {
    let value = value?;
    if let Some(choice) = value.as_str() {
        return match choice {
            "auto" | "none" | "required" => Some(Value::String(choice.to_string())),
            _ => None,
        };
    }
    if value.get("type").and_then(|value| value.as_str()) != Some("function") {
        return None;
    }
    let name = value
        .get("function")
        .and_then(|function| function.get("name"))
        .or_else(|| value.get("name"))
        .and_then(|name| name.as_str())?;
    Some(json!({
        "type": "function",
        "function": { "name": name }
    }))
}

fn sanitize_chat_messages(messages: Vec<Value>) -> Vec<Value> {
    let mut out = Vec::new();
    let mut index = 0;

    while index < messages.len() {
        let message = &messages[index];
        if chat_message_role(message) == Some("tool") {
            index += 1;
            continue;
        }

        let has_tool_calls = message
            .get("tool_calls")
            .and_then(|value| value.as_array())
            .map(|calls| !calls.is_empty())
            .unwrap_or(false);
        if chat_message_role(message) == Some("assistant") && has_tool_calls {
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

            out.push(message.clone());
            out.extend(tool_messages);
            index = next;
            continue;
        }

        out.push(message.clone());
        index += 1;
    }

    out
}

fn chat_message_role(message: &Value) -> Option<&str> {
    message.get("role").and_then(|value| value.as_str())
}

fn chat_message_tool_call_ids(message: &Value) -> Vec<String> {
    message
        .get("tool_calls")
        .and_then(|value| value.as_array())
        .into_iter()
        .flatten()
        .filter_map(|call| call.get("id").and_then(|value| value.as_str()))
        .filter(|id| !id.is_empty())
        .map(ToString::to_string)
        .collect()
}

fn chat_message_tool_call_id(message: &Value) -> Option<&str> {
    message
        .get("tool_call_id")
        .and_then(|value| value.as_str())
        .filter(|id| !id.is_empty())
}

fn chat_payload_initiator(payload: &Value) -> &str {
    let has_agent_turn = payload
        .get("messages")
        .and_then(|value| value.as_array())
        .map(|messages| {
            messages.iter().any(|message| {
                matches!(
                    message.get("role").and_then(|value| value.as_str()),
                    Some("assistant" | "tool")
                )
            })
        })
        .unwrap_or(false);
    if has_agent_turn {
        "agent"
    } else {
        "user"
    }
}

fn chat_completion_to_response(chat: &Value, model: &str) -> Value {
    let output_text = chat_output_text(chat);
    let usage = chat_usage_to_responses_usage(chat.get("usage"));
    let mut output = Vec::new();
    if let Some(choices) = chat.get("choices").and_then(|value| value.as_array()) {
        for choice in choices {
            let Some(message) = choice.get("message") else {
                continue;
            };
            if let Some(text) = chat_message_content_text(message.get("content")) {
                if !text.is_empty() {
                    output.push(json!({
                        "id": format!("msg_{}", Uuid::new_v4().simple()),
                        "type": "message",
                        "status": "completed",
                        "role": "assistant",
                        "content": [{
                            "type": "output_text",
                            "text": text
                        }]
                    }));
                }
            }
            if let Some(tool_calls) = message.get("tool_calls").and_then(|value| value.as_array()) {
                for call in tool_calls {
                    let call_id = call
                        .get("id")
                        .and_then(|value| value.as_str())
                        .filter(|value| !value.is_empty())
                        .map(str::to_string)
                        .unwrap_or_else(|| format!("call_{}", Uuid::new_v4().simple()));
                    let name = call
                        .get("function")
                        .and_then(|value| value.get("name"))
                        .and_then(|value| value.as_str())
                        .unwrap_or("tool");
                    let arguments = call
                        .get("function")
                        .and_then(|value| value.get("arguments"))
                        .and_then(|value| value.as_str())
                        .unwrap_or("{}");
                    output.push(json!({
                        "id": call_id,
                        "type": "function_call",
                        "status": "completed",
                        "call_id": call_id,
                        "name": name,
                        "arguments": arguments
                    }));
                }
            }
        }
    }
    if output.is_empty() && !output_text.is_empty() {
        output.push(json!({
            "id": format!("msg_{}", Uuid::new_v4().simple()),
            "type": "message",
            "status": "completed",
            "role": "assistant",
            "content": [{
                "type": "output_text",
                "text": output_text
            }]
        }));
    }

    json!({
        "id": chat.get("id").and_then(|value| value.as_str()).unwrap_or("resp_copilot_chat"),
        "object": "response",
        "created_at": chat.get("created").and_then(|value| value.as_i64()).unwrap_or_else(|| chrono::Utc::now().timestamp()),
        "status": "completed",
        "model": model,
        "output": output,
        "output_text": output_text,
        "usage": usage
    })
}

fn chat_output_text(chat: &Value) -> String {
    let Some(choices) = chat.get("choices").and_then(|value| value.as_array()) else {
        return String::new();
    };
    choices
        .iter()
        .filter_map(|choice| choice.get("message"))
        .filter_map(|message| message.get("content"))
        .filter_map(|content| {
            if let Some(text) = content.as_str() {
                Some(text.to_string())
            } else if let Some(parts) = content.as_array() {
                Some(
                    parts
                        .iter()
                        .filter_map(|part| part.get("text").and_then(|value| value.as_str()))
                        .collect::<Vec<_>>()
                        .join("\n\n"),
                )
            } else {
                None
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn chat_message_content_text(content: Option<&Value>) -> Option<String> {
    let content = content?;
    if let Some(text) = content.as_str() {
        return Some(text.to_string());
    }
    if let Some(parts) = content.as_array() {
        let text = parts
            .iter()
            .filter_map(|part| part.get("text").and_then(|value| value.as_str()))
            .collect::<Vec<_>>()
            .join("\n\n");
        return Some(text);
    }
    None
}

fn chat_usage_to_responses_usage(usage: Option<&Value>) -> Value {
    crate::quota_usage::openai_compat_usage(usage.unwrap_or(&Value::Null))
}

async fn stream_chat_completions_as_responses(
    state: &crate::AppState,
    context: &crate::UsageContext,
    resp: reqwest::Response,
    model: String,
) -> axum::response::Response {
    let usage_state = state.clone();
    let usage_context = context.clone();
    let lifecycle = crate::StreamRequestGuard::new(&usage_state, &usage_context);
    let stream = async_stream::stream! {
        let mut lifecycle = lifecycle;
        let mut upstream = resp.bytes_stream();
        let mut parser = ChatCompletionsSseAccumulator::default();
        while let Some(chunk) = upstream.next().await {
            match chunk {
                Ok(bytes) => {
                    parser.push(&bytes);
                    if let Some(message) = parser.terminal_error() {
                        crate::record_copilot_error(&usage_state, &usage_context, message);
                        lifecycle.finish();
                        yield Err::<Bytes, std::io::Error>(std::io::Error::new(std::io::ErrorKind::Other, "upstream stream error"));
                        return;
                    }
                }
                Err(err) => {
                    let message = format!("Copilot chat stream read failed: {}", err);
                    crate::record_copilot_error(&usage_state, &usage_context, &message);
                    lifecycle.finish();
                    yield Err::<Bytes, std::io::Error>(std::io::Error::new(std::io::ErrorKind::Other, "stream"));
                    return;
                }
            }
        }

        let response = match parser.finish(&model) {
            Ok(response) => response,
            Err(message) => {
                crate::record_copilot_error(&usage_state, &usage_context, &message);
                lifecycle.finish();
                yield Err::<Bytes, std::io::Error>(std::io::Error::new(std::io::ErrorKind::Other, "upstream stream error"));
                return;
            }
        };
        let usage = crate::usage_metrics_from_response_value(&response);
        crate::record_copilot_success(&usage_state, &usage_context, &usage);
        for event in response_stream_events(&response) {
            yield Ok::<Bytes, std::io::Error>(event);
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
struct ChatCompletionsSseAccumulator {
    buffer: Vec<u8>,
    id: Option<String>,
    created: Option<i64>,
    model: Option<String>,
    content: String,
    tool_calls: Vec<ChatStreamToolCall>,
    usage: Option<Value>,
    finish_reason: Option<String>,
    terminal_error: Option<String>,
    usage_at_terminal: bool,
}

#[derive(Default, Clone)]
struct ChatStreamToolCall {
    id: Option<String>,
    name: Option<String>,
    arguments: String,
}

impl ChatCompletionsSseAccumulator {
    fn push(&mut self, bytes: &Bytes) {
        self.buffer.extend_from_slice(bytes);
        while let Some((event_end, delimiter_len)) = find_sse_boundary(&self.buffer) {
            let raw_event: Vec<u8> = self.buffer.drain(..event_end + delimiter_len).collect();
            self.absorb_event(&raw_event[..event_end]);
        }
    }

    fn finish(mut self, client_model: &str) -> Result<Value, String> {
        if !self.buffer.is_empty() {
            let raw = std::mem::take(&mut self.buffer);
            self.absorb_event(&raw);
        }
        if let Some(error) = self.terminal_error {
            return Err(error);
        }
        if let Some(Value::Object(usage)) = self.usage.as_mut() {
            usage.insert(
                "_quota_final_usage".to_string(),
                Value::Bool(self.usage_at_terminal),
            );
        }

        let mut message = json!({
            "role": "assistant",
            "content": if self.content.is_empty() { Value::Null } else { Value::String(self.content) }
        });
        let tool_calls = self
            .tool_calls
            .iter()
            .filter(|call| call.id.is_some() || call.name.is_some() || !call.arguments.is_empty())
            .map(|call| {
                json!({
                    "id": call
                        .id
                        .clone()
                        .unwrap_or_else(|| format!("call_{}", Uuid::new_v4().simple())),
                    "type": "function",
                    "function": {
                        "name": call.name.clone().unwrap_or_else(|| "tool".to_string()),
                        "arguments": call.arguments
                    }
                })
            })
            .collect::<Vec<_>>();
        if !tool_calls.is_empty() {
            message["tool_calls"] = Value::Array(tool_calls);
        }

        let chat = json!({
            "id": self
                .id
                .unwrap_or_else(|| format!("chatcmpl-{}", Uuid::new_v4().simple())),
            "object": "chat.completion",
            "created": self.created.unwrap_or_else(|| chrono::Utc::now().timestamp()),
            "model": self.model.unwrap_or_else(|| client_model.to_string()),
            "choices": [{
                "index": 0,
                "message": message,
                "finish_reason": self.finish_reason.unwrap_or_else(|| "stop".to_string())
            }],
            "usage": self.usage.unwrap_or_else(|| json!({}))
        });
        Ok(chat_completion_to_response(&chat, client_model))
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

        let terminal = self.finish_reason.is_some()
            || value
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

        if let Some(id) = value
            .get("id")
            .and_then(|value| value.as_str())
            .filter(|value| !value.is_empty())
        {
            self.id = Some(id.to_string());
        }
        if let Some(created) = value.get("created").and_then(|value| value.as_i64()) {
            if created > 0 {
                self.created = Some(created);
            }
        }
        if let Some(model) = value
            .get("model")
            .and_then(|value| value.as_str())
            .filter(|value| !value.is_empty())
        {
            self.model = Some(model.to_string());
        }
        if let Some(usage) = value.get("usage") {
            if !usage.is_null() {
                crate::quota_usage::merge_cumulative_usage(&mut self.usage, usage);
                self.usage_at_terminal =
                    terminal && crate::quota_usage::has_output_observation(usage);
            }
        }

        let Some(choices) = value.get("choices").and_then(|value| value.as_array()) else {
            return;
        };
        for choice in choices {
            if let Some(finish_reason) = choice
                .get("finish_reason")
                .and_then(|value| value.as_str())
                .filter(|value| !value.is_empty())
            {
                self.finish_reason = Some(finish_reason.to_string());
            }
            let Some(delta) = choice.get("delta") else {
                continue;
            };
            if let Some(content) = delta.get("content").and_then(|value| value.as_str()) {
                self.content.push_str(content);
            }
            if let Some(tool_calls) = delta.get("tool_calls").and_then(|value| value.as_array()) {
                for call in tool_calls {
                    self.absorb_tool_call_delta(call);
                }
            }
        }
    }

    fn absorb_tool_call_delta(&mut self, value: &Value) {
        let index = value
            .get("index")
            .and_then(|value| value.as_u64())
            .map(|value| value as usize)
            .unwrap_or(self.tool_calls.len());
        while self.tool_calls.len() <= index {
            self.tool_calls.push(ChatStreamToolCall::default());
        }
        let call = &mut self.tool_calls[index];
        if let Some(id) = value
            .get("id")
            .and_then(|value| value.as_str())
            .filter(|value| !value.is_empty())
        {
            call.id = Some(id.to_string());
        }
        if let Some(function) = value.get("function") {
            if let Some(name) = function
                .get("name")
                .and_then(|value| value.as_str())
                .filter(|value| !value.is_empty())
            {
                call.name = Some(name.to_string());
            }
            if let Some(arguments) = function.get("arguments").and_then(|value| value.as_str()) {
                call.arguments.push_str(arguments);
            }
        }
    }
}

fn response_stream_events(response: &Value) -> Vec<Bytes> {
    let mut events = Vec::new();
    let mut created = response.clone();
    if let Some(object) = created.as_object_mut() {
        object.insert("status".to_string(), json!("in_progress"));
    }
    events.push(response_sse_event(&json!({
        "type": "response.created",
        "response": created
    })));
    events.extend(response_output_events(response));
    events.push(response_sse_event(&json!({
        "type": "response.completed",
        "response": response
    })));
    events.push(done_sse_event());
    events
}

fn response_sse_event(value: &Value) -> Bytes {
    let data = serde_json::to_string(value).unwrap_or_else(|_| "{}".to_string());
    let event = value
        .get("type")
        .and_then(|value| value.as_str())
        .unwrap_or("message");
    Bytes::from(format!("event: {}\ndata: {}\n\n", event, data))
}

fn done_sse_event() -> Bytes {
    Bytes::from_static(b"data: [DONE]\n\n")
}

fn response_output_events(response: &Value) -> Vec<Bytes> {
    let output_items = response
        .get("output")
        .and_then(|value| value.as_array())
        .cloned()
        .unwrap_or_default();

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

    match item.get("type").and_then(|value| value.as_str()) {
        Some("message") => {
            if let Some(content) = item.get("content").and_then(|value| value.as_array()) {
                for (content_index, part) in content.iter().enumerate() {
                    if part.get("type").and_then(|value| value.as_str()) != Some("output_text") {
                        continue;
                    }
                    let text = part
                        .get("text")
                        .and_then(|value| value.as_str())
                        .unwrap_or("");
                    let item_id = response_item_id(item);
                    events.push(response_sse_event(&json!({
                        "type": "response.content_part.added",
                        "item_id": item_id,
                        "output_index": output_index,
                        "content_index": content_index,
                        "part": {"type": "output_text", "text": ""}
                    })));
                    if !text.is_empty() {
                        events.push(response_sse_event(&json!({
                            "type": "response.output_text.delta",
                            "item_id": item_id,
                            "output_index": output_index,
                            "content_index": content_index,
                            "delta": text
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
            let arguments = item
                .get("arguments")
                .and_then(|value| value.as_str())
                .unwrap_or("{}");
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
        object.insert("status".to_string(), json!(status));
    }
    item
}

fn response_item_id(item: &Value) -> String {
    item.get("id")
        .and_then(|value| value.as_str())
        .or_else(|| item.get("call_id").and_then(|value| value.as_str()))
        .map(str::to_string)
        .unwrap_or_else(|| format!("item_{}", Uuid::new_v4().simple()))
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
                        crate::record_copilot_error(&usage_state, &usage_context, message);
                        lifecycle.finish();
                        yield Ok::<Bytes, std::io::Error>(bytes);
                        return;
                    }
                    yield Ok::<Bytes, std::io::Error>(bytes);
                }
                Err(err) => {
                    let message = format!("Copilot stream read failed: {}", err);
                    crate::record_copilot_error(&usage_state, &usage_context, &message);
                    lifecycle.finish();
                    yield Err(std::io::Error::new(std::io::ErrorKind::Other, "stream"));
                    return;
                }
            }
        }
        match parser.finish() {
            Err(message) => crate::record_copilot_error(&usage_state, &usage_context, &message),
            Ok(Some(response)) => {
                let usage = crate::usage_metrics_from_response_value(&response);
                crate::record_copilot_success(&usage_state, &usage_context, &usage);
            }
            Ok(None) => crate::record_copilot_success(
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
        if value.get("type").and_then(|value| value.as_str()) == Some("response.completed") {
            if let Some(response) = value.get("response").cloned() {
                self.last_response = Some(response);
            }
        }
    }
}

fn anthropic_messages_to_responses(payload: &Value, model: &str) -> Result<Value, String> {
    let messages = payload
        .get("messages")
        .and_then(|value| value.as_array())
        .ok_or_else(|| "messages is required".to_string())?;
    let mut input = Vec::new();
    for message in messages {
        input.extend(translate_anthropic_message(message)?);
    }

    let mut out = Map::new();
    out.insert("model".to_string(), Value::String(model.to_string()));
    out.insert("input".to_string(), Value::Array(input));
    if let Some(instructions) = translate_anthropic_system(payload.get("system")) {
        out.insert("instructions".to_string(), Value::String(instructions));
    }
    if let Some(max_tokens) = payload.get("max_tokens").and_then(|value| value.as_u64()) {
        out.insert("max_output_tokens".to_string(), json!(max_tokens));
    }
    copy_if_present(payload, &mut out, "temperature");
    copy_if_present(payload, &mut out, "top_p");
    if let Some(tools) = translate_anthropic_tools(payload.get("tools")) {
        out.insert("tools".to_string(), Value::Array(tools));
    }
    if let Some(tool_choice) = translate_anthropic_tool_choice(payload.get("tool_choice")) {
        out.insert("tool_choice".to_string(), tool_choice);
    }
    out.insert("stream".to_string(), Value::Bool(false));
    Ok(Value::Object(out))
}

fn translate_anthropic_message(message: &Value) -> Result<Vec<Value>, String> {
    let role = message
        .get("role")
        .and_then(|value| value.as_str())
        .ok_or_else(|| "message role is required".to_string())?;
    let content = message
        .get("content")
        .ok_or_else(|| "message content is required".to_string())?;
    if role == "user" {
        translate_anthropic_user_content(content)
    } else if role == "assistant" {
        translate_anthropic_assistant_content(content)
    } else {
        Ok(Vec::new())
    }
}

fn translate_anthropic_user_content(content: &Value) -> Result<Vec<Value>, String> {
    if let Some(text) = content.as_str() {
        return Ok(vec![json!({"role": "user", "content": text})]);
    }
    let Some(parts) = content.as_array() else {
        return Ok(Vec::new());
    };
    let mut items = Vec::new();
    let mut message_parts = Vec::new();
    for part in parts {
        match part.get("type").and_then(|value| value.as_str()) {
            Some("tool_result") => {
                let call_id = part
                    .get("tool_use_id")
                    .and_then(|value| value.as_str())
                    .unwrap_or("");
                items.push(json!({
                    "type": "function_call_output",
                    "call_id": call_id,
                    "output": stringify_tool_result(part.get("content"))
                }));
            }
            Some("text") => {
                if let Some(text) = part.get("text").and_then(|value| value.as_str()) {
                    message_parts.push(json!({"type": "input_text", "text": text}));
                }
            }
            Some("image") => {
                if let Some(image) = translate_anthropic_image(part) {
                    message_parts.push(image);
                }
            }
            _ => {}
        }
    }
    if !message_parts.is_empty() {
        items.push(json!({"role": "user", "content": message_parts}));
    }
    Ok(items)
}

fn translate_anthropic_assistant_content(content: &Value) -> Result<Vec<Value>, String> {
    if let Some(text) = content.as_str() {
        return Ok(vec![json!({"role": "assistant", "content": text})]);
    }
    let Some(parts) = content.as_array() else {
        return Ok(Vec::new());
    };
    let mut items = Vec::new();
    let text = parts
        .iter()
        .filter(|part| part.get("type").and_then(|value| value.as_str()) == Some("text"))
        .filter_map(|part| part.get("text").and_then(|value| value.as_str()))
        .collect::<Vec<_>>()
        .join("\n\n");
    if !text.is_empty() {
        items.push(json!({"role": "assistant", "content": text}));
    }
    for part in parts {
        if part.get("type").and_then(|value| value.as_str()) != Some("tool_use") {
            continue;
        }
        let id = part
            .get("id")
            .and_then(|value| value.as_str())
            .unwrap_or("");
        let name = part
            .get("name")
            .and_then(|value| value.as_str())
            .unwrap_or("");
        let arguments = part
            .get("input")
            .cloned()
            .unwrap_or(Value::Object(Map::new()));
        items.push(json!({
            "type": "function_call",
            "call_id": id,
            "name": name,
            "arguments": arguments.to_string()
        }));
    }
    Ok(items)
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

fn translate_anthropic_image(part: &Value) -> Option<Value> {
    let source = part.get("source")?;
    let source_type = source.get("type").and_then(|value| value.as_str())?;
    if source_type == "base64" {
        let media_type = source.get("media_type").and_then(|value| value.as_str())?;
        let data = source.get("data").and_then(|value| value.as_str())?;
        return Some(json!({
            "type": "input_image",
            "image_url": format!("data:{};base64,{}", media_type, data)
        }));
    }
    None
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
                        .map(|value| value.to_string())
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
                "name": name,
                "description": tool.get("description").and_then(|value| value.as_str()).unwrap_or(""),
                "parameters": tool.get("input_schema").cloned().unwrap_or_else(|| json!({"type":"object"}))
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
            .map(|name| json!({"type": "function", "name": name})),
        _ => None,
    }
}

fn responses_to_anthropic_message(response: &Value, client_model: &str) -> Value {
    let content = response_to_anthropic_content(response);
    let has_tool = content
        .iter()
        .any(|block| block.get("type").and_then(|value| value.as_str()) == Some("tool_use"));
    let usage = response.get("usage").cloned().unwrap_or_else(|| json!({}));
    json!({
        "id": response.get("id").and_then(|value| value.as_str()).unwrap_or("msg_copilot"),
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
                        "id": item.get("call_id").or_else(|| item.get("id")).and_then(|value| value.as_str()).unwrap_or("call_copilot"),
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

    let mut index = 0usize;
    if let Some(content) = message.get("content").and_then(|value| value.as_array()) {
        for block in content {
            out.push(sse_event("content_block_start", &json!({
                "type": "content_block_start",
                "index": index,
                "content_block": if block.get("type").and_then(|value| value.as_str()) == Some("text") {
                    json!({"type":"text","text":""})
                } else {
                    block.clone()
                }
            })));
            if block.get("type").and_then(|value| value.as_str()) == Some("text") {
                out.push(sse_event("content_block_delta", &json!({
                    "type": "content_block_delta",
                    "index": index,
                    "delta": {"type": "text_delta", "text": block.get("text").and_then(|value| value.as_str()).unwrap_or("")}
                })));
            }
            out.push(sse_event(
                "content_block_stop",
                &json!({
                    "type": "content_block_stop",
                    "index": index
                }),
            ));
            index += 1;
        }
    }
    out.push(sse_event("message_delta", &json!({
        "type": "message_delta",
        "delta": {
            "stop_reason": message.get("stop_reason").and_then(|value| value.as_str()).unwrap_or("end_turn"),
            "stop_sequence": Value::Null
        },
        "usage": {
            "output_tokens": message.get("usage").and_then(|usage| usage.get("output_tokens")).and_then(|value| value.as_u64()).unwrap_or(0)
        }
    })));
    out.push(sse_event("message_stop", &json!({"type": "message_stop"})));
    out
}

fn sse_event(event: &str, data: &Value) -> String {
    format!("event: {}\ndata: {}\n\n", event, data)
}

fn anthropic_error_body(kind: &str, message: &str) -> Bytes {
    Bytes::from(
        serde_json::to_vec(&json!({
            "type": "error",
            "error": {
                "type": kind,
                "message": message
            }
        }))
        .unwrap_or_default(),
    )
}

fn responses_payload_has_images(value: &Value) -> bool {
    match value {
        Value::Object(map) => map.iter().any(|(key, value)| {
            if key == "tools" {
                return false;
            }
            key == "image_url"
                || key == "input_image"
                || key == "image"
                || responses_payload_has_images(value)
        }),
        Value::Array(items) => items.iter().any(responses_payload_has_images),
        Value::String(text) => text.starts_with("data:image/"),
        _ => false,
    }
}

fn sanitize_responses_payload(value: &mut Value) {
    let Some(object) = value.as_object_mut() else {
        return;
    };
    object.remove("service_tier");

    if let Some(tools) = object
        .get_mut("tools")
        .and_then(|value| value.as_array_mut())
    {
        tools.retain(|tool| {
            tool.get("type").and_then(|value| value.as_str()) != Some("image_generation")
        });
    }
}

fn copy_if_present(input: &Value, output: &mut Map<String, Value>, key: &str) {
    if let Some(value) = input.get(key) {
        output.insert(key.to_string(), value.clone());
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quota_responses_output_ceiling_survives_reasoning_chat_bridge() {
        for model in ["gpt-5", "gpt-5-mini", "o1", "o3-mini", "o4-mini", "gpt-4o"] {
            let payload = responses_to_chat_completions_payload(
                &json!({"input":"hello", "max_output_tokens":25}),
                model,
                false,
            )
            .unwrap();
            let expected_field = if model == "gpt-4o" {
                "max_tokens"
            } else {
                "max_completion_tokens"
            };
            assert_eq!(payload[expected_field], 25, "{model}: {payload}");
            let bounds = crate::quota_usage::prepared_bounds_for_protocol(
                "copilot",
                &payload,
                crate::quota_usage::PreparedProtocol::ChatCompletions,
            )
            .unwrap();
            assert_eq!(bounds.output_upper_bound, Some(25), "{model}: {payload}");
        }
        let payload = responses_to_chat_completions_payload(
            &json!({"input":"hello", "max_output_tokens":25, "max_completion_tokens":40}),
            "gpt-5",
            false,
        )
        .unwrap();
        assert_eq!(payload["max_completion_tokens"], 40);
        assert!(payload.get("max_tokens").is_none());
        let unsupported = responses_to_chat_completions_payload(
            &json!({"input":"hello", "max_output_tokens":25}),
            "unknown-reasoning-model",
            false,
        )
        .unwrap();
        assert!(crate::quota_usage::prepared_bounds_for_protocol(
            "copilot",
            &unsupported,
            crate::quota_usage::PreparedProtocol::ChatCompletions,
        )
        .unwrap()
        .output_upper_bound
        .is_none());
    }

    #[test]
    fn quota_stream_partial_usage_cannot_be_blessed_by_empty_final_event() {
        for (final_usage, complete) in [
            (json!({}), false),
            (json!({"prompt_tokens":10,"completion_tokens":8}), true),
        ] {
            let mut accumulator = ChatCompletionsSseAccumulator::default();
            accumulator.push(&Bytes::from_static(b"data: {\"choices\":[{\"delta\":{\"content\":\"x\"}}],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":0}}\n\n"));
            accumulator.push(&Bytes::from(format!(
                "data: {}\n\n",
                json!({"choices":[{"finish_reason":"stop"}],"usage":final_usage})
            )));
            let response = accumulator.finish("gpt-4.1").unwrap();
            let usage = crate::quota_usage::normalize("copilot", &response["usage"], true);
            assert_eq!(usage.trustworthy_final, complete);
            if complete {
                assert_eq!(usage.output_tokens, Some(8));
            }
        }
    }

    #[test]
    fn models_are_exposed_with_copilot_prefix() {
        let models = vec![CopilotModelInfo {
            id: "gpt-4.1".to_string(),
            name: Some("GPT-4.1".to_string()),
            vendor: Some("openai".to_string()),
            preview: Some(false),
            model_picker_category: Some("versatile".to_string()),
            policy_state: Some("enabled".to_string()),
        }];

        let entries = models_to_openai_entries(&models);
        assert_eq!(entries[0]["id"], "cop:gpt-4.1");
        assert_eq!(entries[0]["upstream_model"], "gpt-4.1");
        assert_eq!(entries[0]["billing_tier"], "non_premium");
        assert_eq!(entries[0]["premium"], false);
        assert_eq!(entries[0]["utility_model"], true);
        assert_eq!(entries[0]["app_accessible"], true);
    }

    #[test]
    fn translates_anthropic_messages_to_responses_payload() {
        let payload = json!({
            "model": "claude-sonnet-4.5",
            "system": "You are concise.",
            "max_tokens": 64,
            "messages": [
                {"role": "user", "content": "Say hi"}
            ],
            "tools": [
                {"name": "lookup", "description": "Lookup", "input_schema": {"type": "object"}}
            ]
        });

        let out = anthropic_messages_to_responses(&payload, "claude-sonnet-4.5").unwrap();
        assert_eq!(out["model"], "claude-sonnet-4.5");
        assert_eq!(out["instructions"], "You are concise.");
        assert_eq!(out["max_output_tokens"], 64);
        assert_eq!(out["input"][0]["role"], "user");
        assert_eq!(out["tools"][0]["type"], "function");
    }

    #[test]
    fn converts_responses_output_to_anthropic_message() {
        let response = json!({
            "id": "resp_1",
            "output": [{
                "type": "message",
                "content": [{"type": "output_text", "text": "hello"}]
            }],
            "usage": {"input_tokens": 2, "output_tokens": 1}
        });

        let msg = responses_to_anthropic_message(&response, "claude-sonnet-4.5");
        assert_eq!(msg["type"], "message");
        assert_eq!(msg["content"][0]["type"], "text");
        assert_eq!(msg["content"][0]["text"], "hello");
        assert_eq!(msg["usage"]["input_tokens"], 2);
    }

    #[test]
    fn sanitizes_codex_only_responses_fields() {
        let mut payload = json!({
            "model": "gpt-5.1",
            "input": "hi",
            "service_tier": "fast",
            "tools": [
                {"type": "function", "name": "lookup"},
                {"type": "image_generation"}
            ]
        });

        sanitize_responses_payload(&mut payload);
        assert!(payload.get("service_tier").is_none());
        assert_eq!(payload["tools"].as_array().unwrap().len(), 1);
        assert_eq!(payload["tools"][0]["type"], "function");
    }

    #[test]
    fn image_detection_ignores_tool_schemas() {
        let payload = json!({
            "model": "gpt-4.1",
            "input": "inspect repository",
            "tools": [{
                "type": "function",
                "name": "trace",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "image_url": {"type": "string"},
                        "image": {"type": "string"}
                    }
                }
            }]
        });
        assert!(!responses_payload_has_images(&payload));

        let payload_with_image = json!({
            "model": "gpt-4.1",
            "input": [{
                "role": "user",
                "content": [{"type": "input_image", "image_url": "data:image/png;base64,abc"}]
            }]
        });
        assert!(responses_payload_has_images(&payload_with_image));
    }

    #[test]
    fn responses_to_chat_payload_maps_simple_input() {
        let payload = json!({
            "model": "gpt-4.1",
            "instructions": "Be terse.",
            "input": "Reply pong.",
            "max_output_tokens": 16
        });

        let chat = responses_to_chat_completions_payload(&payload, "gpt-4.1", false).unwrap();
        assert_eq!(chat["model"], "gpt-4.1");
        assert_eq!(chat["max_tokens"], 16);
        assert_eq!(chat["stream"], false);
        assert_eq!(chat["messages"][0]["role"], "system");
        assert_eq!(chat["messages"][1]["role"], "user");
        assert_eq!(chat["messages"][1]["content"], "Reply pong.");
    }

    #[test]
    fn responses_to_chat_payload_maps_tools_and_tool_history() {
        let payload = json!({
            "model": "gpt-4.1",
            "input": [
                {"role": "user", "content": "lookup alpha"},
                {"type": "function_call", "call_id": "call_1", "name": "lookup", "arguments": "{\"query\":\"alpha\"}"},
                {"type": "function_call_output", "call_id": "call_1", "output": "alpha=42"},
                {"role": "user", "content": [{"type": "input_text", "text": "finish"}]}
            ],
            "tools": [{
                "type": "function",
                "name": "lookup",
                "description": "Lookup a value",
                "parameters": {"type": "object", "properties": {"query": {"type": "string"}}}
            }],
            "tool_choice": {"type": "function", "name": "lookup"},
            "parallel_tool_calls": true
        });

        let chat = responses_to_chat_completions_payload(&payload, "gpt-4.1", true).unwrap();
        assert_eq!(chat["stream"], true);
        assert_eq!(chat["messages"][0]["role"], "user");
        assert_eq!(chat["messages"][1]["role"], "assistant");
        assert_eq!(chat["messages"][1]["tool_calls"][0]["id"], "call_1");
        assert_eq!(
            chat["messages"][1]["tool_calls"][0]["function"]["name"],
            "lookup"
        );
        assert_eq!(chat["messages"][2]["role"], "tool");
        assert_eq!(chat["messages"][2]["tool_call_id"], "call_1");
        assert_eq!(chat["messages"][2]["content"], "alpha=42");
        assert_eq!(chat["messages"][3]["content"], "finish");
        assert_eq!(chat["tools"][0]["function"]["name"], "lookup");
        assert_eq!(chat["tool_choice"]["function"]["name"], "lookup");
        assert_eq!(chat["parallel_tool_calls"], true);
    }

    #[test]
    fn chat_completion_to_response_maps_text_and_usage() {
        let chat = json!({
            "id": "chatcmpl_1",
            "created": 123,
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": "pong"
                }
            }],
            "usage": {
                "prompt_tokens": 2,
                "completion_tokens": 1,
                "total_tokens": 3
            }
        });

        let response = chat_completion_to_response(&chat, "gpt-4.1");
        assert_eq!(response["id"], "chatcmpl_1");
        assert_eq!(response["model"], "gpt-4.1");
        assert_eq!(response["output_text"], "pong");
        assert_eq!(response["usage"]["input_tokens"], 2);
        assert_eq!(response["usage"]["output_tokens"], 1);
    }

    #[test]
    fn chat_completion_to_response_maps_tool_calls() {
        let chat = json!({
            "id": "chatcmpl_2",
            "created": 123,
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_abc",
                        "type": "function",
                        "function": {
                            "name": "lookup",
                            "arguments": "{\"query\":\"alpha\"}"
                        }
                    }]
                }
            }]
        });

        let response = chat_completion_to_response(&chat, "gpt-4.1");
        assert_eq!(response["output_text"], "");
        assert_eq!(response["output"][0]["type"], "function_call");
        assert_eq!(response["output"][0]["call_id"], "call_abc");
        assert_eq!(response["output"][0]["name"], "lookup");
        assert_eq!(response["output"][0]["arguments"], "{\"query\":\"alpha\"}");
    }

    #[test]
    fn chat_stream_accumulator_maps_tool_call_chunks() {
        let mut accumulator = ChatCompletionsSseAccumulator::default();
        accumulator.push(&Bytes::from_static(
            br#"data: {"id":"chatcmpl_3","created":123,"model":"gpt-4.1","choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[{"index":0,"id":"call_stream","type":"function","function":{"name":"lookup","arguments":""}}]}}]}

"#,
        ));
        accumulator.push(&Bytes::from_static(
            br#"data: {"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"query\""}}]}}]}

data: {"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":":\"beta\"}"}}]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":5,"completion_tokens":3,"total_tokens":8}}

data: [DONE]

"#,
        ));

        let response = accumulator.finish("gpt-4.1").unwrap();
        assert_eq!(response["id"], "chatcmpl_3");
        assert_eq!(response["output"][0]["type"], "function_call");
        assert_eq!(response["output"][0]["call_id"], "call_stream");
        assert_eq!(response["output"][0]["name"], "lookup");
        assert_eq!(response["output"][0]["arguments"], "{\"query\":\"beta\"}");
        assert_eq!(response["usage"]["total_tokens"], 8);
    }

    #[test]
    fn copilot_sse_trackers_reject_terminal_error_events() {
        let mut chat = ChatCompletionsSseAccumulator::default();
        chat.push(&Bytes::from_static(
            b"event: error\ndata: {\"message\":\"copilot quota exhausted\"}\n\n",
        ));
        assert!(matches!(
            chat.finish("gpt-4.1"),
            Err(message) if message == "copilot quota exhausted"
        ));

        let mut native = NativeResponsesSseUsageTracker::default();
        native.push(&Bytes::from_static(
            b"data: {\"type\":\"error\",\"error\":{\"message\":\"copilot failed\"}}\n\n",
        ));
        assert!(matches!(
            native.finish(),
            Err(message) if message == "copilot failed"
        ));
    }
}
