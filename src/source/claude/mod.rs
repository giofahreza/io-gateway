use axum::http::{HeaderMap, Method, Uri};
use bytes::Bytes;

use crate::source::{ResponseMode, RouteError, RoutedRequest, TargetModel};

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
    if (*method == Method::GET || *method == Method::HEAD)
        && (upstream_path == "models" || upstream_path == "v1/models")
    {
        return Ok(crate::source::v1::provider::convert(
            crate::source::TargetModel::UnifiedV1Models,
            "models".to_string(),
            uri,
            body,
        ));
    }

    if is_messages_path(&upstream_path) && *method == Method::POST {
        if crate::source::v1::provider::target_from_request_body(&body) == Some(TargetModel::Custom)
        {
            let mut routed = codex::convert("responses".to_string(), uri, method, headers, body);
            routed.target = TargetModel::Custom;
            return Ok(routed);
        }
        if crate::source::v1::provider::target_from_request_body(&body)
            == Some(TargetModel::MiniMax)
        {
            return Ok(RoutedRequest {
                target: TargetModel::MiniMax,
                upstream_path: "anthropic/v1/messages".to_string(),
                upstream_query: uri.query().map(|value| value.to_string()),
                upstream_body: crate::source::v1::provider::strip_provider_prefix_from_body(body),
                response_mode: ResponseMode::Passthrough,
            });
        }
        if crate::source::v1::provider::target_from_request_body(&body)
            == Some(TargetModel::Copilot)
        {
            return Ok(RoutedRequest {
                target: TargetModel::Copilot,
                upstream_path: "anthropic/v1/messages".to_string(),
                upstream_query: uri.query().map(|value| value.to_string()),
                upstream_body: crate::source::v1::provider::strip_provider_prefix_from_body(body),
                response_mode: ResponseMode::Passthrough,
            });
        }
        if crate::source::v1::provider::target_from_request_body(&body)
            .is_some_and(|target| matches!(target, TargetModel::Glm | TargetModel::Compatible))
        {
            return Ok(RoutedRequest {
                target: crate::source::v1::provider::target_from_request_body(&body).unwrap(),
                upstream_path: "anthropic/v1/messages".to_string(),
                upstream_query: uri.query().map(|value| value.to_string()),
                upstream_body: crate::source::v1::provider::strip_provider_prefix_from_body(body),
                response_mode: ResponseMode::Passthrough,
            });
        }
        if crate::source::v1::provider::target_from_request_body(&body) == Some(TargetModel::Claude)
        {
            return Ok(RoutedRequest {
                target: TargetModel::Claude,
                upstream_path: "anthropic/v1/messages".to_string(),
                upstream_query: uri.query().map(|value| value.to_string()),
                upstream_body: crate::source::v1::provider::strip_provider_prefix_from_body(body),
                response_mode: ResponseMode::Passthrough,
            });
        }

        // Legacy bridge: Claude-style messages endpoint to Codex responses target.
        return Ok(codex::convert(
            "responses".to_string(),
            uri,
            method,
            headers,
            body,
        ));
    }

    if upstream_path == "responses" && *method == Method::POST {
        if crate::source::v1::provider::target_from_request_body(&body) == Some(TargetModel::Custom)
        {
            return Ok(crate::source::v1::provider::convert(
                TargetModel::Custom,
                upstream_path,
                uri,
                body,
            ));
        }
        if crate::source::v1::provider::target_from_request_body(&body)
            == Some(TargetModel::MiniMax)
        {
            return Ok(crate::source::v1::provider::convert(
                TargetModel::MiniMax,
                upstream_path,
                uri,
                body,
            ));
        }
        if crate::source::v1::provider::target_from_request_body(&body)
            == Some(TargetModel::Copilot)
        {
            return Ok(crate::source::v1::provider::convert(
                TargetModel::Copilot,
                upstream_path,
                uri,
                body,
            ));
        }
        if crate::source::v1::provider::target_from_request_body(&body)
            .is_some_and(|target| matches!(target, TargetModel::Glm | TargetModel::Compatible))
        {
            return Ok(crate::source::v1::provider::convert(
                crate::source::v1::provider::target_from_request_body(&body).unwrap(),
                upstream_path,
                uri,
                body,
            ));
        }
        if crate::source::v1::provider::target_from_request_body(&body) == Some(TargetModel::Claude)
        {
            return Ok(crate::source::v1::provider::convert(
                TargetModel::Claude,
                upstream_path,
                uri,
                body,
            ));
        }
    }

    Ok(codex::convert(upstream_path, uri, method, headers, body))
}

fn is_messages_path(path: &str) -> bool {
    path == "messages" || path == "v1/messages"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_v1_messages_routes_minimax_to_anthropic_pass_through() {
        let uri: Uri = "/claude/v1/messages".parse().unwrap();
        let routed = route_to_target(
            "/claude/v1/messages",
            &uri,
            &Method::POST,
            &HeaderMap::new(),
            Bytes::from_static(br#"{"model":"MiniMax-M3[1m]","messages":[]}"#),
        )
        .unwrap();

        assert_eq!(routed.target, TargetModel::MiniMax);
        assert_eq!(routed.upstream_path, "anthropic/v1/messages");
        let body: serde_json::Value = serde_json::from_slice(&routed.upstream_body).unwrap();
        assert_eq!(body["model"], "MiniMax-M3[1m]");
    }

    #[test]
    fn claude_v1_messages_routes_copilot_to_anthropic_bridge() {
        let uri: Uri = "/claude/v1/messages".parse().unwrap();
        let routed = route_to_target(
            "/claude/v1/messages",
            &uri,
            &Method::POST,
            &HeaderMap::new(),
            Bytes::from_static(br#"{"model":"cop:claude-sonnet-4.5","messages":[]}"#),
        )
        .unwrap();

        assert_eq!(routed.target, TargetModel::Copilot);
        assert_eq!(routed.upstream_path, "anthropic/v1/messages");
        let body: serde_json::Value = serde_json::from_slice(&routed.upstream_body).unwrap();
        assert_eq!(body["model"], "claude-sonnet-4.5");
    }

    #[test]
    fn claude_v1_messages_routes_glm_to_anthropic_pass_through() {
        let uri: Uri = "/claude/v1/messages".parse().unwrap();
        let routed = route_to_target(
            "/claude/v1/messages",
            &uri,
            &Method::POST,
            &HeaderMap::new(),
            Bytes::from_static(br#"{"model":"glm:glm-5.2","messages":[]}"#),
        )
        .unwrap();

        assert_eq!(routed.target, TargetModel::Glm);
        assert_eq!(routed.upstream_path, "anthropic/v1/messages");
        let body: serde_json::Value = serde_json::from_slice(&routed.upstream_body).unwrap();
        assert_eq!(body["model"], "glm-5.2");
    }

    #[test]
    fn claude_messages_routes_custom_model_to_responses_bridge() {
        let uri: Uri = "/claude/messages".parse().unwrap();
        let mut headers = HeaderMap::new();
        headers.insert("content-type", "application/json".parse().unwrap());
        let routed = route_to_target(
            "/claude/messages",
            &uri,
            &Method::POST,
            &headers,
            Bytes::from_static(br#"{"model":"ctm:workhorse","messages":[{"role":"user","content":"hello"}],"max_tokens":100}"#),
        )
        .unwrap();

        assert_eq!(routed.target, TargetModel::Custom);
        assert_eq!(routed.upstream_path, "responses");
        let body: serde_json::Value = serde_json::from_slice(&routed.upstream_body).unwrap();
        assert_eq!(body["model"], "ctm:workhorse");
        assert_eq!(body["input"][0]["content"][0]["text"], "hello");
        assert!(body.get("messages").is_none());
    }

    #[test]
    fn claude_messages_keeps_non_provider_model_on_codex_bridge() {
        let uri: Uri = "/claude/messages".parse().unwrap();
        let routed = route_to_target(
            "/claude/messages",
            &uri,
            &Method::POST,
            &HeaderMap::new(),
            Bytes::from_static(br#"{"model":"gpt-5.1","messages":[]}"#),
        )
        .unwrap();

        assert_eq!(routed.target, TargetModel::Codex);
        assert_eq!(routed.upstream_path, "responses");
    }

    #[test]
    fn claude_v1_messages_routes_native_claude_to_anthropic_oauth() {
        let uri: Uri = "/claude/v1/messages".parse().unwrap();
        let routed = route_to_target(
            "/claude/v1/messages",
            &uri,
            &Method::POST,
            &HeaderMap::new(),
            Bytes::from_static(br#"{"model":"cld:claude-sonnet-4-20250514","messages":[]}"#),
        )
        .unwrap();

        assert_eq!(routed.target, TargetModel::Claude);
        assert_eq!(routed.upstream_path, "anthropic/v1/messages");
        let body: serde_json::Value = serde_json::from_slice(&routed.upstream_body).unwrap();
        assert_eq!(body["model"], "claude-sonnet-4-20250514");
    }

    #[test]
    fn claude_models_routes_to_unified_catalog() {
        let uri: Uri = "/claude/models".parse().unwrap();
        let routed = route_to_target(
            "/claude/models",
            &uri,
            &Method::GET,
            &HeaderMap::new(),
            Bytes::new(),
        )
        .unwrap();

        assert_eq!(routed.target, TargetModel::UnifiedV1Models);
        assert_eq!(routed.upstream_path, "models");
    }

    #[test]
    fn claude_v1_models_routes_to_unified_catalog() {
        let uri: Uri = "/claude/v1/models".parse().unwrap();
        let routed = route_to_target(
            "/claude/v1/models",
            &uri,
            &Method::GET,
            &HeaderMap::new(),
            Bytes::new(),
        )
        .unwrap();

        assert_eq!(routed.target, TargetModel::UnifiedV1Models);
        assert_eq!(routed.upstream_path, "models");
    }

    #[test]
    fn claude_v1_models_supports_head_method() {
        let uri: Uri = "/claude/v1/models".parse().unwrap();
        let routed = route_to_target(
            "/claude/v1/models",
            &uri,
            &Method::HEAD,
            &HeaderMap::new(),
            Bytes::new(),
        )
        .unwrap();

        assert_eq!(routed.target, TargetModel::UnifiedV1Models);
        assert_eq!(routed.upstream_path, "models");
    }

    #[test]
    fn claude_v1_models_preserves_query_string() {
        let uri: Uri = "/claude/v1/models?cursor=abc&limit=50".parse().unwrap();
        let routed = route_to_target(
            "/claude/v1/models",
            &uri,
            &Method::GET,
            &HeaderMap::new(),
            Bytes::new(),
        )
        .unwrap();

        assert_eq!(routed.target, TargetModel::UnifiedV1Models);
        assert_eq!(routed.upstream_path, "models");
        assert_eq!(
            routed.upstream_query.as_deref(),
            Some("cursor=abc&limit=50")
        );
    }
}

/// Lists models through the Claude-prefixed compatibility surface.
#[utoipa::path(
    get,
    path = "/claude/models",
    security(("bearer_auth" = [])),
    responses(
        (
            status = 200,
            description = "Raw model list",
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

/// Lists models through the Claude-prefixed, versioned compatibility surface.
#[utoipa::path(
    get,
    path = "/claude/v1/models",
    security(("bearer_auth" = [])),
    responses(
        (
            status = 200,
            description = "Raw model list",
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
pub(crate) fn models_v1_doc() {}

/// Accepts a Claude-style messages payload and bridges it to the Codex responses backend.
#[utoipa::path(
    post,
    path = "/claude/messages",
    request_body(
        content = crate::source::openapi::ClaudeMessagesCreateRequest,
        content_type = "application/json",
        description = "Claude-style messages payload"
    ),
    security(("bearer_auth" = [])),
    responses(
        (
            status = 200,
            description = "Bridged response from the Codex backend",
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
pub(crate) fn messages_doc() {}

/// Sends a Codex-compatible responses payload through the Claude-prefixed surface.
#[utoipa::path(
    post,
    path = "/claude/responses",
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
