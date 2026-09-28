use axum::{
    body::Body,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
};
use bytes::Bytes;
use futures_util::StreamExt;
use std::time::Duration;

const DEFAULT_BASE_URL: &str = "https://api.x.ai/v1";

const MODEL_FALLBACKS: &[(&str, &str)] = &[
    ("grok-4.3", "Grok 4.3"),
    ("grok-4.1", "Grok 4.1"),
    ("grok-3", "Grok 3"),
    ("grok-imagine-image-quality", "Grok Imagine Image (Quality)"),
    ("grok-imagine-image", "Grok Imagine Image"),
    ("grok-imagine-video", "Grok Imagine Video"),
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

    let models: Vec<serde_json::Value> = if let Some(account) =
        super::accounts::first_enabled(&state)
    {
        if !account.models.is_empty() {
            account
                .models
                .iter()
                .map(|model| {
                    serde_json::json!({
                        "id": model.model_id,
                        "object": "model",
                        "created": 0u64,
                        "owned_by": if model.owned_by.is_empty() { "xai" } else { model.owned_by.as_str() },
                        "display_name": if model.display_name.is_empty() { model.model_id.as_str() } else { model.display_name.as_str() },
                        "aliases": model.aliases,
                        "context_window": model.context_window,
                        "capabilities": ["chat", "text", "images", "video"]
                    })
                })
                .collect()
        } else {
            fallback_models()
        }
    } else {
        fallback_models()
    };

    axum::Json(serde_json::json!({
        "object": "list",
        "data": models
    }))
    .into_response()
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
        Ok(v) => v,
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

    let model = request_value
        .get("model")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .unwrap_or("grok-4.3")
        .to_string();

    let stream = request_value
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let prompt_metrics = crate::prompt_metrics_from_request_value(&request_value);
    let mut payload = serde_json::json!({
        "model": &model,
        "input": request_value.get("input").cloned().unwrap_or(serde_json::Value::Null),
        "store": false,
    });

    if let Some(instructions) = request_value
        .get("instructions")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|v| !v.is_empty())
    {
        payload["instructions"] = serde_json::Value::String(instructions.to_string());
    }

    if let Some(tools) = request_value.get("tools").filter(|v| v.is_array()) {
        payload["tools"] = tools.clone();
        payload["tool_choice"] = serde_json::Value::String("auto".to_string());
        payload["parallel_tool_calls"] = serde_json::Value::Bool(true);
    }

    if stream {
        payload["stream"] = serde_json::Value::Bool(true);
    }

    let payload_body = serde_json::to_string(&payload).unwrap_or_default();
    let accounts = super::accounts::candidate_accounts(&state);
    if accounts.is_empty() {
        crate::release_api_key_budget_before_dispatch(&state);
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            [("Content-Type", "application/json")],
            crate::source::v1::response::openai_error_body(
                "No Grok accounts configured",
                "server_error",
                None,
            ),
        )
            .into_response();
    }

    let mut last_error: Option<(StatusCode, String)> = None;
    for (attempt_idx, account) in accounts.iter().enumerate() {
        let context = crate::grok_usage_context(
            account,
            Some(model.clone()),
            "/grok/v1/responses",
            prompt_metrics.clone(),
        );
        if let Err(response) = crate::reserve_api_key_budgets_for_prepared_dispatch(
            &state,
            context.provider_name,
            &context.key,
            payload_body.as_bytes(),
        )
        .await
        {
            return response;
        }
        crate::record_request_started(&state, &context);

        let upstream_base = account
            .api_base_url
            .as_deref()
            .unwrap_or(DEFAULT_BASE_URL)
            .trim_end_matches('/');
        let upstream_url = format!("{}/responses", upstream_base);

        match state
            .client
            .post(&upstream_url)
            .header(
                "Authorization",
                format!("{} {}", account.token_type, account.access_token),
            )
            .header("Content-Type", "application/json")
            .header(
                "Accept",
                if stream {
                    "text/event-stream"
                } else {
                    "application/json"
                },
            )
            .body(payload_body.clone())
            .timeout(Duration::from_secs(180))
            .send()
            .await
        {
            Ok(resp) => {
                let status = resp.status();
                let rate_limits = super::auth::extract_rate_limits(resp.headers());
                if !status.is_success() {
                    persist_runtime_metadata(
                        &state,
                        account,
                        Some(model.as_str()),
                        &rate_limits,
                        "after error",
                    );
                    let err_body = resp.text().await.unwrap_or_default();
                    let message = err_body.clone();
                    crate::record_grok_error(&state, &context, &message);
                    if attempt_idx + 1 < accounts.len()
                        && crate::should_retry_account_error(status, &message)
                    {
                        last_error = Some((status, message));
                        continue;
                    }
                    let body_bytes = Bytes::from(err_body);
                    return (
                        StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY),
                        [("Content-Type", "application/json")],
                        crate::source::v1::response::upstream_error_to_openai(status, &body_bytes),
                    )
                        .into_response();
                }

                if stream {
                    persist_runtime_metadata(
                        &state,
                        account,
                        Some(model.as_str()),
                        &rate_limits,
                        "",
                    );
                    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, axum::Error>>(16);
                    let state_clone = state.clone();
                    let context_clone = context.clone();
                    let rl_headers = forwarded_ratelimit_headers(resp.headers());
                    let mut upstream: crate::ReqwestByteStream = Box::pin(resp.bytes_stream());
                    let prefix = match crate::read_sse_prelude(
                        &mut upstream,
                        Duration::from_secs(state.cfg.upstream_first_event_timeout_seconds.max(1)),
                    )
                    .await
                    {
                        Ok(prefix) => prefix,
                        Err(message) => {
                            crate::record_grok_error(&state, &context, &message);
                            if attempt_idx + 1 < accounts.len()
                                && crate::should_retry_account_error(
                                    StatusCode::BAD_GATEWAY,
                                    &message,
                                )
                            {
                                last_error = Some((StatusCode::BAD_GATEWAY, message));
                                continue;
                            }
                            return grok_response_error(&message);
                        }
                    };
                    // Construct before spawning so cancellation before the
                    // first poll also finishes the account/budget lifecycle.
                    let mut cleanup = crate::StreamRequestGuard::new(&state, &context);
                    tokio::spawn(async move {
                        let mut chunk_stream: crate::ReqwestByteStream = Box::pin(
                            futures_util::stream::once(async move {
                                Ok::<Bytes, reqwest::Error>(prefix)
                            })
                            .chain(upstream),
                        );
                        let mut buffer = Vec::new();
                        let mut failed = false;
                        loop {
                            let chunk = match next_grok_stream_chunk(&mut chunk_stream, &tx).await {
                                GrokStreamRead::ClientDisconnected => return,
                                GrokStreamRead::Upstream(None) => break,
                                GrokStreamRead::Upstream(Some(chunk)) => chunk,
                            };
                            match chunk {
                                Ok(bytes) => {
                                    buffer.extend_from_slice(&bytes);
                                    if tx.send(Ok(bytes)).await.is_err() {
                                        // Cleanup conservatively charges the
                                        // dispatched input on disconnect.
                                        return;
                                    }
                                }
                                Err(e) => {
                                    let message = format!("Grok stream read failed: {}", e);
                                    let _ = tx.send(Err(axum::Error::new(e))).await;
                                    crate::record_grok_error(
                                        &state_clone,
                                        &context_clone,
                                        &message,
                                    );
                                    failed = true;
                                    break;
                                }
                            }
                        }
                        if !failed {
                            let body_bytes = Bytes::from(buffer);
                            match grok_sse_outcome(&body_bytes) {
                                Ok(Some(usage)) => {
                                    crate::record_grok_success(&state_clone, &context_clone, &usage)
                                }
                                Ok(None) => crate::record_request_completed_with_unknown_usage(
                                    &state_clone,
                                    &context_clone,
                                ),
                                Err(message) => {
                                    crate::record_grok_error(&state_clone, &context_clone, &message)
                                }
                            }
                        }
                        cleanup.finish();
                    });

                    let stream_body = Body::from_stream(rx_stream(rx));
                    let mut response = (
                        StatusCode::OK,
                        [("Content-Type", "text/event-stream")],
                        stream_body,
                    )
                        .into_response();
                    let response_headers = response.headers_mut();
                    for (k, v) in rl_headers {
                        response_headers.insert(k, v);
                    }
                    return response;
                } else {
                    let rl_headers = forwarded_ratelimit_headers(resp.headers());
                    let is_sse = resp
                        .headers()
                        .get(reqwest::header::CONTENT_TYPE)
                        .and_then(|value| value.to_str().ok())
                        .is_some_and(|value| value.contains("text/event-stream"));
                    let body_bytes = match resp.bytes().await {
                        Ok(bytes) => bytes,
                        Err(error) => {
                            let message = format!("Grok response body failed: {}", error);
                            crate::record_grok_error(&state, &context, &message);
                            return grok_response_error(&message);
                        }
                    };
                    // Some compatible upstreams return SSE even for a
                    // buffered request. Inspect it before converting; an
                    // error envelope must not turn into an empty success.
                    let (body_bytes, unknown_sse_usage) = if is_sse {
                        let usage = match grok_sse_outcome(&body_bytes) {
                            Ok(usage) => usage,
                            Err(message) => {
                                crate::record_grok_error(&state, &context, &message);
                                return grok_response_error(&message);
                            }
                        };
                        (
                            crate::source::v1::response::sse_to_response_json(&body_bytes),
                            usage.is_none(),
                        )
                    } else {
                        (body_bytes, false)
                    };
                    let response_value: serde_json::Value =
                        match serde_json::from_slice(&body_bytes) {
                            Ok(value) => value,
                            Err(_) => {
                                let message = "Grok returned an invalid JSON response";
                                crate::record_grok_error(&state, &context, message);
                                return grok_response_error(message);
                            }
                        };
                    if let Some(message) = crate::sse_error_from_value(&response_value) {
                        crate::record_grok_error(&state, &context, &message);
                        return grok_response_error(&message);
                    }
                    let usage = crate::usage_metrics_from_response_value(&response_value);
                    if unknown_sse_usage {
                        crate::record_request_completed_with_unknown_usage(&state, &context);
                    } else {
                        crate::record_grok_success(&state, &context, &usage);
                    }
                    let effective_model = response_value
                        .get("model")
                        .and_then(|v| v.as_str())
                        .filter(|value| !value.trim().is_empty())
                        .unwrap_or(model.as_str());
                    persist_runtime_metadata(
                        &state,
                        account,
                        Some(effective_model),
                        &rate_limits,
                        "",
                    );
                    let mut response = (
                        StatusCode::OK,
                        [("Content-Type", "application/json")],
                        body_bytes,
                    )
                        .into_response();
                    let response_headers = response.headers_mut();
                    for (k, v) in rl_headers {
                        response_headers.insert(k, v);
                    }
                    return response;
                }
            }
            Err(err) => {
                let message = format!("grok upstream unavailable: {}", err);
                crate::record_grok_error(&state, &context, &message);
                last_error = Some((StatusCode::BAD_GATEWAY, message));
                if attempt_idx + 1 < accounts.len() {
                    continue;
                }
                break;
            }
        }
    }

    let (status, message) = last_error.unwrap_or_else(|| {
        (
            StatusCode::BAD_GATEWAY,
            "All Grok accounts failed".to_string(),
        )
    });
    (
        status,
        [("Content-Type", "application/json")],
        crate::source::v1::response::openai_error_body(
            &format!("All Grok accounts failed; last error: {}", message),
            "server_error",
            None,
        ),
    )
        .into_response()
}

fn grok_response_error(message: &str) -> axum::response::Response {
    (
        StatusCode::BAD_GATEWAY,
        [("Content-Type", "application/json")],
        crate::source::v1::response::openai_error_body(message, "server_error", None),
    )
        .into_response()
}

fn grok_sse_outcome(body: &Bytes) -> Result<Option<crate::UsageMetrics>, String> {
    crate::sse_response_body_outcome(body)
}

enum GrokStreamRead {
    ClientDisconnected,
    Upstream(Option<Result<Bytes, reqwest::Error>>),
}

async fn next_grok_stream_chunk(
    upstream: &mut crate::ReqwestByteStream,
    downstream: &tokio::sync::mpsc::Sender<Result<Bytes, axum::Error>>,
) -> GrokStreamRead {
    tokio::select! {
        // A silent upstream must not keep an abandoned account and its
        // budget hold active until the full upstream read timeout expires.
        biased;
        _ = downstream.closed() => GrokStreamRead::ClientDisconnected,
        chunk = upstream.next() => GrokStreamRead::Upstream(chunk),
    }
}

/// Returns the `x-ratelimit-*` headers from the upstream response, ready to be
/// spread into the axum response tuple so callers can see the live quota on
/// every response.
fn forwarded_ratelimit_headers(
    headers: &reqwest::header::HeaderMap,
) -> Vec<(axum::http::HeaderName, axum::http::HeaderValue)> {
    let mut out = Vec::new();
    for (name, value) in headers.iter() {
        let n = name.as_str().to_ascii_lowercase();
        if n.starts_with("x-ratelimit-") {
            if let (Ok(aname), Ok(avalue)) = (
                axum::http::HeaderName::from_bytes(name.as_str().as_bytes()),
                axum::http::HeaderValue::from_bytes(value.as_bytes()),
            ) {
                out.push((aname, avalue));
            }
        }
    }
    out
}

fn persist_runtime_metadata(
    state: &crate::AppState,
    account: &super::accounts::GrokAccount,
    effective_model: Option<&str>,
    rate_limits: &[super::auth::GrokRateLimitInfo],
    context: &str,
) {
    match super::auth::persist_runtime_metadata(
        &state.cfg,
        account.file_name.as_deref(),
        effective_model,
        rate_limits,
    ) {
        Ok(()) => super::accounts::update_runtime_metadata(
            state,
            account.file_name.as_deref(),
            effective_model,
            rate_limits,
        ),
        Err(err) => tracing::warn!(
            "failed to persist Grok runtime metadata {}: {}",
            context,
            err
        ),
    }
}

/// Forwards `POST /v1/images/generations` to `POST {account.api_base_url}/images/generations`.
pub async fn image_generations(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    proxy_simple(&state, &headers, &body, "images/generations").await
}

/// Forwards `POST /v1/videos/generations` to `POST {account.api_base_url}/videos/generations`.
pub async fn video_generations(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    proxy_simple(&state, &headers, &body, "videos/generations").await
}

async fn proxy_simple(
    state: &crate::AppState,
    headers: &HeaderMap,
    body: &Bytes,
    upstream_suffix: &str,
) -> axum::response::Response {
    if !crate::check_api_key(&state, headers) {
        crate::release_api_key_budget_before_dispatch(state);
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

    let parsed_body: Option<serde_json::Value> =
        match serde_json::from_slice::<serde_json::Value>(body) {
            Ok(value) if value.is_object() => Some(value),
            _ => {
                crate::release_api_key_budget_before_dispatch(state);
                return (
                    StatusCode::BAD_REQUEST,
                    [("Content-Type", "application/json")],
                    crate::source::v1::response::openai_error_body(
                        "Request body must be a JSON object",
                        "invalid_request_error",
                        None,
                    ),
                )
                    .into_response();
            }
        };
    let model = parsed_body
        .as_ref()
        .and_then(|v| v.get("model").and_then(|m| m.as_str()).map(str::to_string));
    let prompt_metrics = parsed_body
        .as_ref()
        .map(crate::prompt_metrics_from_request_value)
        .unwrap_or_default();

    let accounts = super::accounts::candidate_accounts(state);
    if accounts.is_empty() {
        crate::release_api_key_budget_before_dispatch(state);
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            [("Content-Type", "application/json")],
            crate::source::v1::response::openai_error_body(
                "No Grok accounts configured",
                "server_error",
                None,
            ),
        )
            .into_response();
    }

    let mut last_error: Option<(StatusCode, String)> = None;
    for (attempt_idx, account) in accounts.iter().enumerate() {
        let upstream_base = account
            .api_base_url
            .as_deref()
            .unwrap_or(DEFAULT_BASE_URL)
            .trim_end_matches('/');
        let upstream_url = format!("{}/{}", upstream_base, upstream_suffix);
        let context = crate::grok_usage_context(
            account,
            model.clone(),
            &format!("/grok/v1/{}", upstream_suffix),
            prompt_metrics.clone(),
        );
        if let Err(response) = crate::reserve_api_key_budgets_for_prepared_dispatch(
            state,
            context.provider_name,
            &context.key,
            body,
        )
        .await
        {
            return response;
        }
        crate::record_request_started(state, &context);

        match state
            .client
            .post(&upstream_url)
            .header(
                "Authorization",
                format!("{} {}", account.token_type, account.access_token),
            )
            .header("Content-Type", "application/json")
            .body(body.to_vec())
            .timeout(Duration::from_secs(180))
            .send()
            .await
        {
            Ok(resp) => {
                let status = resp.status();
                let rate_limits = super::auth::extract_rate_limits(resp.headers());
                if !status.is_success() {
                    persist_runtime_metadata(
                        state,
                        account,
                        model.as_deref(),
                        &rate_limits,
                        "after error",
                    );
                    let err_body = resp.text().await.unwrap_or_default();
                    crate::record_grok_error(state, &context, &err_body);
                    if attempt_idx + 1 < accounts.len()
                        && crate::should_retry_account_error(status, &err_body)
                    {
                        last_error = Some((status, err_body));
                        continue;
                    }
                    let body_bytes = Bytes::from(err_body);
                    return (
                        StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY),
                        [("Content-Type", "application/json")],
                        crate::source::v1::response::upstream_error_to_openai(status, &body_bytes),
                    )
                        .into_response();
                }
                persist_runtime_metadata(state, account, model.as_deref(), &rate_limits, "");
                let rl_headers = forwarded_ratelimit_headers(resp.headers());
                let body_bytes = match resp.bytes().await {
                    Ok(bytes) => bytes,
                    Err(err) => {
                        let message = format!("Grok media response body failed: {}", err);
                        crate::record_grok_error(state, &context, &message);
                        return (
                            StatusCode::BAD_GATEWAY,
                            [("Content-Type", "application/json")],
                            crate::source::v1::response::openai_error_body(
                                &message,
                                "server_error",
                                None,
                            ),
                        )
                            .into_response();
                    }
                };
                if let Ok(value) = serde_json::from_slice::<serde_json::Value>(&body_bytes) {
                    let usage = crate::usage_metrics_from_response_value(&value);
                    crate::record_grok_success(state, &context, &usage);
                } else {
                    // A 2xx media response may omit the normal JSON usage
                    // shape. It has already reached Grok, so settle its
                    // managed-key input hold conservatively now.
                    crate::record_request_completed_with_unknown_usage(state, &context);
                }
                let mut response = (
                    StatusCode::OK,
                    [("Content-Type", "application/json")],
                    body_bytes,
                )
                    .into_response();
                let h = response.headers_mut();
                for (k, v) in rl_headers {
                    h.insert(k, v);
                }
                return response;
            }
            Err(err) => {
                let message = format!("grok upstream unavailable: {}", err);
                crate::record_grok_error(state, &context, &message);
                last_error = Some((StatusCode::BAD_GATEWAY, message));
                if attempt_idx + 1 < accounts.len() {
                    continue;
                }
                break;
            }
        }
    }

    let (status, message) = last_error.unwrap_or_else(|| {
        (
            StatusCode::BAD_GATEWAY,
            "All Grok accounts failed".to_string(),
        )
    });
    (
        status,
        [("Content-Type", "application/json")],
        crate::source::v1::response::openai_error_body(
            &format!("All Grok accounts failed; last error: {}", message),
            "server_error",
            None,
        ),
    )
        .into_response()
}

fn rx_stream(
    mut rx: tokio::sync::mpsc::Receiver<Result<Bytes, axum::Error>>,
) -> impl futures_util::Stream<Item = Result<Bytes, axum::Error>> {
    async_stream::stream! {
        while let Some(chunk) = rx.recv().await {
            yield chunk;
        }
    }
}

fn fallback_models() -> Vec<serde_json::Value> {
    MODEL_FALLBACKS
        .iter()
        .map(|(id, name)| {
            serde_json::json!({
                "id": id,
                "object": "model",
                "created": 0u64,
                "owned_by": "xai",
                "display_name": name,
                "capabilities": ["chat", "text", "images", "video"]
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{grok_sse_outcome, next_grok_stream_chunk, GrokStreamRead, DEFAULT_BASE_URL};
    use bytes::Bytes;

    #[test]
    fn grok_responses_url_matches_xai_docs() {
        let upstream_url = format!("{}/responses", DEFAULT_BASE_URL.trim_end_matches('/'));
        assert_eq!(upstream_url, "https://api.x.ai/v1/responses");
    }

    #[test]
    fn sse_accounting_grok_terminal_error_then_done_cannot_be_success() {
        let body = Bytes::from_static(
            b"data:{\"type\":\"response.output_text.delta\",\"delta\":\"partial\"}\n\nevent:error\ndata:{\"message\":\"upstream rejected request\"}\n\ndata:[DONE]\n\n",
        );
        assert_eq!(
            grok_sse_outcome(&body).err().as_deref(),
            Some("upstream rejected request")
        );
    }

    #[test]
    fn sse_accounting_grok_eof_without_terminal_event_is_a_failure() {
        let body = Bytes::from_static(
            b"data:{\"type\":\"response.output_text.delta\",\"delta\":\"partial\"}\n\n",
        );
        assert!(grok_sse_outcome(&body).is_err());
        assert!(grok_sse_outcome(&Bytes::new()).is_err());
    }

    #[test]
    fn sse_accounting_grok_completed_preserves_no_space_usage() {
        let body = Bytes::from_static(
            b"event:response.completed\r\ndata:{\"response\":{\"error\":null,\"usage\":{\"input_tokens\":17,\"output_tokens\":3}}}\r\n\r\n",
        );
        let usage = grok_sse_outcome(&body).unwrap().unwrap();
        assert_eq!(usage.input_tokens, 17);
        assert_eq!(usage.output_tokens, 3);
    }

    #[test]
    fn sse_accounting_grok_done_without_usage_remains_unknown() {
        assert!(grok_sse_outcome(&Bytes::from_static(b"data:[DONE]\n\n"))
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn sse_accounting_grok_disconnect_interrupts_silent_upstream() {
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        let mut upstream: crate::ReqwestByteStream = Box::pin(futures_util::stream::pending());
        let worker = tokio::spawn(async move { next_grok_stream_chunk(&mut upstream, &tx).await });
        tokio::task::yield_now().await;
        drop(rx);

        let result = tokio::time::timeout(std::time::Duration::from_millis(250), worker)
            .await
            .expect("client disconnect must not wait for another upstream chunk")
            .unwrap();
        assert!(matches!(result, GrokStreamRead::ClientDisconnected));
    }
}
