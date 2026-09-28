use axum::http::{HeaderMap, Method};
use bytes::Bytes;

const DEFAULT_CODEX_CLIENT_VERSION: &str = "1.0.0";
const DEFAULT_CODEX_USER_AGENT: &str =
    "codex_cli_rs/0.76.0 (Debian 13.0.0; x86_64) WindowsTerminal";

pub fn build_upstream_url(base: &str, path: &str, query: Option<&str>) -> String {
    let mut base = base.trim_end_matches('/').to_string();
    if !path.is_empty() {
        base.push('/');
        base.push_str(path.trim_start_matches('/'));
    }
    if let Some(q) = query {
        base.push('?');
        base.push_str(q);
    }
    base
}

pub fn build_request_body(
    method: &Method,
    upstream_path: &str,
    headers: &HeaderMap,
    body: Bytes,
    session_id: &str,
) -> Bytes {
    if *method == Method::POST && upstream_path == "responses" {
        let body = maybe_apply_prompt_cache_key(headers, body, session_id);
        return ensure_store_and_stream(headers, body);
    }
    body
}

pub fn apply_default_headers(
    mut req: reqwest::RequestBuilder,
    incoming: &HeaderMap,
    account_id: Option<&str>,
    session_id: &str,
) -> reqwest::RequestBuilder {
    if !incoming.contains_key("content-type") {
        req = req.header("Content-Type", "application/json");
    }
    if !incoming.contains_key("accept") {
        req = req.header("Accept", "text/event-stream");
    }
    if !incoming.contains_key("connection") {
        req = req.header("Connection", "Keep-Alive");
    }
    if !incoming.contains_key("openai-beta") {
        req = req.header("Openai-Beta", "responses=experimental");
    }
    // Force upstream Codex API version to a modern baseline even when client
    // sends an older `Version` header.
    req = req.header("Version", DEFAULT_CODEX_CLIENT_VERSION);
    // Session/conversation and account selection belong to the gateway's
    // chosen upstream credential.  They must not be inherited from the
    // client: a caller could otherwise resume another account's retained
    // context or override a selected-account API-key scope.  The proxy drops
    // these headers as a first line of defense; force replacement here too so
    // every direct gateway caller gets the same invariant.
    req = req.header("Session_id", session_id);
    req = req.header("Conversation_id", session_id);
    if !incoming.contains_key("user-agent") {
        req = req.header("User-Agent", DEFAULT_CODEX_USER_AGENT);
    }
    if !incoming.contains_key("origin") {
        req = req.header("Origin", "https://chatgpt.com");
    }
    if !incoming.contains_key("referer") {
        req = req.header("Referer", "https://chatgpt.com/");
    }
    if !incoming.contains_key("accept-language") {
        req = req.header("Accept-Language", "en-US,en;q=0.9");
    }
    if !incoming.contains_key("accept-encoding") {
        req = req.header("Accept-Encoding", "identity");
    }
    if !incoming.contains_key("originator") {
        req = req.header("Originator", "codex_cli_rs");
    }
    if let Some(id) = account_id {
        if !id.trim().is_empty() {
            req = req.header("Chatgpt-Account-Id", id);
        }
    }
    req
}

fn maybe_apply_prompt_cache_key(headers: &HeaderMap, body: Bytes, session_id: &str) -> Bytes {
    if let Some(ct) = headers.get("content-type").and_then(|v| v.to_str().ok()) {
        if !ct.to_ascii_lowercase().contains("application/json") {
            return body;
        }
    } else {
        return body;
    }

    let mut value: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return body,
    };
    if let serde_json::Value::Object(map) = &mut value {
        if !map.contains_key("prompt_cache_key") {
            map.insert(
                "prompt_cache_key".to_string(),
                serde_json::Value::String(session_id.to_string()),
            );
        }
        if !map.contains_key("stream") {
            map.insert("stream".to_string(), serde_json::Value::Bool(true));
        }
        if let Ok(out) = serde_json::to_vec(&value) {
            return Bytes::from(out);
        }
    }
    body
}

fn ensure_store_and_stream(headers: &HeaderMap, body: Bytes) -> Bytes {
    if let Some(ct) = headers.get("content-type").and_then(|v| v.to_str().ok()) {
        if !ct.to_ascii_lowercase().contains("application/json") {
            return body;
        }
    } else {
        return body;
    }

    let mut value: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return body,
    };
    if let serde_json::Value::Object(map) = &mut value {
        map.insert("store".to_string(), serde_json::Value::Bool(false));
        map.insert("stream".to_string(), serde_json::Value::Bool(true));
        if let Ok(out) = serde_json::to_vec(&value) {
            return Bytes::from(out);
        }
    }
    body
}

#[cfg(test)]
mod tests {
    use super::apply_default_headers;
    use axum::http::{HeaderMap, HeaderValue};

    #[test]
    fn gateway_owned_routing_headers_override_client_values() {
        let mut incoming = HeaderMap::new();
        incoming.insert(
            "chatgpt-account-id",
            HeaderValue::from_static("client-account"),
        );
        incoming.insert("session_id", HeaderValue::from_static("client-session"));
        incoming.insert(
            "conversation_id",
            HeaderValue::from_static("client-conversation"),
        );

        let request = apply_default_headers(
            reqwest::Client::new().get("https://example.test/responses"),
            &incoming,
            Some("gateway-account"),
            "gateway-session",
        )
        .build()
        .unwrap();

        assert_eq!(
            request.headers().get("chatgpt-account-id").unwrap(),
            "gateway-account"
        );
        assert_eq!(
            request.headers().get("session_id").unwrap(),
            "gateway-session"
        );
        assert_eq!(
            request.headers().get("conversation_id").unwrap(),
            "gateway-session"
        );
    }
}
