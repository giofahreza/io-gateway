use axum::http::{HeaderMap, Method, Uri};
use bytes::Bytes;
use serde_json::Value;

pub mod claude;
pub mod codex;
pub mod openapi;
pub mod v1;

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum TargetModel {
    Codex,
    Antigravity,
    Gemini,
    Qwen,
    DeepSeek,
    Grok,
    MiniMax,
    Copilot,
    Claude,
    Glm,
    Compatible,
    Custom,
    CodexModels,
    UnifiedV1Models,
}

#[derive(Debug, Clone, Copy)]
pub enum ResponseMode {
    Passthrough,
    SseToJson,
}

#[derive(Debug, Clone, Copy)]
pub struct RouteError {
    pub status: axum::http::StatusCode,
    pub message: &'static str,
}

#[derive(Debug, Clone)]
pub struct RoutedRequest {
    pub target: TargetModel,
    pub upstream_path: String,
    pub upstream_query: Option<String>,
    pub upstream_body: Bytes,
    pub response_mode: ResponseMode,
}

pub fn route_request(
    path: &str,
    uri: &Uri,
    method: &Method,
    headers: &HeaderMap,
    body: Bytes,
) -> Result<RoutedRequest, RouteError> {
    let trimmed = path.trim_start_matches('/');
    if trimmed == "v1" || trimmed.starts_with("v1/") {
        return v1::route_to_target(path, uri, method, headers, body);
    }
    if trimmed == "codex" || trimmed.starts_with("codex/") {
        return codex::route_to_target(path, uri, method, headers, body);
    }
    if trimmed == "claude" || trimmed.starts_with("claude/") {
        return claude::route_to_target(path, uri, method, headers, body);
    }
    // Backward compatibility: old behavior treated bare routes as OpenAI-compatible.
    v1::route_to_target(path, uri, method, headers, body)
}

pub(crate) fn decode_stringified_responses_input(body: Bytes) -> Bytes {
    let Ok(mut value) = serde_json::from_slice::<Value>(&body) else {
        return body;
    };
    let Some(object) = value.as_object_mut() else {
        return body;
    };
    let mut changed = false;

    if let Some(input) = object.get("input").and_then(|value| value.as_str()) {
        if let Some(decoded) = parse_stringified_responses_input(input) {
            object.insert("input".to_string(), decoded);
            changed = true;
        }
    }

    if let Some(input) = object.get_mut("input") {
        changed |= strip_codex_internal_responses_input_metadata(input);
    }

    if changed {
        serde_json::to_vec(&value).map(Bytes::from).unwrap_or(body)
    } else {
        body
    }
}

fn parse_stringified_responses_input(input: &str) -> Option<Value> {
    let mut current = input.trim().to_string();
    for _ in 0..4 {
        if current.starts_with('[') {
            let decoded = serde_json::from_str::<Value>(&current).ok()?;
            return responses_input_array(decoded);
        }

        if current.starts_with('{') {
            let decoded = serde_json::from_str::<Value>(&current).ok()?;
            if looks_like_responses_input_item(&decoded) {
                return Some(Value::Array(vec![decoded]));
            }
            if let Some(inner) = decoded.get("input") {
                if let Some(array) = responses_input_array(inner.clone()) {
                    return Some(array);
                }
                if let Some(input) = inner.as_str() {
                    current = input.trim().to_string();
                    continue;
                }
            }
            return None;
        }

        if current.starts_with('"') {
            let decoded = serde_json::from_str::<String>(&current).ok()?;
            let next = decoded.trim();
            if next == current {
                return None;
            }
            current = next.to_string();
            continue;
        }

        return None;
    }
    None
}

fn responses_input_array(decoded: Value) -> Option<Value> {
    let items = decoded.as_array()?;
    let all_items_have_response_markers = items.iter().all(has_responses_input_item_markers);
    let has_known_response_item =
        items.is_empty() || items.iter().any(looks_like_responses_input_item);
    if all_items_have_response_markers && has_known_response_item {
        Some(decoded)
    } else {
        None
    }
}

fn has_responses_input_item_markers(item: &Value) -> bool {
    let Some(object) = item.as_object() else {
        return false;
    };
    object
        .get("role")
        .and_then(|value| value.as_str())
        .is_some()
        || object
            .get("type")
            .and_then(|value| value.as_str())
            .is_some()
}

fn looks_like_responses_input_item(item: &Value) -> bool {
    let Some(object) = item.as_object() else {
        return false;
    };
    if object
        .get("role")
        .and_then(|value| value.as_str())
        .is_some()
    {
        return true;
    }

    matches!(
        object.get("type").and_then(|value| value.as_str()),
        Some(
            "message"
                | "function_call"
                | "function_call_output"
                | "tool_result"
                | "reasoning"
                | "compaction"
                | "computer_call"
                | "computer_call_output"
                | "file_search_call"
                | "web_search_call"
                | "image_generation_call"
        )
    )
}

fn strip_codex_internal_responses_input_metadata(value: &mut Value) -> bool {
    match value {
        Value::Array(items) => items.iter_mut().fold(false, |changed, item| {
            strip_codex_internal_responses_input_metadata(item) || changed
        }),
        Value::Object(object) => {
            let changed = object
                .remove("internal_chat_message_metadata_passthrough")
                .is_some();

            object.values_mut().fold(changed, |changed, item| {
                strip_codex_internal_responses_input_metadata(item) || changed
            })
        }
        _ => false,
    }
}

pub(crate) fn stringify_responses_input_arguments(body: Bytes) -> Bytes {
    let Ok(mut value) = serde_json::from_slice::<Value>(&body) else {
        return body;
    };
    let Some(input) = value
        .as_object_mut()
        .and_then(|object| object.get_mut("input"))
    else {
        return body;
    };

    if stringify_arguments(input) {
        serde_json::to_vec(&value).map(Bytes::from).unwrap_or(body)
    } else {
        body
    }
}

fn stringify_arguments(value: &mut Value) -> bool {
    match value {
        Value::Array(items) => items
            .iter_mut()
            .fold(false, |changed, item| stringify_arguments(item) || changed),
        Value::Object(object) => {
            let mut changed = false;
            if let Some(arguments) = object.get_mut("arguments") {
                if !arguments.is_string() {
                    let encoded =
                        serde_json::to_string(arguments).unwrap_or_else(|_| arguments.to_string());
                    *arguments = Value::String(encoded);
                    changed = true;
                }
            }

            object.values_mut().fold(changed, |changed, item| {
                stringify_arguments(item) || changed
            })
        }
        _ => false,
    }
}

pub(crate) fn objectify_responses_input_arguments(value: &mut Value) -> bool {
    match value {
        Value::Array(items) => items.iter_mut().fold(false, |changed, item| {
            objectify_responses_input_arguments(item) || changed
        }),
        Value::Object(object) => {
            let mut changed = false;
            let should_objectify_arguments =
                object.get("type").and_then(|value| value.as_str()) == Some("tool_search_call");
            if should_objectify_arguments {
                if let Some(arguments) = object.get_mut("arguments") {
                    if let Some(text) = arguments.as_str() {
                        let parsed = serde_json::from_str::<Value>(text)
                            .ok()
                            .filter(|parsed| parsed.is_object())
                            .unwrap_or_else(|| serde_json::json!({}));
                        *arguments = parsed;
                        changed = true;
                    }
                }
            }

            object.values_mut().fold(changed, |changed, item| {
                objectify_responses_input_arguments(item) || changed
            })
        }
        _ => false,
    }
}

pub(crate) fn strip_prefix_path(path: &str, prefix: &str) -> String {
    let trimmed = path.trim_start_matches('/');
    if trimmed == prefix {
        return String::new();
    }
    if let Some(rest) = trimmed.strip_prefix(&format!("{}/", prefix)) {
        return rest.to_string();
    }
    trimmed.to_string()
}

pub(crate) fn wants_stream(headers: &HeaderMap, body: &Bytes) -> bool {
    if let Some(accept) = headers.get("accept").and_then(|v| v.to_str().ok()) {
        if accept.to_ascii_lowercase().contains("text/event-stream") {
            return true;
        }
    }
    if let Some(ct) = headers.get("content-type").and_then(|v| v.to_str().ok()) {
        if !ct.to_ascii_lowercase().contains("application/json") {
            return false;
        }
    } else {
        return false;
    }
    let value: serde_json::Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(_) => return false,
    };
    matches!(value.get("stream"), Some(serde_json::Value::Bool(true)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_stringified_responses_input_array() {
        let transcript = serde_json::json!([
            {
                "role": "user",
                "content": [{ "type": "input_text", "text": "hello" }]
            }
        ]);
        let body = serde_json::json!({
            "model": "gpt-5.4",
            "input": transcript.to_string()
        })
        .to_string();

        let decoded = decode_stringified_responses_input(Bytes::from(body));
        let value: Value = serde_json::from_slice(&decoded).unwrap();
        assert_eq!(value["input"], transcript);
    }

    #[test]
    fn decodes_double_stringified_responses_input_array() {
        let transcript = serde_json::json!([
            {
                "role": "user",
                "content": [{ "type": "input_text", "text": "hello" }]
            }
        ]);
        let body = serde_json::json!({
            "model": "gpt-5.4",
            "input": serde_json::to_string(&transcript.to_string()).unwrap()
        })
        .to_string();

        let decoded = decode_stringified_responses_input(Bytes::from(body));
        let value: Value = serde_json::from_slice(&decoded).unwrap();
        assert_eq!(value["input"], transcript);
    }

    #[test]
    fn decodes_stringified_request_body_input_array() {
        let transcript = serde_json::json!([
            {
                "role": "user",
                "content": [{ "type": "input_text", "text": "hello" }]
            }
        ]);
        let body = serde_json::json!({
            "model": "gpt-5.4",
            "input": serde_json::json!({ "input": transcript }).to_string()
        })
        .to_string();

        let decoded = decode_stringified_responses_input(Bytes::from(body));
        let value: Value = serde_json::from_slice(&decoded).unwrap();
        assert_eq!(value["input"], transcript);
    }

    #[test]
    fn decodes_stringified_single_response_item_as_array() {
        let item = serde_json::json!({
            "role": "user",
            "content": [{ "type": "input_text", "text": "hello" }]
        });
        let body = serde_json::json!({
            "model": "gpt-5.4",
            "input": item.to_string()
        })
        .to_string();

        let decoded = decode_stringified_responses_input(Bytes::from(body));
        let value: Value = serde_json::from_slice(&decoded).unwrap();
        assert_eq!(value["input"], serde_json::json!([item]));
    }

    #[test]
    fn strips_decoded_codex_internal_metadata_and_preserves_object_arguments() {
        let transcript = serde_json::json!([
            {
                "type": "tool_search_call",
                "id": "ts_1",
                "call_id": "call_1",
                "arguments": { "query": "Atlassian MCP" },
                "internal_chat_message_metadata_passthrough": {
                    "turn_id": "turn_1"
                }
            },
            {
                "role": "user",
                "content": [{ "type": "input_text", "text": "continue" }]
            }
        ]);
        let body = serde_json::json!({
            "model": "MiniMax-M3",
            "input": transcript.to_string()
        })
        .to_string();

        let decoded = decode_stringified_responses_input(Bytes::from(body));
        let value: Value = serde_json::from_slice(&decoded).unwrap();
        assert_eq!(
            value["input"][0]["arguments"],
            serde_json::json!({ "query": "Atlassian MCP" })
        );
        assert!(value["input"][0]
            .get("internal_chat_message_metadata_passthrough")
            .is_none());
    }

    #[test]
    fn strips_real_input_array_metadata_without_requiring_stringified_input() {
        let body = serde_json::json!({
            "model": "MiniMax-M3",
            "input": [
                {
                    "type": "function_call",
                    "name": "tool",
                    "arguments": { "path": "/tmp" },
                    "internal_chat_message_metadata_passthrough": { "turn_id": "turn_1" }
                }
            ]
        })
        .to_string();

        let decoded = decode_stringified_responses_input(Bytes::from(body));
        let value: Value = serde_json::from_slice(&decoded).unwrap();
        assert_eq!(
            value["input"][0]["arguments"],
            serde_json::json!({ "path": "/tmp" })
        );
        assert!(value["input"][0]
            .get("internal_chat_message_metadata_passthrough")
            .is_none());
    }

    #[test]
    fn stringifies_responses_input_arguments_for_string_argument_providers() {
        let body = serde_json::json!({
            "model": "MiniMax-M3",
            "input": [
                {
                    "type": "function_call",
                    "name": "tool",
                    "arguments": { "path": "/tmp" }
                }
            ]
        })
        .to_string();

        let decoded = stringify_responses_input_arguments(Bytes::from(body));
        let value: Value = serde_json::from_slice(&decoded).unwrap();
        assert_eq!(
            value["input"][0]["arguments"],
            serde_json::json!("{\"path\":\"/tmp\"}")
        );
    }

    #[test]
    fn objectifies_tool_search_argument_strings_for_codex_inputs() {
        let mut input = serde_json::json!([
            {
                "type": "function_call",
                "name": "empty_args",
                "arguments": ""
            },
            {
                "type": "tool_search_call",
                "arguments": "\"plain text\""
            },
            {
                "type": "tool_search_call",
                "name": "array_args",
                "arguments": "[\"not\", \"an\", \"object\"]"
            },
            {
                "type": "tool_search_call",
                "name": "object_args",
                "arguments": "{\"path\":\"/tmp\"}"
            }
        ]);

        assert!(objectify_responses_input_arguments(&mut input));
        assert_eq!(input[0]["arguments"], "");
        assert_eq!(input[1]["arguments"], serde_json::json!({}));
        assert_eq!(input[2]["arguments"], serde_json::json!({}));
        assert_eq!(input[3]["arguments"], serde_json::json!({"path": "/tmp"}));
    }

    #[test]
    fn leaves_plain_prompt_strings_untouched() {
        let body = Bytes::from_static(br#"{"model":"gpt-5.4","input":"[not json"}"#);
        let decoded = decode_stringified_responses_input(body.clone());
        assert_eq!(decoded, body);
    }

    #[test]
    fn leaves_json_array_prompts_that_are_not_response_items_untouched() {
        let body = Bytes::from_static(br#"{"model":"gpt-5.4","input":"[\"literal\"]"}"#);
        let decoded = decode_stringified_responses_input(body.clone());
        assert_eq!(decoded, body);
    }

    #[test]
    fn decodes_mixed_transcript_with_unknown_typed_item() {
        let transcript = serde_json::json!([
            {
                "role": "user",
                "content": [{ "type": "input_text", "text": "hello" }]
            },
            {
                "type": "future_response_item",
                "payload": {}
            }
        ]);
        let body = serde_json::json!({
            "model": "gpt-5.4",
            "input": transcript.to_string()
        })
        .to_string();

        let decoded = decode_stringified_responses_input(Bytes::from(body));
        let value: Value = serde_json::from_slice(&decoded).unwrap();
        assert_eq!(value["input"], transcript);
    }

    #[test]
    fn leaves_unknown_typed_json_array_prompt_untouched() {
        let body = Bytes::from_static(br#"{"model":"gpt-5.4","input":"[{\"type\":\"todo\"}]"}"#);
        let decoded = decode_stringified_responses_input(body.clone());
        assert_eq!(decoded, body);
    }

    #[test]
    fn leaves_plain_json_object_prompt_untouched() {
        let body = Bytes::from_static(br#"{"model":"gpt-5.4","input":"{\"todo\":true}"}"#);
        let decoded = decode_stringified_responses_input(body.clone());
        assert_eq!(decoded, body);
    }
}
