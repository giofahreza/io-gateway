use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use bytes::Bytes;
use serde_json::{json, Value};
use std::time::{Duration, Instant};

use super::{accounts::MiniMaxAccount, api::normalize_base_url};

pub const IMAGE_MODEL: &str = "image-01";
pub const IMAGE_MODEL_LIVE: &str = "image-01-live";
pub const VIDEO_MODEL_H3: &str = "MiniMax-H3";
pub const VIDEO_MODEL_H3_MAX: &str = "MiniMax-H3-Max";

const VIDEO_TASK_BINDING_TTL: Duration = Duration::from_secs(7 * 24 * 60 * 60);
const IMAGE_ASPECT_RATIOS: &[&str] = &["1:1", "16:9", "4:3", "3:2", "2:3", "3:4", "9:16", "21:9"];
const VIDEO_ASPECT_RATIOS: &[&str] = &["adaptive", "21:9", "16:9", "4:3", "1:1", "3:4", "9:16"];
const LEGACY_VIDEO_MODELS: &[&str] = &[
    "MiniMax-Hailuo-2.3",
    "MiniMax-Hailuo-2.3-Fast",
    "MiniMax-Hailuo-02",
    "T2V-01-Director",
    "T2V-01",
    "I2V-01-Director",
    "I2V-01-live",
    "I2V-01",
    "S2V-01",
];

#[derive(Clone, Copy, Eq, PartialEq)]
enum VideoApi {
    H3V2,
    LegacyV1,
}

/// Associates a MiniMax video task with the account that created it. MiniMax
/// tasks are account-scoped, so polling must not use round-robin routing.
#[derive(Clone)]
pub(crate) struct VideoTaskBinding {
    pub account_key: String,
    pub model: String,
    api: VideoApi,
    pub created_at: Instant,
}

pub(crate) fn media_model_records() -> Vec<Value> {
    let mut models = vec![
        json!({
            "id": IMAGE_MODEL,
            "object": "model",
            "created": 0,
            "owned_by": "minimax",
            "capabilities": ["images"]
        }),
        json!({
            "id": IMAGE_MODEL_LIVE,
            "object": "model",
            "created": 0,
            "owned_by": "minimax",
            "capabilities": ["images"]
        }),
        json!({
            "id": VIDEO_MODEL_H3,
            "object": "model",
            "created": 0,
            "owned_by": "minimax",
            "capabilities": ["video"]
        }),
        json!({
            "id": VIDEO_MODEL_H3_MAX,
            "object": "model",
            "created": 0,
            "owned_by": "minimax",
            "capabilities": ["video"]
        }),
    ];
    models.extend(LEGACY_VIDEO_MODELS.iter().map(|id| {
        json!({
            "id": id,
            "object": "model",
            "created": 0,
            "owned_by": "minimax",
            "capabilities": ["video"]
        })
    }));
    models
}

pub async fn image_generations(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if !crate::check_api_key(&state, &headers) {
        crate::release_api_key_budget_before_dispatch(&state);
        return authentication_error();
    }

    let raw: Value = match serde_json::from_slice(&body) {
        Ok(value) => value,
        Err(_) => {
            crate::release_api_key_budget_before_dispatch(&state);
            return invalid_request("Invalid request body");
        }
    };
    let payload = match build_image_payload(&raw) {
        Ok(payload) => payload,
        Err(err) => {
            crate::release_api_key_budget_before_dispatch(&state);
            return invalid_request(&err);
        }
    };
    let Some(account) = super::accounts::candidate_accounts(&state)
        .into_iter()
        .next()
    else {
        crate::release_api_key_budget_before_dispatch(&state);
        return no_accounts();
    };

    // Media generation can incur a charge even if the client loses the
    // connection, so never retry a create request on another account.
    let model = payload
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or(IMAGE_MODEL)
        .to_string();
    let context = crate::minimax_usage_context(
        &account,
        Some(model.clone()),
        "/minimax/v1/image_generation",
        crate::prompt_metrics_from_request_value(&raw),
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
    crate::record_minimax_request(&state, &context);
    let response = state
        .client
        .post(media_url(
            account.base_url.as_deref(),
            "/v1/image_generation",
        ))
        .header(
            "Authorization",
            format!("Bearer {}", account.api_key.trim()),
        )
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .json(&payload)
        .timeout(Duration::from_secs(180))
        .send()
        .await;

    let response = match response {
        Ok(response) => response,
        Err(err) => {
            let message = format!("MiniMax image request failed: {}", err);
            crate::record_minimax_error(&state, &context, &message);
            return upstream_failure(&message);
        }
    };
    let status = response.status();
    let bytes = match response.bytes().await {
        Ok(bytes) => bytes,
        Err(err) => {
            let message = format!("MiniMax image response body failed: {}", err);
            crate::record_minimax_error(&state, &context, &message);
            return upstream_failure(&message);
        }
    };
    if !status.is_success() {
        let message = format!(
            "MiniMax image generation returned {}: {}",
            status,
            String::from_utf8_lossy(&bytes)
        );
        crate::record_minimax_error(&state, &context, &message);
        return (
            status,
            [("Content-Type", "application/json")],
            crate::source::v1::response::upstream_error_to_openai(status, &bytes),
        )
            .into_response();
    }
    let value: Value = match serde_json::from_slice(&bytes) {
        Ok(value) => value,
        Err(err) => {
            let message = format!("invalid MiniMax image response: {}", err);
            crate::record_minimax_error(&state, &context, &message);
            return upstream_failure(&message);
        }
    };
    if let Some((status, message)) = minimax_api_error(&value) {
        crate::record_minimax_error(&state, &context, &message);
        return provider_error(status, &message);
    }
    let output = match translate_image_response(&value, &model) {
        Ok(output) => output,
        Err(err) => {
            crate::record_minimax_error(&state, &context, &err);
            return upstream_failure(&err);
        }
    };
    let usage = crate::usage_metrics_from_response_value(&value);
    crate::record_minimax_success(&state, &context, &usage);
    (
        StatusCode::OK,
        [("Content-Type", "application/json")],
        json_bytes(&output),
    )
        .into_response()
}

pub async fn video_generations(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if !crate::check_api_key(&state, &headers) {
        crate::release_api_key_budget_before_dispatch(&state);
        return authentication_error();
    }

    let raw: Value = match serde_json::from_slice(&body) {
        Ok(value) => value,
        Err(_) => {
            crate::release_api_key_budget_before_dispatch(&state);
            return invalid_request("Invalid request body");
        }
    };
    let payload = match build_video_payload(&raw) {
        Ok(payload) => payload,
        Err(err) => {
            crate::release_api_key_budget_before_dispatch(&state);
            return invalid_request(&err);
        }
    };
    let Some(account) = super::accounts::candidate_accounts(&state)
        .into_iter()
        .next()
    else {
        crate::release_api_key_budget_before_dispatch(&state);
        return no_accounts();
    };
    let model = payload
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or(VIDEO_MODEL_H3)
        .to_string();
    let api = video_api_for_model(&model).expect("validated MiniMax video model");
    let upstream_path = match api {
        VideoApi::H3V2 => "/v2/video_generation",
        VideoApi::LegacyV1 => "/v1/video_generation",
    };
    let context = crate::minimax_usage_context(
        &account,
        Some(model.clone()),
        &format!("/minimax{}", upstream_path),
        crate::prompt_metrics_from_request_value(&raw),
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
    crate::record_minimax_request(&state, &context);
    let response = state
        .client
        .post(media_url(account.base_url.as_deref(), upstream_path))
        .header(
            "Authorization",
            format!("Bearer {}", account.api_key.trim()),
        )
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .json(&payload)
        .timeout(Duration::from_secs(180))
        .send()
        .await;

    let response = match response {
        Ok(response) => response,
        Err(err) => {
            let message = format!("MiniMax video request failed: {}", err);
            crate::record_minimax_error(&state, &context, &message);
            return upstream_failure(&message);
        }
    };
    let status = response.status();
    let bytes = match response.bytes().await {
        Ok(bytes) => bytes,
        Err(err) => {
            let message = format!("MiniMax video response body failed: {}", err);
            crate::record_minimax_error(&state, &context, &message);
            return upstream_failure(&message);
        }
    };
    if !status.is_success() {
        let message = format!(
            "MiniMax video generation returned {}: {}",
            status,
            String::from_utf8_lossy(&bytes)
        );
        crate::record_minimax_error(&state, &context, &message);
        return (
            status,
            [("Content-Type", "application/json")],
            crate::source::v1::response::upstream_error_to_openai(status, &bytes),
        )
            .into_response();
    }
    let value: Value = match serde_json::from_slice(&bytes) {
        Ok(value) => value,
        Err(err) => {
            let message = format!("invalid MiniMax video response: {}", err);
            crate::record_minimax_error(&state, &context, &message);
            return upstream_failure(&message);
        }
    };
    if let Some((status, message)) = minimax_api_error(&value) {
        crate::record_minimax_error(&state, &context, &message);
        return provider_error(status, &message);
    }
    let task_id = match video_task_id(&value) {
        Some(task_id) => task_id,
        None => {
            let message = "MiniMax video response did not include task_id".to_string();
            crate::record_minimax_error(&state, &context, &message);
            return upstream_failure(&message);
        }
    };
    remember_video_task(&state, &task_id, &account, &model, api);
    let usage = crate::usage_metrics_from_response_value(&value);
    crate::record_minimax_success(&state, &context, &usage);
    let status_url = format!("/v1/videos/{}", task_id);
    let output = json!({
        "id": task_id.clone(),
        "object": "video",
        "status": "queued",
        "provider_status": "submitted",
        "model": model,
        "provider": "minimax",
        "task_id": task_id,
        "status_url": status_url,
        "created_at": chrono::Utc::now().timestamp(),
    });
    (
        StatusCode::OK,
        [("Content-Type", "application/json")],
        json_bytes(&output),
    )
        .into_response()
}

pub async fn video_status(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    task_id: &str,
) -> Response {
    if !crate::check_api_key(&state, &headers) {
        return authentication_error();
    }
    if !is_safe_task_id(task_id) {
        return invalid_request("invalid MiniMax video task id");
    }

    let binding = state
        .minimax_video_tasks
        .lock()
        .unwrap()
        .get(task_id)
        .cloned();
    let accounts = accounts_for_video_task(&state, binding.as_ref());
    if accounts.is_empty() {
        return not_found("MiniMax video task was not found for an enabled account");
    }

    let api_candidates = binding
        .as_ref()
        .map(|binding| vec![binding.api])
        .unwrap_or_else(|| vec![VideoApi::H3V2, VideoApi::LegacyV1]);
    let mut last_not_found = None;
    for account in accounts {
        for api in &api_candidates {
            let upstream_path = match api {
                VideoApi::H3V2 => format!("/v2/query/video_generation/{}", task_id),
                VideoApi::LegacyV1 => "/v1/query/video_generation".to_string(),
            };
            let mut request = state
                .client
                .get(media_url(account.base_url.as_deref(), &upstream_path))
                .header(
                    "Authorization",
                    format!("Bearer {}", account.api_key.trim()),
                )
                .header("Accept", "application/json")
                .timeout(Duration::from_secs(60));
            if *api == VideoApi::LegacyV1 {
                request = request.query(&[("task_id", task_id)]);
            }
            let response = match request.send().await {
                Ok(response) => response,
                Err(err) => {
                    return upstream_failure(&format!(
                        "MiniMax video status request failed: {}",
                        err
                    ))
                }
            };
            let status = response.status();
            let bytes = match response.bytes().await {
                Ok(bytes) => bytes,
                Err(err) => {
                    return upstream_failure(&format!("MiniMax video status body failed: {}", err));
                }
            };
            if !status.is_success() {
                // Bindings do not survive a gateway restart. A read-only status
                // lookup can safely try both APIs and enabled accounts then.
                if binding.is_none()
                    && matches!(
                        status,
                        StatusCode::NOT_FOUND | StatusCode::FORBIDDEN | StatusCode::BAD_REQUEST
                    )
                {
                    last_not_found = Some(bytes);
                    continue;
                }
                return (
                    status,
                    [("Content-Type", "application/json")],
                    crate::source::v1::response::upstream_error_to_openai(status, &bytes),
                )
                    .into_response();
            }
            let value: Value = match serde_json::from_slice(&bytes) {
                Ok(value) => value,
                Err(err) => {
                    return upstream_failure(&format!(
                        "invalid MiniMax video status response: {}",
                        err
                    ))
                }
            };
            if let Some((status, message)) = minimax_api_error(&value) {
                return provider_error(status, &message);
            }
            let output = match api {
                VideoApi::H3V2 => translate_video_status(&value, task_id, binding.as_ref()),
                VideoApi::LegacyV1 => {
                    let download_url = if legacy_video_succeeded(&value) {
                        match legacy_video_download_url(&state, &account, &value).await {
                            Ok(url) => Some(url),
                            Err(err) => return upstream_failure(&err),
                        }
                    } else {
                        None
                    };
                    translate_legacy_video_status(
                        &value,
                        task_id,
                        binding.as_ref(),
                        download_url.as_deref(),
                    )
                }
            };
            return match output {
                Ok(output) => (
                    StatusCode::OK,
                    [("Content-Type", "application/json")],
                    json_bytes(&output),
                )
                    .into_response(),
                Err(err) => upstream_failure(&err),
            };
        }
    }

    if let Some(bytes) = last_not_found {
        return (
            StatusCode::NOT_FOUND,
            [("Content-Type", "application/json")],
            crate::source::v1::response::upstream_error_to_openai(StatusCode::NOT_FOUND, &bytes),
        )
            .into_response();
    }
    not_found("MiniMax video task was not found")
}

pub(crate) fn build_image_payload(raw: &Value) -> Result<Value, String> {
    let model = request_model(raw)?;
    if !matches!(model.as_str(), IMAGE_MODEL | IMAGE_MODEL_LIVE) {
        return Err(format!(
            "MiniMax image generation requires '{}' or '{}'",
            IMAGE_MODEL, IMAGE_MODEL_LIVE
        ));
    }
    let prompt = required_string(raw, "prompt")?;
    if prompt.chars().count() > 1500 {
        return Err("MiniMax image prompt must not exceed 1500 characters".to_string());
    }

    let mut payload = json!({
        "model": model,
        "prompt": prompt,
    });
    let response_format = match raw
        .get("response_format")
        .and_then(Value::as_str)
        .unwrap_or("url")
    {
        "url" => "url",
        "b64_json" | "base64" => "base64",
        other => return Err(format!("unsupported image response_format '{}'", other)),
    };
    payload["response_format"] = Value::String(response_format.to_string());

    if let Some(n) = raw.get("n") {
        let n = n
            .as_u64()
            .filter(|n| (1..=9).contains(n))
            .ok_or_else(|| "MiniMax image n must be an integer from 1 to 9".to_string())?;
        payload["n"] = json!(n);
    }
    if let Some(seed) = raw.get("seed") {
        if !seed.is_i64() && !seed.is_u64() {
            return Err("MiniMax image seed must be an integer".to_string());
        }
        payload["seed"] = seed.clone();
    }
    if let Some(prompt_optimizer) = raw.get("prompt_optimizer") {
        if !prompt_optimizer.is_boolean() {
            return Err("MiniMax image prompt_optimizer must be a boolean".to_string());
        }
        payload["prompt_optimizer"] = prompt_optimizer.clone();
    }
    if let Some(subject_reference) = raw.get("subject_reference") {
        payload["subject_reference"] = validate_subject_reference(subject_reference)?;
    }

    if let Some(aspect_ratio) = raw.get("aspect_ratio").and_then(Value::as_str) {
        if !IMAGE_ASPECT_RATIOS.contains(&aspect_ratio) {
            return Err(format!(
                "unsupported MiniMax image aspect_ratio '{}'",
                aspect_ratio
            ));
        }
        payload["aspect_ratio"] = Value::String(aspect_ratio.to_string());
    } else if let Some(size) = raw.get("size").and_then(Value::as_str) {
        if size != "auto" {
            let (width, height) = parse_image_size(size)?;
            payload["width"] = json!(width);
            payload["height"] = json!(height);
        }
    } else if raw.get("width").is_some() || raw.get("height").is_some() {
        let width = raw
            .get("width")
            .and_then(Value::as_u64)
            .ok_or_else(|| "MiniMax image width must be an integer".to_string())?;
        let height = raw
            .get("height")
            .and_then(Value::as_u64)
            .ok_or_else(|| "MiniMax image height must be an integer".to_string())?;
        validate_image_dimensions(width, height)?;
        payload["width"] = json!(width);
        payload["height"] = json!(height);
    }
    Ok(payload)
}

pub(crate) fn build_video_payload(raw: &Value) -> Result<Value, String> {
    let model = request_model(raw)?;
    match video_api_for_model(&model) {
        Some(VideoApi::H3V2) => build_h3_video_payload(raw, &model),
        Some(VideoApi::LegacyV1) => build_legacy_video_payload(raw, &model),
        None => Err(format!("unsupported MiniMax video model '{}'", model)),
    }
}

fn build_h3_video_payload(raw: &Value, model: &str) -> Result<Value, String> {
    let (content, content_info) = build_video_content(raw)?;
    let duration = raw.get("duration").and_then(Value::as_u64).unwrap_or(5);
    let valid_duration = match model {
        VIDEO_MODEL_H3 => (4..=15).contains(&duration),
        VIDEO_MODEL_H3_MAX => (5..=15).contains(&duration),
        _ => false,
    };
    if !valid_duration {
        return Err(format!(
            "{} does not support duration {} seconds",
            model, duration
        ));
    }

    let resolution = raw
        .get("resolution")
        .and_then(Value::as_str)
        .unwrap_or("768P");
    let valid_resolution = match model {
        VIDEO_MODEL_H3 => matches!(resolution, "768P" | "2K"),
        VIDEO_MODEL_H3_MAX => matches!(resolution, "480P" | "768P"),
        _ => false,
    };
    if !valid_resolution {
        return Err(format!(
            "{} does not support resolution '{}'",
            model, resolution
        ));
    }

    if model == VIDEO_MODEL_H3_MAX && content_info.has_reference_media {
        return Err(format!(
            "{} does not support reference image, video, or audio input",
            VIDEO_MODEL_H3_MAX
        ));
    }

    let default_ratio = if content_info.has_media {
        "adaptive"
    } else {
        "16:9"
    };
    let mut ratio = raw
        .get("ratio")
        .and_then(Value::as_str)
        .unwrap_or(default_ratio);
    if !VIDEO_ASPECT_RATIOS.contains(&ratio) {
        return Err(format!("unsupported MiniMax video ratio '{}'", ratio));
    }
    if !content_info.has_media && ratio == "adaptive" {
        return Err("MiniMax text-to-video requires a concrete ratio".to_string());
    }

    // The upstream always derives image-to-video dimensions from the input
    // image. Sending a concrete ratio is accepted but silently ignored there,
    // so normalize it to the documented value before forwarding.
    if content_info.has_frame {
        ratio = "adaptive";
    }

    let mut payload = json!({
        "model": model,
        "content": content,
        "duration": duration,
        "resolution": resolution,
        "ratio": ratio,
    });
    attach_video_callback_url(&mut payload, raw)?;
    Ok(payload)
}

fn build_legacy_video_payload(raw: &Value, model: &str) -> Result<Value, String> {
    if raw.get("content").is_some() {
        return Err(
            "legacy MiniMax video models use prompt, input_image, first_frame_image, and last_frame_image instead of content"
                .to_string(),
        );
    }
    let prompt = required_string(raw, "prompt")?;
    if prompt.chars().count() > 2000 {
        return Err("MiniMax legacy video prompt must not exceed 2000 characters".to_string());
    }

    let first_frame_value = raw
        .get("input_image")
        .or_else(|| raw.get("image_url"))
        .or_else(|| raw.get("first_frame_image"));
    let first_frame_image = first_frame_value.and_then(media_input_url);
    if first_frame_value.is_some() && first_frame_image.is_none() {
        return Err("MiniMax legacy video first_frame_image must provide a URL".to_string());
    }
    let last_frame_value = raw.get("last_frame_image");
    let last_frame_image = last_frame_value.and_then(media_input_url);
    if last_frame_value.is_some() && last_frame_image.is_none() {
        return Err("MiniMax legacy video last_frame_image must provide a URL".to_string());
    }
    let has_frame = first_frame_image.is_some() || last_frame_image.is_some();
    let has_last_frame = last_frame_image.is_some();

    if legacy_video_requires_input_image(model) && first_frame_image.is_none() {
        return Err(format!(
            "{} requires input_image or first_frame_image",
            model
        ));
    }
    if legacy_video_is_text_only(model) && has_frame {
        return Err(format!("{} does not support image input", model));
    }
    if last_frame_image.is_some() && first_frame_image.is_none() {
        return Err("MiniMax legacy video last_frame_image requires first_frame_image".to_string());
    }

    let duration = raw.get("duration").and_then(Value::as_u64).unwrap_or(6);
    if !matches!(duration, 6 | 10) || (!legacy_video_supports_ten_seconds(model) && duration != 6) {
        return Err(format!(
            "{} does not support duration {} seconds",
            model, duration
        ));
    }
    let default_resolution = if legacy_video_is_high_resolution(model) {
        "768P"
    } else {
        "720P"
    };
    let resolution = raw
        .get("resolution")
        .and_then(Value::as_str)
        .unwrap_or(default_resolution);
    let valid_resolution =
        legacy_video_supports_resolution(model, has_frame, has_last_frame, resolution);
    if !valid_resolution {
        return Err(format!(
            "{} does not support resolution '{}'",
            model, resolution
        ));
    }
    if duration == 10 && resolution == "1080P" {
        return Err(format!(
            "{} only supports 768P output for 10-second video",
            model
        ));
    }

    let mut payload = json!({
        "model": model,
        "prompt": prompt,
        "duration": duration,
        "resolution": resolution,
    });
    if let Some(first_frame_image) = first_frame_image {
        payload["first_frame_image"] = Value::String(first_frame_image);
    }
    if let Some(last_frame_image) = last_frame_image {
        payload["last_frame_image"] = Value::String(last_frame_image);
    }
    for field in ["prompt_optimizer", "fast_pretreatment"] {
        if let Some(value) = raw.get(field) {
            if !value.is_boolean() {
                return Err(format!("MiniMax legacy video {} must be a boolean", field));
            }
            payload[field] = value.clone();
        }
    }
    if let Some(subject_reference) = raw.get("subject_reference") {
        if model != "S2V-01" {
            return Err(format!("{} does not support subject_reference", model));
        }
        payload["subject_reference"] = subject_reference.clone();
    } else if model == "S2V-01" {
        return Err("S2V-01 requires subject_reference".to_string());
    }
    attach_video_callback_url(&mut payload, raw)?;
    Ok(payload)
}

fn attach_video_callback_url(payload: &mut Value, raw: &Value) -> Result<(), String> {
    let Some(callback_url) = raw.get("callback_url") else {
        return Ok(());
    };
    let callback_url = callback_url
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "MiniMax video callback_url must be a non-empty URL".to_string())?;
    let callback_url = url::Url::parse(callback_url)
        .map_err(|_| "MiniMax video callback_url must be a valid URL".to_string())?;
    if !matches!(callback_url.scheme(), "http" | "https") {
        return Err("MiniMax video callback_url must use http or https".to_string());
    }
    payload["callback_url"] = Value::String(callback_url.to_string());
    Ok(())
}

pub(crate) fn translate_image_response(value: &Value, model: &str) -> Result<Value, String> {
    let data = value
        .get("data")
        .and_then(Value::as_object)
        .ok_or_else(|| "MiniMax image response did not include data".to_string())?;
    let mut images = Vec::new();
    if let Some(urls) = data.get("image_urls").and_then(Value::as_array) {
        images.extend(
            urls.iter()
                .filter_map(Value::as_str)
                .map(|url| json!({ "url": url })),
        );
    }
    if let Some(images_base64) = data.get("image_base64").and_then(Value::as_array) {
        images.extend(
            images_base64
                .iter()
                .filter_map(Value::as_str)
                .map(|b64_json| json!({ "b64_json": b64_json })),
        );
    }
    if images.is_empty() {
        return Err("MiniMax image response did not include generated images".to_string());
    }
    let mut output = json!({
        "created": chrono::Utc::now().timestamp(),
        "data": images,
        "model": model,
        "provider": "minimax",
    });
    if let Some(metadata) = value.get("metadata") {
        output["metadata"] = metadata.clone();
    }
    Ok(output)
}

pub(crate) fn translate_video_status(
    value: &Value,
    task_id: &str,
    binding: Option<&VideoTaskBinding>,
) -> Result<Value, String> {
    let task = value.get("task").cloned().unwrap_or_else(|| value.clone());
    let task_object = task
        .as_object()
        .ok_or_else(|| "MiniMax video status response did not include task".to_string())?;
    let provider_status = task_object
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("queued");
    let status = match provider_status {
        "succeeded" | "completed" => "completed",
        "failed" => "failed",
        "cancelled" | "canceled" => "cancelled",
        _ => "in_progress",
    };
    let id = task_object
        .get("id")
        .or_else(|| task_object.get("task_id"))
        .and_then(Value::as_str)
        .unwrap_or(task_id);
    let mut output = json!({
        "id": id,
        "object": "video",
        "status": status,
        "provider_status": provider_status,
        "provider": "minimax",
        "model": task_object
            .get("model")
            .and_then(Value::as_str)
            .or_else(|| binding.map(|binding| binding.model.as_str())),
        "task": task.clone(),
    });
    for key in ["duration", "resolution", "ratio", "usage"] {
        if let Some(value) = task_object.get(key) {
            output[key] = value.clone();
        }
    }
    if let Some(url) = task_object
        .get("content")
        .and_then(|content| content.get("url"))
        .and_then(Value::as_str)
    {
        output["url"] = Value::String(url.to_string());
    }
    Ok(output)
}

fn translate_legacy_video_status(
    value: &Value,
    task_id: &str,
    binding: Option<&VideoTaskBinding>,
    download_url: Option<&str>,
) -> Result<Value, String> {
    let provider_status = value
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("Queueing");
    let status = match provider_status.to_ascii_lowercase().as_str() {
        "success" | "succeeded" | "completed" => "completed",
        "fail" | "failed" => "failed",
        "cancelled" | "canceled" => "cancelled",
        _ => "in_progress",
    };
    let id = value_id(value.get("task_id")).unwrap_or_else(|| task_id.to_string());
    let mut output = json!({
        "id": id,
        "object": "video",
        "status": status,
        "provider_status": provider_status,
        "provider": "minimax",
        "model": binding.map(|binding| binding.model.as_str()),
        "task": value,
    });
    for key in ["file_id", "video_width", "video_height", "usage"] {
        if let Some(value) = value.get(key) {
            output[key] = value.clone();
        }
    }
    if let Some(download_url) = download_url {
        output["url"] = Value::String(download_url.to_string());
    }
    Ok(output)
}

fn legacy_video_succeeded(value: &Value) -> bool {
    value
        .get("status")
        .and_then(Value::as_str)
        .is_some_and(|status| {
            matches!(
                status.to_ascii_lowercase().as_str(),
                "success" | "succeeded" | "completed"
            )
        })
}

async fn legacy_video_download_url(
    state: &crate::AppState,
    account: &MiniMaxAccount,
    task: &Value,
) -> Result<String, String> {
    let file_id = value_id(task.get("file_id"))
        .ok_or_else(|| "MiniMax legacy video succeeded without file_id".to_string())?;
    let response = state
        .client
        .get(media_url(account.base_url.as_deref(), "/v1/files/retrieve"))
        .query(&[("file_id", file_id.as_str())])
        .header(
            "Authorization",
            format!("Bearer {}", account.api_key.trim()),
        )
        .header("Accept", "application/json")
        .timeout(Duration::from_secs(60))
        .send()
        .await
        .map_err(|err| format!("MiniMax legacy video file request failed: {}", err))?;
    let status = response.status();
    let bytes = response
        .bytes()
        .await
        .map_err(|err| format!("MiniMax legacy video file body failed: {}", err))?;
    if !status.is_success() {
        return Err(format!(
            "MiniMax legacy video file request returned {}: {}",
            status,
            String::from_utf8_lossy(&bytes)
        ));
    }
    let value: Value = serde_json::from_slice(&bytes)
        .map_err(|err| format!("invalid MiniMax legacy video file response: {}", err))?;
    if let Some((_status, message)) = minimax_api_error(&value) {
        return Err(message);
    }
    value
        .get("file")
        .and_then(|file| file.get("download_url"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|url| !url.is_empty())
        .map(str::to_string)
        .ok_or_else(|| {
            "MiniMax legacy video file response did not include download_url".to_string()
        })
}

fn value_id(value: Option<&Value>) -> Option<String> {
    value.and_then(|value| {
        value
            .as_str()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .or_else(|| value.as_u64().map(|value| value.to_string()))
            .or_else(|| value.as_i64().map(|value| value.to_string()))
    })
}

#[derive(Default)]
struct VideoContentInfo {
    has_media: bool,
    has_frame: bool,
    has_reference_media: bool,
}

fn build_video_content(raw: &Value) -> Result<(Vec<Value>, VideoContentInfo), String> {
    let mut content = if let Some(content) = raw.get("content").and_then(Value::as_array) {
        content.clone()
    } else {
        vec![json!({ "type": "text", "text": required_string(raw, "prompt")? })]
    };
    if raw.get("content").is_none() {
        append_media_item(
            &mut content,
            raw.get("input_image").or_else(|| raw.get("image_url")),
            "image_url",
            "first_frame",
        )?;
        append_media_item(
            &mut content,
            raw.get("first_frame_image"),
            "image_url",
            "first_frame",
        )?;
        append_media_item(
            &mut content,
            raw.get("last_frame_image"),
            "image_url",
            "last_frame",
        )?;
        append_media_items(
            &mut content,
            raw.get("reference_images"),
            "image_url",
            "reference_image",
        )?;
        append_media_items(
            &mut content,
            raw.get("reference_videos"),
            "video_url",
            "reference_video",
        )?;
        append_media_items(
            &mut content,
            raw.get("reference_audio"),
            "audio_url",
            "reference_audio",
        )?;
    }
    if content.is_empty() {
        return Err("MiniMax video content must not be empty".to_string());
    }

    // An image item without a role is a first-frame image in MiniMax's V2 API.
    // Materializing that default makes downstream validation unambiguous.
    for item in &mut content {
        if item.get("type").and_then(Value::as_str) == Some("image_url")
            && item.get("role").is_none()
        {
            if let Some(item) = item.as_object_mut() {
                item.insert("role".to_string(), Value::String("first_frame".to_string()));
            }
        }
    }

    let mut has_text = false;
    let mut first_frames = 0usize;
    let mut last_frames = 0usize;
    let mut reference_images = 0usize;
    let mut reference_videos = 0usize;
    let mut reference_audio = 0usize;
    let mut info = VideoContentInfo::default();
    for item in &content {
        let kind = item
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| "MiniMax video content items need a type".to_string())?;
        match kind {
            "text" => {
                let text = item.get("text").and_then(Value::as_str).unwrap_or("");
                if !text.trim().is_empty() {
                    if text.chars().count() > 7000 {
                        return Err(
                            "MiniMax video prompt must not exceed 7000 characters".to_string()
                        );
                    }
                    has_text = true;
                }
            }
            "image_url" => {
                require_media_url(item, "image_url")?;
                info.has_media = true;
                match item.get("role").and_then(Value::as_str) {
                    Some("first_frame") => {
                        first_frames += 1;
                        info.has_frame = true;
                    }
                    Some("last_frame") => {
                        last_frames += 1;
                        info.has_frame = true;
                    }
                    Some("reference_image") => {
                        reference_images += 1;
                        info.has_reference_media = true;
                    }
                    Some(role) => {
                        return Err(format!(
                            "MiniMax image_url content does not support role '{}'",
                            role
                        ));
                    }
                    None => unreachable!("image_url items are normalized with a first_frame role"),
                }
            }
            "video_url" => {
                require_media_url(item, "video_url")?;
                info.has_media = true;
                match item.get("role").and_then(Value::as_str) {
                    Some("reference_video") => {
                        reference_videos += 1;
                        info.has_reference_media = true;
                    }
                    Some(role) => {
                        return Err(format!(
                            "MiniMax video_url content requires role 'reference_video', not '{}'",
                            role
                        ));
                    }
                    None => {
                        return Err(
                            "MiniMax video_url content requires role 'reference_video'".to_string()
                        );
                    }
                }
            }
            "audio_url" => {
                require_media_url(item, "audio_url")?;
                info.has_media = true;
                match item.get("role").and_then(Value::as_str) {
                    Some("reference_audio") => {
                        reference_audio += 1;
                        info.has_reference_media = true;
                    }
                    Some(role) => {
                        return Err(format!(
                            "MiniMax audio_url content requires role 'reference_audio', not '{}'",
                            role
                        ));
                    }
                    None => {
                        return Err(
                            "MiniMax audio_url content requires role 'reference_audio'".to_string()
                        );
                    }
                }
            }
            _ => return Err(format!("unsupported MiniMax video content type '{}'", kind)),
        }
    }
    if !has_text {
        return Err("MiniMax video content requires a non-empty text prompt".to_string());
    }
    if info.has_frame && info.has_reference_media {
        return Err("MiniMax video cannot mix first/last frames with reference media".to_string());
    }
    if first_frames > 1 || last_frames > 1 {
        return Err(
            "MiniMax video supports at most one first frame and one last frame".to_string(),
        );
    }
    if last_frames > 0 && first_frames == 0 {
        return Err("MiniMax video last_frame requires a first_frame".to_string());
    }
    if reference_images > 9 || reference_videos > 3 || reference_audio > 3 {
        return Err(
            "MiniMax video supports at most 9 reference images, 3 videos, and 3 audio files"
                .to_string(),
        );
    }
    Ok((content, info))
}

fn require_media_url(item: &Value, key: &str) -> Result<(), String> {
    let url = item
        .get(key)
        .and_then(|value| value.get("url").or(Some(value)))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty());
    if url.is_some() {
        Ok(())
    } else {
        Err(format!("MiniMax {} content requires a non-empty url", key))
    }
}

fn validate_subject_reference(value: &Value) -> Result<Value, String> {
    let references = value
        .as_array()
        .ok_or_else(|| "MiniMax image subject_reference must be an array".to_string())?;
    if references.is_empty() {
        return Err("MiniMax image subject_reference must not be empty".to_string());
    }
    for reference in references {
        let subject_type = reference
            .get("type")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty());
        if subject_type != Some("character") {
            return Err(
                "MiniMax image subject_reference currently requires type 'character'".to_string(),
            );
        }
        let image_file = reference
            .get("image_file")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty());
        if image_file.is_none() {
            return Err("MiniMax image subject_reference requires image_file".to_string());
        }
    }
    Ok(value.clone())
}

fn append_media_item(
    content: &mut Vec<Value>,
    value: Option<&Value>,
    kind: &str,
    role: &str,
) -> Result<(), String> {
    let Some(value) = value else {
        return Ok(());
    };
    let url = media_input_url(value)
        .ok_or_else(|| format!("MiniMax video {} must provide a URL", role))?;
    let mut item = serde_json::Map::new();
    item.insert("type".to_string(), Value::String(kind.to_string()));
    item.insert("role".to_string(), Value::String(role.to_string()));
    item.insert(kind.to_string(), json!({ "url": url }));
    content.push(Value::Object(item));
    Ok(())
}

fn append_media_items(
    content: &mut Vec<Value>,
    value: Option<&Value>,
    kind: &str,
    role: &str,
) -> Result<(), String> {
    let Some(values) = value else {
        return Ok(());
    };
    let values = values
        .as_array()
        .ok_or_else(|| format!("MiniMax video {} must be an array", role))?;
    for value in values {
        append_media_item(content, Some(value), kind, role)?;
    }
    Ok(())
}

fn media_input_url(value: &Value) -> Option<String> {
    value
        .as_str()
        .map(str::to_string)
        .or_else(|| value.get("url").and_then(Value::as_str).map(str::to_string))
        .or_else(|| {
            value
                .get("image_url")
                .and_then(|image_url| image_url.get("url").or(Some(image_url)))
                .and_then(Value::as_str)
                .map(str::to_string)
        })
}

fn parse_image_size(value: &str) -> Result<(u64, u64), String> {
    let (width, height) = value
        .split_once('x')
        .ok_or_else(|| "MiniMax image size must use WIDTHxHEIGHT".to_string())?;
    let width = width
        .parse::<u64>()
        .map_err(|_| "MiniMax image size width must be an integer".to_string())?;
    let height = height
        .parse::<u64>()
        .map_err(|_| "MiniMax image size height must be an integer".to_string())?;
    validate_image_dimensions(width, height)?;
    Ok((width, height))
}

fn validate_image_dimensions(width: u64, height: u64) -> Result<(), String> {
    if !(512..=2048).contains(&width)
        || !(512..=2048).contains(&height)
        || width % 8 != 0
        || height % 8 != 0
    {
        return Err(
            "MiniMax image dimensions must be 512-2048 pixels and divisible by 8".to_string(),
        );
    }
    Ok(())
}

fn request_model(raw: &Value) -> Result<String, String> {
    let model = required_string(raw, "model")?;
    Ok(model
        .strip_prefix("min:")
        .unwrap_or(model.as_str())
        .to_string())
}

fn video_api_for_model(model: &str) -> Option<VideoApi> {
    if matches!(model, VIDEO_MODEL_H3 | VIDEO_MODEL_H3_MAX) {
        Some(VideoApi::H3V2)
    } else if LEGACY_VIDEO_MODELS.contains(&model) {
        Some(VideoApi::LegacyV1)
    } else {
        None
    }
}

fn legacy_video_is_text_only(model: &str) -> bool {
    matches!(model, "T2V-01-Director" | "T2V-01")
}

fn legacy_video_requires_input_image(model: &str) -> bool {
    matches!(
        model,
        "MiniMax-Hailuo-2.3-Fast" | "I2V-01-Director" | "I2V-01-live" | "I2V-01"
    )
}

fn legacy_video_supports_ten_seconds(model: &str) -> bool {
    matches!(
        model,
        "MiniMax-Hailuo-2.3" | "MiniMax-Hailuo-2.3-Fast" | "MiniMax-Hailuo-02"
    )
}

fn legacy_video_is_high_resolution(model: &str) -> bool {
    matches!(
        model,
        "MiniMax-Hailuo-2.3" | "MiniMax-Hailuo-2.3-Fast" | "MiniMax-Hailuo-02"
    )
}

fn legacy_video_supports_resolution(
    model: &str,
    has_frame: bool,
    has_last_frame: bool,
    resolution: &str,
) -> bool {
    match model {
        // Hailuo 2.3 models only expose 768P and 1080P. The provider rejects
        // 512P even though older generic Hailuo documentation listed it.
        "MiniMax-Hailuo-2.3" | "MiniMax-Hailuo-2.3-Fast" => {
            matches!(resolution, "768P" | "1080P")
        }
        // Hailuo-02 accepts 512P only for first-frame image-to-video. Text
        // generation and first/last-frame generation require 768P or 1080P.
        "MiniMax-Hailuo-02" if has_frame && !has_last_frame => {
            matches!(resolution, "512P" | "768P" | "1080P")
        }
        "MiniMax-Hailuo-02" => matches!(resolution, "768P" | "1080P"),
        _ => resolution == "720P",
    }
}

fn required_string(raw: &Value, key: &str) -> Result<String, String> {
    raw.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .ok_or_else(|| format!("{} is required", key))
}

fn media_url(base_url: Option<&str>, suffix: &str) -> String {
    let mut base = normalize_base_url(base_url);
    for suffix in [
        "/v1/responses",
        "/v1/chat/completions",
        "/chat/completions",
        "/v1",
    ] {
        if base.ends_with(suffix) {
            base.truncate(base.len() - suffix.len());
            break;
        }
    }
    format!("{}{}", base.trim_end_matches('/'), suffix)
}

fn minimax_api_error(value: &Value) -> Option<(StatusCode, String)> {
    let base = value.get("base_resp")?;
    let code = base.get("status_code").and_then(Value::as_i64).unwrap_or(0);
    if code == 0 {
        return None;
    }
    let message = base
        .get("status_msg")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .unwrap_or("MiniMax request failed")
        .to_string();
    let status = match code {
        1002 | 2056 => StatusCode::TOO_MANY_REQUESTS,
        1004 | 2049 => StatusCode::UNAUTHORIZED,
        1008 => StatusCode::PAYMENT_REQUIRED,
        1026 => StatusCode::UNPROCESSABLE_ENTITY,
        2013 => StatusCode::BAD_REQUEST,
        _ => StatusCode::BAD_GATEWAY,
    };
    Some((status, format!("MiniMax error {}: {}", code, message)))
}

fn video_task_id(value: &Value) -> Option<String> {
    value_id(
        value
            .get("task_id")
            .or_else(|| value.get("task").and_then(|task| task.get("id"))),
    )
}

fn remember_video_task(
    state: &crate::AppState,
    task_id: &str,
    account: &MiniMaxAccount,
    model: &str,
    api: VideoApi,
) {
    let mut tasks = state.minimax_video_tasks.lock().unwrap();
    tasks.retain(|_, task| task.created_at.elapsed() < VIDEO_TASK_BINDING_TTL);
    tasks.insert(
        task_id.to_string(),
        VideoTaskBinding {
            account_key: crate::minimax_stats_key(account),
            model: model.to_string(),
            api,
            created_at: Instant::now(),
        },
    );
}

fn accounts_for_video_task(
    state: &crate::AppState,
    binding: Option<&VideoTaskBinding>,
) -> Vec<MiniMaxAccount> {
    let accounts = state.minimax_accounts.lock().unwrap();
    match binding {
        Some(binding) => accounts
            .iter()
            .find(|account| {
                account.enabled && crate::minimax_stats_key(account) == binding.account_key
            })
            .cloned()
            .into_iter()
            .collect(),
        None => accounts
            .iter()
            .filter(|account| account.enabled)
            .cloned()
            .collect(),
    }
}

fn is_safe_task_id(value: &str) -> bool {
    !value.is_empty()
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_'))
}

fn json_bytes(value: &Value) -> Bytes {
    Bytes::from(serde_json::to_vec(value).unwrap_or_default())
}

fn authentication_error() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [("Content-Type", "application/json")],
        crate::source::v1::response::openai_error_body(
            "Invalid proxy API key",
            "authentication_error",
            Some("invalid_api_key"),
        ),
    )
        .into_response()
}

fn invalid_request(message: &str) -> Response {
    provider_error(StatusCode::BAD_REQUEST, message)
}

fn no_accounts() -> Response {
    provider_error(
        StatusCode::SERVICE_UNAVAILABLE,
        "No MiniMax accounts configured",
    )
}

fn not_found(message: &str) -> Response {
    provider_error(StatusCode::NOT_FOUND, message)
}

fn upstream_failure(message: &str) -> Response {
    provider_error(StatusCode::BAD_GATEWAY, message)
}

fn provider_error(status: StatusCode, message: &str) -> Response {
    let error_type = if status == StatusCode::UNAUTHORIZED {
        "authentication_error"
    } else if status == StatusCode::TOO_MANY_REQUESTS {
        "rate_limit_error"
    } else if status == StatusCode::PAYMENT_REQUIRED {
        "billing_error"
    } else if status.is_client_error() {
        "invalid_request_error"
    } else {
        "server_error"
    };
    (
        status,
        [("Content-Type", "application/json")],
        crate::source::v1::response::openai_error_body(message, error_type, None),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_payload_translates_openai_fields() {
        let payload = build_image_payload(&json!({
            "model": "min:image-01",
            "prompt": "A bright yellow mug",
            "size": "1024x1024",
            "n": 2,
            "response_format": "b64_json",
            "seed": 42,
            "prompt_optimizer": true,
        }))
        .unwrap();

        assert_eq!(payload["model"], IMAGE_MODEL);
        assert_eq!(payload["response_format"], "base64");
        assert_eq!(payload["width"], 1024);
        assert_eq!(payload["height"], 1024);
        assert_eq!(payload["n"], 2);
        assert_eq!(payload["seed"], 42);
    }

    #[test]
    fn image_response_translates_base64_images() {
        let response = translate_image_response(
            &json!({
                "data": { "image_base64": ["YWJj"] },
                "metadata": { "success_count": 1 }
            }),
            IMAGE_MODEL_LIVE,
        )
        .unwrap();

        assert_eq!(response["data"][0]["b64_json"], "YWJj");
        assert_eq!(response["metadata"]["success_count"], 1);
        assert_eq!(response["model"], IMAGE_MODEL_LIVE);
    }

    #[test]
    fn video_payload_supports_image_to_video() {
        let payload = build_video_payload(&json!({
            "model": "min:MiniMax-H3",
            "prompt": "Make the mug slowly rotate",
            "input_image": "https://example.com/mug.png",
            "duration": 5,
            "resolution": "768P"
        }))
        .unwrap();

        assert_eq!(payload["model"], VIDEO_MODEL_H3);
        assert_eq!(payload["ratio"], "adaptive");
        assert_eq!(payload["content"][1]["type"], "image_url");
        assert_eq!(payload["content"][1]["role"], "first_frame");
    }

    #[test]
    fn video_payload_rejects_invalid_h3_max_resolution() {
        let err = build_video_payload(&json!({
            "model": VIDEO_MODEL_H3_MAX,
            "prompt": "A mug rotates",
            "duration": 5,
            "resolution": "2K",
            "ratio": "16:9"
        }))
        .unwrap_err();

        assert!(err.contains("does not support resolution"));
    }

    #[test]
    fn video_payload_rejects_h3_max_reference_media() {
        let err = build_video_payload(&json!({
            "model": VIDEO_MODEL_H3_MAX,
            "prompt": "A mug rotates",
            "reference_images": ["https://example.com/mug.png"],
            "duration": 5,
            "resolution": "768P"
        }))
        .unwrap_err();

        assert!(err.contains("does not support reference"));
    }

    #[test]
    fn video_payload_normalizes_direct_image_content_and_preserves_callback() {
        let payload = build_video_payload(&json!({
            "model": VIDEO_MODEL_H3,
            "content": [
                { "type": "text", "text": "Make the mug rotate" },
                { "type": "image_url", "image_url": { "url": "https://example.com/mug.png" } }
            ],
            "duration": 5,
            "resolution": "768P",
            "ratio": "16:9",
            "callback_url": "https://example.com/minimax-callback"
        }))
        .unwrap();

        assert_eq!(payload["content"][1]["role"], "first_frame");
        assert_eq!(payload["ratio"], "adaptive");
        assert_eq!(
            payload["callback_url"],
            "https://example.com/minimax-callback"
        );
    }

    #[test]
    fn legacy_video_payload_uses_v1_hailuo_shape() {
        let payload = build_video_payload(&json!({
            "model": "min:MiniMax-Hailuo-2.3",
            "prompt": "A blue circle slowly rotates",
            "duration": 6,
            "resolution": "768P",
            "prompt_optimizer": true
        }))
        .unwrap();

        assert_eq!(payload["model"], "MiniMax-Hailuo-2.3");
        assert_eq!(payload["prompt"], "A blue circle slowly rotates");
        assert_eq!(payload["duration"], 6);
        assert_eq!(payload["resolution"], "768P");
        assert!(payload.get("content").is_none());
    }

    #[test]
    fn legacy_hailuo_23_rejects_unsupported_512p() {
        let err = build_video_payload(&json!({
            "model": "min:MiniMax-Hailuo-2.3",
            "prompt": "A blue circle slowly rotates",
            "duration": 6,
            "resolution": "512P"
        }))
        .unwrap_err();

        assert!(err.contains("does not support resolution"));
    }

    #[test]
    fn legacy_hailuo_02_allows_512p_only_for_first_frame_video() {
        let image_to_video = build_video_payload(&json!({
            "model": "min:MiniMax-Hailuo-02",
            "prompt": "A blue circle slowly rotates",
            "input_image": "https://example.com/circle.png",
            "duration": 6,
            "resolution": "512P"
        }));
        assert!(image_to_video.is_ok());

        let text_to_video = build_video_payload(&json!({
            "model": "min:MiniMax-Hailuo-02",
            "prompt": "A blue circle slowly rotates",
            "duration": 6,
            "resolution": "512P"
        }))
        .unwrap_err();
        assert!(text_to_video.contains("does not support resolution"));
    }

    #[test]
    fn legacy_video_status_includes_file_download_url() {
        let binding = VideoTaskBinding {
            account_key: "minimax:account_id:mock".to_string(),
            model: "MiniMax-Hailuo-2.3".to_string(),
            api: VideoApi::LegacyV1,
            created_at: Instant::now(),
        };
        let output = translate_legacy_video_status(
            &json!({
                "task_id": "task-legacy-123",
                "status": "Success",
                "file_id": "file-123",
                "video_width": 768,
                "video_height": 768
            }),
            "task-legacy-123",
            Some(&binding),
            Some("https://example.com/generated.mp4"),
        )
        .unwrap();

        assert_eq!(output["status"], "completed");
        assert_eq!(output["model"], "MiniMax-Hailuo-2.3");
        assert_eq!(output["file_id"], "file-123");
        assert_eq!(output["url"], "https://example.com/generated.mp4");
    }

    #[test]
    fn video_status_includes_download_url() {
        let output = translate_video_status(
            &json!({
                "task": {
                    "id": "task-123",
                    "model": VIDEO_MODEL_H3,
                    "status": "succeeded",
                    "content": { "url": "https://example.com/video.mp4" },
                    "duration": 5,
                    "resolution": "768P"
                }
            }),
            "task-123",
            None,
        )
        .unwrap();

        assert_eq!(output["status"], "completed");
        assert_eq!(output["url"], "https://example.com/video.mp4");
    }

    #[test]
    fn media_url_strips_text_api_suffixes() {
        assert_eq!(
            media_url(Some("https://api.minimax.io/v1"), "/v1/image_generation"),
            "https://api.minimax.io/v1/image_generation"
        );
        assert_eq!(
            media_url(
                Some("https://proxy.example/v1/chat/completions"),
                "/v2/video_generation"
            ),
            "https://proxy.example/v2/video_generation"
        );
    }

    #[test]
    fn minimax_usage_limit_maps_to_openai_rate_limit_error() {
        let (status, message) = minimax_api_error(&json!({
            "base_resp": {
                "status_code": 2056,
                "status_msg": "Token Plan usage limit reached"
            }
        }))
        .unwrap();

        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
        assert!(message.contains("Token Plan usage limit reached"));
    }
}
