use axum::http::{HeaderMap, Method, Uri};
use bytes::Bytes;
use serde_json::{Map, Value};

use crate::source::{RouteError, RoutedRequest, TargetModel};

pub mod catalog;
pub mod codex;
pub mod response;
pub mod route;

pub fn route_to_target(
    path: &str,
    uri: &Uri,
    method: &Method,
    headers: &HeaderMap,
    body: Bytes,
) -> Result<RoutedRequest, RouteError> {
    let upstream_path = route::resolve(path, method)?;
    if upstream_path == "models" && *method == Method::GET {
        return Ok(RoutedRequest {
            target: TargetModel::CodexModels,
            upstream_path,
            upstream_query: None,
            upstream_body: body,
            response_mode: crate::source::codex::response::resolve_mode("models"),
        });
    }

    if matches!(upstream_path.as_str(), "responses" | "responses/compact")
        && *method == Method::POST
    {
        let body = crate::source::decode_stringified_responses_input(body);
        crate::source::v1::provider::validate_provider_prefix_in_body(&body)?;
        let target = crate::source::v1::provider::target_from_request_body(&body)
            .unwrap_or(TargetModel::Codex);
        if target != TargetModel::Codex {
            if matches!(
                target,
                TargetModel::MiniMax | TargetModel::Copilot | TargetModel::Custom
            ) {
                let body = if target == TargetModel::MiniMax {
                    crate::source::stringify_responses_input_arguments(body)
                } else {
                    body
                };
                return Ok(crate::source::v1::provider::convert(
                    target,
                    upstream_path,
                    uri,
                    body,
                ));
            }
            let body = normalize_codex_provider_body(body);
            return Ok(crate::source::v1::provider::convert(
                target,
                upstream_path,
                uri,
                body,
            ));
        }
        return Ok(codex::convert(upstream_path, uri, method, headers, body));
    }

    Ok(codex::convert(upstream_path, uri, method, headers, body))
}

fn normalize_codex_provider_body(body: Bytes) -> Bytes {
    let Ok(Value::Object(input)) = serde_json::from_slice::<Value>(&body) else {
        return body;
    };

    let mut output = Map::new();
    copy_json_field(&input, &mut output, "model");
    copy_json_field(&input, &mut output, "previous_model");
    copy_json_field(&input, &mut output, "instructions");
    copy_json_field(&input, &mut output, "input");
    copy_json_field(&input, &mut output, "messages");
    if let Some(tools) = normalize_provider_tools(input.get("tools")) {
        output.insert("tools".to_string(), Value::Array(tools));
    }
    copy_json_field(&input, &mut output, "tool_choice");
    copy_json_field(&input, &mut output, "parallel_tool_calls");
    copy_json_field(&input, &mut output, "max_output_tokens");
    copy_json_field(&input, &mut output, "temperature");
    copy_json_field(&input, &mut output, "top_p");
    copy_json_field(&input, &mut output, "stop");
    copy_json_field(&input, &mut output, "stream");
    copy_json_field(&input, &mut output, "reasoning");
    copy_json_field(&input, &mut output, "reasoning_effort");
    copy_json_field(&input, &mut output, "text");
    copy_json_field(&input, &mut output, "response_format");

    serde_json::to_vec(&Value::Object(output))
        .map(Bytes::from)
        .unwrap_or(body)
}

fn copy_json_field(input: &Map<String, Value>, output: &mut Map<String, Value>, name: &str) {
    if let Some(value) = input.get(name) {
        output.insert(name.to_string(), value.clone());
    }
}

fn normalize_provider_tools(tools: Option<&Value>) -> Option<Vec<Value>> {
    let tools = tools?.as_array()?;
    let mut normalized = Vec::new();
    for tool in tools {
        if tool.get("type").and_then(|value| value.as_str()) != Some("function") {
            continue;
        }

        if let Some(function) = tool.get("function") {
            normalized.push(serde_json::json!({
                "type": "function",
                "function": function
            }));
            continue;
        }

        let Some(name) = tool.get("name").and_then(|value| value.as_str()) else {
            continue;
        };
        let mut function = Map::new();
        function.insert("name".to_string(), Value::String(name.to_string()));
        copy_json_field_object(tool, &mut function, "description");
        copy_json_field_object(tool, &mut function, "parameters");
        copy_json_field_object(tool, &mut function, "strict");
        normalized.push(serde_json::json!({
            "type": "function",
            "function": Value::Object(function)
        }));
    }

    if normalized.is_empty() {
        None
    } else {
        Some(normalized)
    }
}

fn copy_json_field_object(input: &Value, output: &mut Map<String, Value>, name: &str) {
    if let Some(value) = input.get(name) {
        output.insert(name.to_string(), value.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{HeaderMap, Method, Uri};

    #[test]
    fn codex_responses_can_route_deepseek_models() {
        let uri: Uri = "/codex/responses".parse().unwrap();
        let routed = route_to_target(
            "/codex/responses",
            &uri,
            &Method::POST,
            &HeaderMap::new(),
            Bytes::from_static(
                br#"{"model":"deepseek-v4-pro","input":"hi","store":true,"include":["x"]}"#,
            ),
        )
        .unwrap();

        assert_eq!(routed.target, TargetModel::DeepSeek);
        assert_eq!(routed.upstream_path, "responses");
        let body: Value = serde_json::from_slice(&routed.upstream_body).unwrap();
        assert_eq!(body["model"], "deepseek-v4-pro");
        assert_eq!(body["input"], "hi");
        assert!(body.get("store").is_none());
        assert!(body.get("include").is_none());
    }

    #[test]
    fn codex_compaction_route_preserves_previous_model_request() {
        let uri: Uri = "/codex/responses/compact".parse().unwrap();
        let routed = route_to_target(
            "/codex/responses/compact",
            &uri,
            &Method::POST,
            &HeaderMap::new(),
            Bytes::from_static(
                br#"{"model":"gpt-5.4","previous_model":"gpt-5.3-codex","input":[]}"#,
            ),
        )
        .unwrap();

        assert_eq!(routed.target, TargetModel::Codex);
        assert_eq!(routed.upstream_path, "responses/compact");
        let body: Value = serde_json::from_slice(&routed.upstream_body).unwrap();
        assert_eq!(body["model"], "gpt-5.4");
        assert_eq!(body["previous_model"], "gpt-5.3-codex");
    }

    #[test]
    fn codex_responses_preserves_gemini_input_array() {
        let uri: Uri = "/codex/responses".parse().unwrap();
        let routed = route_to_target(
            "/codex/responses",
            &uri,
            &Method::POST,
            &HeaderMap::new(),
            Bytes::from_static(
                br#"{"model":"gemini-2.5-pro","input":[{"role":"user","content":[{"type":"input_text","text":"hello"}]}]}"#,
            ),
        )
        .unwrap();

        assert_eq!(routed.target, TargetModel::Gemini);
        let body: Value = serde_json::from_slice(&routed.upstream_body).unwrap();
        assert!(body.get("messages").is_none());
        assert_eq!(body["input"][0]["role"], "user");
        assert_eq!(body["input"][0]["content"][0]["text"], "hello");
    }

    #[test]
    fn codex_responses_uses_provider_prefix_and_strips_it_for_upstream() {
        let uri: Uri = "/codex/responses".parse().unwrap();
        let routed = route_to_target(
            "/codex/responses",
            &uri,
            &Method::POST,
            &HeaderMap::new(),
            Bytes::from_static(
                br#"{"model":"agw:gemini-2.5-pro","input":[{"role":"user","content":[{"type":"input_text","text":"hello"}]}]}"#,
            ),
        )
        .unwrap();

        assert_eq!(routed.target, TargetModel::Antigravity);
        let body: Value = serde_json::from_slice(&routed.upstream_body).unwrap();
        assert_eq!(body["model"], "gemini-2.5-pro");
        assert!(body.get("messages").is_none());
        assert_eq!(body["input"][0]["content"][0]["text"], "hello");
    }

    #[test]
    fn codex_responses_decodes_stringified_input_when_switching_to_provider() {
        let uri: Uri = "/codex/responses".parse().unwrap();
        let transcript = serde_json::json!([
            {
                "role": "user",
                "content": [{ "type": "input_text", "text": "hello from codex session" }]
            }
        ]);
        let request = serde_json::json!({
            "model": "cop:gpt-5.1",
            "input": transcript.to_string(),
            "store": true,
            "include": ["reasoning.encrypted_content"]
        });

        let routed = route_to_target(
            "/codex/responses",
            &uri,
            &Method::POST,
            &HeaderMap::new(),
            Bytes::from(request.to_string()),
        )
        .unwrap();

        assert_eq!(routed.target, TargetModel::Copilot);
        let body: Value = serde_json::from_slice(&routed.upstream_body).unwrap();
        assert_eq!(body["model"], "gpt-5.1");
        assert_eq!(body["input"], transcript);
        assert_eq!(body["store"], true);
        assert_eq!(body["include"][0], "reasoning.encrypted_content");
    }

    #[test]
    fn codex_responses_decodes_stringified_input_for_codex_upstream() {
        let uri: Uri = "/codex/responses".parse().unwrap();
        let transcript = serde_json::json!([
            {
                "role": "user",
                "content": [{ "type": "input_text", "text": "continue" }]
            }
        ]);
        let request = serde_json::json!({
            "model": "cod:gpt-5.4",
            "input": transcript.to_string()
        });

        let routed = route_to_target(
            "/codex/responses",
            &uri,
            &Method::POST,
            &HeaderMap::new(),
            Bytes::from(request.to_string()),
        )
        .unwrap();

        assert_eq!(routed.target, TargetModel::Codex);
        let body: Value = serde_json::from_slice(&routed.upstream_body).unwrap();
        assert_eq!(body["model"], "gpt-5.4");
        assert_eq!(body["input"], transcript);
    }

    #[test]
    fn codex_responses_objectifies_arguments_for_codex_upstream() {
        let uri: Uri = "/codex/responses".parse().unwrap();
        let request = serde_json::json!({
            "model": "cod:gpt-5.4",
            "input": [
                {
                    "type": "tool_search_call",
                    "call_id": "call_1",
                    "arguments": "{\"cmd\":\"pwd\"}"
                }
            ]
        });

        let routed = route_to_target(
            "/codex/responses",
            &uri,
            &Method::POST,
            &HeaderMap::new(),
            Bytes::from(request.to_string()),
        )
        .unwrap();

        assert_eq!(routed.target, TargetModel::Codex);
        let body: Value = serde_json::from_slice(&routed.upstream_body).unwrap();
        assert_eq!(body["model"], "gpt-5.4");
        assert_eq!(
            body["input"][0]["arguments"],
            serde_json::json!({"cmd": "pwd"})
        );
    }

    #[test]
    fn codex_responses_preserves_function_call_argument_strings_for_codex_upstream() {
        let uri: Uri = "/codex/responses".parse().unwrap();
        let request = serde_json::json!({
            "model": "cod:gpt-5.4",
            "input": [
                {
                    "type": "function_call",
                    "name": "exec_command",
                    "call_id": "call_1",
                    "arguments": "{\"cmd\":\"pwd\"}"
                }
            ]
        });

        let routed = route_to_target(
            "/codex/responses",
            &uri,
            &Method::POST,
            &HeaderMap::new(),
            Bytes::from(request.to_string()),
        )
        .unwrap();

        assert_eq!(routed.target, TargetModel::Codex);
        let body: Value = serde_json::from_slice(&routed.upstream_body).unwrap();
        assert_eq!(body["model"], "gpt-5.4");
        assert_eq!(body["input"][0]["arguments"], "{\"cmd\":\"pwd\"}");
    }

    #[test]
    fn codex_responses_passes_minimax_body_through() {
        let uri: Uri = "/codex/responses".parse().unwrap();
        let routed = route_to_target(
            "/codex/responses",
            &uri,
            &Method::POST,
            &HeaderMap::new(),
            Bytes::from_static(
                br#"{"model":"min:MiniMax-M3","input":"hello","store":true,"include":["reasoning.encrypted_content"],"tools":[{"type":"function","name":"shell","parameters":{"type":"object"}}]}"#,
            ),
        )
        .unwrap();

        assert_eq!(routed.target, TargetModel::MiniMax);
        let body: Value = serde_json::from_slice(&routed.upstream_body).unwrap();
        assert_eq!(body["model"], "MiniMax-M3");
        assert_eq!(body["input"], "hello");
        assert_eq!(body["store"], true);
        assert_eq!(body["include"][0], "reasoning.encrypted_content");
        assert_eq!(body["tools"][0]["name"], "shell");
    }

    #[test]
    fn codex_responses_stringifies_arguments_for_minimax_upstream() {
        let uri: Uri = "/codex/responses".parse().unwrap();
        let request = serde_json::json!({
            "model": "min:MiniMax-M3",
            "input": [
                {
                    "type": "tool_search_call",
                    "call_id": "call_1",
                    "arguments": { "query": "Atlassian MCP" }
                }
            ]
        });

        let routed = route_to_target(
            "/codex/responses",
            &uri,
            &Method::POST,
            &HeaderMap::new(),
            Bytes::from(request.to_string()),
        )
        .unwrap();

        assert_eq!(routed.target, TargetModel::MiniMax);
        let body: Value = serde_json::from_slice(&routed.upstream_body).unwrap();
        assert_eq!(body["model"], "MiniMax-M3");
        assert_eq!(
            body["input"][0]["arguments"],
            serde_json::json!("{\"query\":\"Atlassian MCP\"}")
        );
    }

    #[test]
    fn codex_responses_passes_copilot_body_through() {
        let uri: Uri = "/codex/responses".parse().unwrap();
        let routed = route_to_target(
            "/codex/responses",
            &uri,
            &Method::POST,
            &HeaderMap::new(),
            Bytes::from_static(
                br#"{"model":"cop:gpt-5.1","input":"hello","store":true,"include":["reasoning.encrypted_content"],"tools":[{"type":"function","name":"shell","parameters":{"type":"object"}}]}"#,
            ),
        )
        .unwrap();

        assert_eq!(routed.target, TargetModel::Copilot);
        let body: Value = serde_json::from_slice(&routed.upstream_body).unwrap();
        assert_eq!(body["model"], "gpt-5.1");
        assert_eq!(body["input"], "hello");
        assert_eq!(body["store"], true);
        assert_eq!(body["include"][0], "reasoning.encrypted_content");
        assert_eq!(body["tools"][0]["name"], "shell");
    }

    #[test]
    fn codex_responses_passes_custom_model_body_through() {
        let uri: Uri = "/codex/responses".parse().unwrap();
        let routed = route_to_target(
            "/codex/responses",
            &uri,
            &Method::POST,
            &HeaderMap::new(),
            Bytes::from_static(
                br#"{"model":"ctm:workhorse","input":"hello","store":true,"include":["reasoning.encrypted_content"],"tools":[{"type":"function","name":"trace","parameters":{"type":"object"}}]}"#,
            ),
        )
        .unwrap();

        assert_eq!(routed.target, TargetModel::Custom);
        let body: Value = serde_json::from_slice(&routed.upstream_body).unwrap();
        assert_eq!(body["model"], "workhorse");
        assert_eq!(body["input"], "hello");
        assert_eq!(body["store"], true);
        assert_eq!(body["include"][0], "reasoning.encrypted_content");
        assert_eq!(body["tools"][0]["name"], "trace");
    }

    #[test]
    fn codex_responses_strips_codex_provider_prefix_for_upstream() {
        let uri: Uri = "/codex/responses".parse().unwrap();
        let routed = route_to_target(
            "/codex/responses",
            &uri,
            &Method::POST,
            &HeaderMap::new(),
            Bytes::from_static(br#"{"model":"cod:gpt-5.4","input":"hello"}"#),
        )
        .unwrap();

        assert_eq!(routed.target, TargetModel::Codex);
        let body: Value = serde_json::from_slice(&routed.upstream_body).unwrap();
        assert_eq!(body["model"], "gpt-5.4");
        assert_eq!(body["input"][0]["role"], "user");
        assert_eq!(body["input"][0]["content"][0]["text"], "hello");
    }

    #[test]
    fn codex_spark_responses_strips_unsupported_image_generation_tool() {
        let uri: Uri = "/codex/responses".parse().unwrap();
        let routed = route_to_target(
            "/codex/responses",
            &uri,
            &Method::POST,
            &HeaderMap::new(),
            Bytes::from_static(
                br#"{"model":"cod:gpt-5.3-codex-spark","input":"qa only","tools":[{"type":"image_generation"},{"type":"function","name":"shell","parameters":{"type":"object"}}]}"#,
            ),
        )
        .unwrap();

        assert_eq!(routed.target, TargetModel::Codex);
        let body: Value = serde_json::from_slice(&routed.upstream_body).unwrap();
        assert_eq!(body["model"], "gpt-5.3-codex-spark");
        assert_eq!(body["input"][0]["role"], "user");
        assert_eq!(body["input"][0]["content"][0]["text"], "qa only");
        let tools = body["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["type"], "function");
        assert_eq!(tools[0]["name"], "shell");
    }

    #[test]
    fn codex_responses_rejects_invalid_provider_prefix_syntax() {
        let uri: Uri = "/codex/responses".parse().unwrap();
        let err = route_to_target(
            "/codex/responses",
            &uri,
            &Method::POST,
            &HeaderMap::new(),
            Bytes::from_static(br#"{"model":"agw: gemini-2.5-pro","input":"hello"}"#),
        )
        .unwrap_err();

        assert_eq!(err.status, axum::http::StatusCode::BAD_REQUEST);
    }

    #[test]
    fn codex_responses_converts_responses_tools_for_deepseek() {
        let uri: Uri = "/codex/responses".parse().unwrap();
        let routed = route_to_target(
            "/codex/responses",
            &uri,
            &Method::POST,
            &HeaderMap::new(),
            Bytes::from_static(
                br#"{"model":"deepseek-v4-pro","input":"hi","tools":[{"type":"function","name":"shell","description":"run a command","parameters":{"type":"object"},"strict":true}]}"#,
            ),
        )
        .unwrap();

        assert_eq!(routed.target, TargetModel::DeepSeek);
        let body: Value = serde_json::from_slice(&routed.upstream_body).unwrap();
        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["function"]["name"], "shell");
        assert_eq!(body["tools"][0]["function"]["description"], "run a command");
        assert_eq!(body["tools"][0]["function"]["parameters"]["type"], "object");
        assert_eq!(body["tools"][0]["function"]["strict"], true);
    }

    #[test]
    fn codex_models_routes_to_codex_catalog() {
        let uri: Uri = "/codex/models".parse().unwrap();
        let routed = route_to_target(
            "/codex/models",
            &uri,
            &Method::GET,
            &HeaderMap::new(),
            Bytes::new(),
        )
        .unwrap();

        assert_eq!(routed.target, TargetModel::CodexModels);
        assert_eq!(routed.upstream_path, "models");
    }
}

/// Lists full Codex runtime metadata, or an explicit compact discovery view.
#[utoipa::path(
    get,
    path = "/codex/models",
    params(
        ("view" = Option<String>, Query, description = "Use discovery for IDs, labels and visibility only; omitted or full preserves the runtime catalog")
    ),
    security(("bearer_auth" = [])),
    responses(
        (
            status = 200,
            description = "Raw Codex model list",
            body = crate::source::openapi::UpstreamModelListResponse
        ),
        (
            status = 401,
            description = "Missing or invalid proxy API key",
            body = String
        )
    )
)]
#[allow(dead_code)]
pub(crate) fn models_doc() {}

/// Sends a raw Codex responses request through the gateway.
#[utoipa::path(
    post,
    path = "/codex/responses",
    request_body(
        content = crate::source::openapi::CodexResponsesCreateRequest,
        content_type = "application/json",
        description = "Codex responses payload"
    ),
    security(("bearer_auth" = [])),
    responses(
        (
            status = 200,
            description = "Upstream Codex response",
            body = crate::source::openapi::ResponseSummary
        ),
        (
            status = 401,
            description = "Missing or invalid proxy API key",
            body = String
        )
    )
)]
#[allow(dead_code)]
pub(crate) fn responses_doc() {}
