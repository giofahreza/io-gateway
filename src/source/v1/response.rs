use axum::http::{HeaderMap, Method, StatusCode};
use bytes::Bytes;

use crate::source::{wants_stream, ResponseMode};

pub fn resolve_mode(
    upstream_path: &str,
    method: &Method,
    headers: &HeaderMap,
    body: &Bytes,
) -> ResponseMode {
    let is_responses_post = upstream_path == "responses" && *method == Method::POST;
    if is_responses_post && !wants_stream(headers, body) {
        ResponseMode::SseToJson
    } else {
        ResponseMode::Passthrough
    }
}

pub fn sse_to_response_json(body: &Bytes) -> Bytes {
    let mut response_obj: Option<serde_json::Value> = None;
    let mut output_text = String::new();

    for event in crate::sse_response_events(body) {
        let Some(v) = crate::sse_event_json_value(event) else {
            continue;
        };
        if let Some(t) = v.get("type").and_then(|v| v.as_str()) {
            if t == "response.output_text.delta" {
                if let Some(delta) = v.get("delta").and_then(|v| v.as_str()) {
                    output_text.push_str(delta);
                }
            } else if t == "response.completed" {
                if let Some(r) = v.get("response") {
                    response_obj = Some(r.clone());
                }
            } else if t == "response.created" && response_obj.is_none() {
                if let Some(r) = v.get("response") {
                    response_obj = Some(r.clone());
                }
            }
        }
    }

    let mut resp = response_obj.unwrap_or_else(|| {
        serde_json::json!({
            "object": "response",
            "output": []
        })
    });
    if let serde_json::Value::Object(map) = &mut resp {
        map.insert(
            "output_text".to_string(),
            serde_json::Value::String(output_text.clone()),
        );
        let output_is_empty = map
            .get("output")
            .and_then(|v| v.as_array())
            .map(|a| a.is_empty())
            .unwrap_or(true);
        if output_is_empty {
            map.insert(
                "output".to_string(),
                serde_json::json!([{
                    "type": "message",
                    "id": "msg_compat",
                    "role": "assistant",
                    "content": [{"type":"output_text","text": output_text}]
                }]),
            );
        }
    }
    Bytes::from(serde_json::to_vec(&resp).unwrap_or_default())
}

#[cfg(test)]
mod sse_accounting_tests {
    use super::sse_to_response_json;
    use bytes::Bytes;

    #[test]
    fn sse_accounting_conversion_preserves_no_space_response_and_usage() {
        let body = Bytes::from_static(
            b"data:{\"type\":\"response.output_text.delta\",\"delta\":\"ok\"}\n\ndata:{\"type\":\"response.completed\",\"response\":{\"id\":\"resp_test\",\"usage\":{\"input_tokens\":17}}}\n\n",
        );
        let response: serde_json::Value =
            serde_json::from_slice(&sse_to_response_json(&body)).unwrap();
        assert_eq!(response["id"], "resp_test");
        assert_eq!(response["usage"]["input_tokens"], 17);
        assert_eq!(response["output_text"], "ok");
    }
}

pub fn models_list_to_openai_json(body: &Bytes) -> Result<Bytes, String> {
    let value: serde_json::Value =
        serde_json::from_slice(body).map_err(|_| "failed to parse upstream models response")?;
    let models = value
        .get("models")
        .and_then(|v| v.as_array())
        .ok_or("upstream models response missing models list")?;

    let mut data = Vec::with_capacity(models.len());
    for m in models {
        if let Some(mapped) = map_model_for_openai(m) {
            data.push(mapped);
        }
    }

    let out = serde_json::json!({
        "object": "list",
        "data": data
    });
    serde_json::to_vec(&out)
        .map(Bytes::from)
        .map_err(|_| "failed to serialize models response".to_string())
}

pub fn model_retrieve_to_openai_json(body: &Bytes, model_id: &str) -> Result<Bytes, String> {
    let value: serde_json::Value =
        serde_json::from_slice(body).map_err(|_| "failed to parse upstream models response")?;
    let models = value
        .get("models")
        .and_then(|v| v.as_array())
        .ok_or("upstream models response missing models list")?;

    let selected = models
        .iter()
        .filter_map(map_model_for_openai)
        .find(|m| m.get("id").and_then(|x| x.as_str()) == Some(model_id));

    let out = match selected {
        Some(model) => model,
        None => {
            return Err(format!("The model '{}' does not exist", model_id));
        }
    };

    serde_json::to_vec(&out)
        .map(Bytes::from)
        .map_err(|_| "failed to serialize model response".to_string())
}

pub fn openai_error_body(message: &str, kind: &str, code: Option<&str>) -> Bytes {
    let body = serde_json::json!({
        "error": {
            "message": message,
            "type": kind,
            "param": serde_json::Value::Null,
            "code": code
        }
    });
    Bytes::from(serde_json::to_vec(&body).unwrap_or_default())
}

pub fn upstream_error_to_openai(status: StatusCode, body: &Bytes) -> Bytes {
    let parsed: serde_json::Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(_) => {
            return openai_error_body(
                "Upstream request failed",
                status_to_error_type(status),
                status_to_error_code(status),
            );
        }
    };

    if parsed.get("error").is_some() {
        return Bytes::from(serde_json::to_vec(&parsed).unwrap_or_default());
    }

    let message = parsed
        .get("detail")
        .or_else(|| parsed.get("message"))
        .and_then(|v| v.as_str())
        .unwrap_or("Upstream request failed");

    openai_error_body(
        message,
        status_to_error_type(status),
        status_to_error_code(status),
    )
}

fn map_model_for_openai(model: &serde_json::Value) -> Option<serde_json::Value> {
    let id = model
        .get("id")
        .or_else(|| model.get("slug"))
        .and_then(|v| v.as_str())?;
    Some(serde_json::json!({
        "id": id,
        "object": "model",
        "created": 0,
        "owned_by": "openai"
    }))
}

pub(crate) fn status_to_error_type(status: StatusCode) -> &'static str {
    if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
        "authentication_error"
    } else if status == StatusCode::TOO_MANY_REQUESTS {
        "rate_limit_error"
    } else if status.is_client_error() {
        "invalid_request_error"
    } else {
        "server_error"
    }
}

pub(crate) fn status_to_error_code(status: StatusCode) -> Option<&'static str> {
    if status == StatusCode::NOT_FOUND {
        Some("not_found")
    } else if status == StatusCode::TOO_MANY_REQUESTS {
        Some("rate_limit_exceeded")
    } else if status == StatusCode::UNAUTHORIZED {
        Some("invalid_api_key")
    } else {
        None
    }
}
