//! Conservative input accounting for JSON API requests.
//!
//! This module deliberately does *not* try to identify only the familiar
//! `input`, `messages`, or `system` fields.  A client can put model-visible
//! text in tool definitions, function results, provider extensions, or fields
//! the gateway does not yet know about.  Instead, every byte of the serialized
//! JSON request is treated as potentially billable input.  A byte is a safe
//! (if intentionally conservative) upper bound for one tokenizer unit.
//!
//! Some request shapes refer to content whose token cost cannot be determined
//! from the JSON alone, such as an image, an uploaded file, or server-side
//! conversation history.  Those shapes are still included in the byte bound,
//! but mark the footprint incomplete so a caller enforcing a request cap can
//! reject the request rather than accidentally treating that content as zero.

use serde_json::Value;

/// Why a request's token footprint cannot be completely assessed locally.
///
/// The JSON pointer identifies the structural location only; it deliberately
/// never stores the potentially sensitive value found at that location.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct UnmeasurableInput {
    pub kind: UnmeasurableInputKind,
    pub json_pointer: String,
}

/// Categories of input which have an upstream-dependent token cost.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum UnmeasurableInputKind {
    /// An image, audio/video payload, document, file, or other media source.
    Media,
    /// A reference to provider-retained or remotely loaded context.
    RemoteContext,
}

/// A conservative accounting result for one fully prepared JSON request.
///
/// `upper_bound_tokens` is the number of UTF-8 bytes in the JSON wire value.
/// Byte-oriented tokenizers can always represent a byte individually, so this
/// is a deliberately high safe bound when an exact provider/model tokenizer is
/// unavailable.  It includes field names, strings, scalar values, and JSON
/// punctuation so unknown fields cannot become a limit bypass.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct InputFootprint {
    /// Number of Unicode scalar values in the serialized JSON wire value.
    /// This includes JSON syntax and escaping, matching the same conservative
    /// scope as `upper_bound_tokens` while retaining a human-readable metric.
    pub input_chars: u64,
    /// Number of scalar JSON values. Empty arrays/objects count as one item so
    /// a structurally meaningful, but empty, value cannot disappear from
    /// telemetry.
    pub prompt_items: u64,
    /// Conservative UTF-8 byte token upper bound for the complete JSON value.
    pub upper_bound_tokens: u64,
    /// Whether all model-visible input is locally measurable.
    pub complete: bool,
    /// Media or remote-context shapes that made `complete` false.
    pub unmeasurable: Vec<UnmeasurableInput>,
}

impl InputFootprint {
    /// Returns true when the request contains an image, file, audio/video, or
    /// similar content whose model tokenization cannot be inferred from JSON.
    pub(crate) fn has_unmeasurable_media(&self) -> bool {
        self.unmeasurable
            .iter()
            .any(|item| item.kind == UnmeasurableInputKind::Media)
    }

    /// Returns true when a provider may inject retained or remotely fetched
    /// context that is not represented in the request JSON.
    pub(crate) fn has_unmeasurable_remote_context(&self) -> bool {
        self.unmeasurable
            .iter()
            .any(|item| item.kind == UnmeasurableInputKind::RemoteContext)
    }
}

/// Assess a fully prepared request body before it is dispatched upstream.
///
/// Call this after gateway/provider adapters have injected system instructions
/// or converted a client request into its upstream shape.  Calling it on the
/// original client request alone cannot account for injected text.
pub(crate) fn assess_request_value(value: &Value) -> InputFootprint {
    assess_with_schema_layout(value, SchemaLayout::Standard)
}

/// The Google adapter's final payload has different schema locations from
/// OpenAI/Anthropic, optionally inside the gateway's `request` envelope. This
/// entrypoint is selected by the adapter/provider, never by a client field.
pub(crate) fn assess_google_request_value(value: &Value) -> InputFootprint {
    assess_with_schema_layout(
        value,
        SchemaLayout::Google {
            envelope: value.get("request").is_some(),
        },
    )
}

#[derive(Clone, Copy)]
enum SchemaLayout {
    Standard,
    Google { envelope: bool },
}

fn assess_with_schema_layout(value: &Value, schema_layout: SchemaLayout) -> InputFootprint {
    // `serde_json::Value` serialization is infallible. Keep the fallback
    // defensive in case serde_json's implementation changes in the future:
    // an unknown serialization failure must not result in a zero allowance.
    let encoded = serde_json::to_vec(value).unwrap_or_else(|_| b"null".to_vec());
    let input_chars = u64::try_from(
        std::str::from_utf8(&encoded)
            .expect("serde_json output is valid UTF-8")
            .chars()
            .count(),
    )
    .unwrap_or(u64::MAX);
    let upper_bound_tokens = u64::try_from(encoded.len()).unwrap_or(u64::MAX);

    let mut unmeasurable = Vec::new();
    scan_for_unmeasurable(value, "", false, schema_layout, &mut unmeasurable);

    InputFootprint {
        input_chars,
        prompt_items: semantic_item_count(value),
        upper_bound_tokens,
        complete: unmeasurable.is_empty(),
        unmeasurable,
    }
}

/// Count values rather than only familiar prompt strings. This is intentionally
/// a telemetry metric; token enforcement must use `upper_bound_tokens` and
/// `complete`, not an `items` multiplier.
fn semantic_item_count(value: &Value) -> u64 {
    match value {
        Value::Array(values) => {
            if values.is_empty() {
                1
            } else {
                values.iter().fold(0_u64, |count, value| {
                    count.saturating_add(semantic_item_count(value))
                })
            }
        }
        Value::Object(values) => {
            if values.is_empty() {
                1
            } else {
                values.values().fold(0_u64, |count, value| {
                    count.saturating_add(semantic_item_count(value))
                })
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => 1,
    }
}

fn scan_for_unmeasurable(
    value: &Value,
    json_pointer: &str,
    inside_literal_json: bool,
    schema_layout: SchemaLayout,
    found: &mut Vec<UnmeasurableInput>,
) {
    match value {
        Value::Array(items) => {
            for (index, item) in items.iter().enumerate() {
                scan_for_unmeasurable(
                    item,
                    &pointer_child(json_pointer, &index.to_string()),
                    inside_literal_json,
                    schema_layout,
                    found,
                );
            }
        }
        Value::Object(object) => {
            // Schemas and function arguments may use names such as `image_url`
            // or `context`, but these values are literal JSON at their native
            // protocol positions, not automatically loaded uploads/references.
            if !inside_literal_json {
                scan_object_markers(object, json_pointer, found);
            }

            for (key, child) in object {
                let child_is_literal = inside_literal_json
                    || is_json_schema_container(json_pointer, key, child, schema_layout)
                    || is_json_argument_container(json_pointer, key, object, schema_layout);
                scan_for_unmeasurable(
                    child,
                    &pointer_child(json_pointer, key),
                    child_is_literal,
                    schema_layout,
                    found,
                );
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }
}

fn scan_object_markers(
    object: &serde_json::Map<String, Value>,
    json_pointer: &str,
    found: &mut Vec<UnmeasurableInput>,
) {
    let type_name = object
        .get("type")
        .and_then(Value::as_str)
        .map(|value| value.trim().to_ascii_lowercase());

    let object_is_media = type_name.as_deref().is_some_and(is_media_part_type)
        || object_looks_like_inline_media(object);
    let object_is_remote_context = type_name
        .as_deref()
        .is_some_and(is_remote_context_part_type);

    if object_is_media {
        add_unmeasurable(found, UnmeasurableInputKind::Media, json_pointer);
    }
    if object_is_remote_context {
        add_unmeasurable(found, UnmeasurableInputKind::RemoteContext, json_pointer);
    }

    for (key, child) in object {
        // Nullable optional references explicitly mean there is no attached
        // media or retained response. Their literal JSON still counts in the
        // byte bound, but they cannot add hidden context. A typed media or
        // remote-context part was already marked above regardless of fields.
        if child.is_null() {
            continue;
        }
        // A typed media part already has one clear marker at the part itself;
        // avoid emitting duplicate events for its `image_url`/`file_data`.
        if !object_is_media && is_media_field(key) {
            add_unmeasurable(
                found,
                UnmeasurableInputKind::Media,
                &pointer_child(json_pointer, key),
            );
        }
        // Native Anthropic's MCP connector can inject remote tool results
        // without a separate `tools: [{type: "mcp_toolset"}]` declaration.
        // An empty server list attaches nothing; malformed nonempty values
        // remain conservatively unmeasurable.
        let has_mcp_servers =
            key == "mcp_servers" && !child.as_array().is_some_and(|servers| servers.is_empty());
        if !object_is_remote_context && (is_remote_context_field(key) || has_mcp_servers) {
            add_unmeasurable(
                found,
                UnmeasurableInputKind::RemoteContext,
                &pointer_child(json_pointer, key),
            );
        }
    }
}

fn add_unmeasurable(
    found: &mut Vec<UnmeasurableInput>,
    kind: UnmeasurableInputKind,
    json_pointer: &str,
) {
    let json_pointer = json_pointer.to_string();
    if found
        .iter()
        .any(|item| item.kind == kind && item.json_pointer == json_pointer)
    {
        return;
    }
    found.push(UnmeasurableInput { kind, json_pointer });
}

fn is_media_part_type(value: &str) -> bool {
    matches!(
        value,
        "image"
            | "input_image"
            | "image_url"
            | "image_file"
            | "audio"
            | "input_audio"
            | "audio_url"
            | "video"
            | "input_video"
            | "video_url"
            | "file"
            | "input_file"
            | "document"
            | "pdf"
            | "attachment"
            | "computer_screenshot"
    ) || contains_any(
        value,
        &["image", "audio", "video", "media", "document", "attachment"],
    )
}

fn is_remote_context_part_type(value: &str) -> bool {
    matches!(
        value,
        "item_reference"
            | "input_reference"
            | "context_reference"
            | "remote_context"
            | "conversation_reference"
            | "file_reference"
            | "compaction"
    ) || is_hosted_context_tool_type(value)
        || contains_any(
            value,
            &["reference", "remote_context", "conversation_context"],
        )
}

/// Provider-hosted tools can add retrieved/executed content during the same
/// request without including that input in the submitted JSON. A client-side
/// function named `web_search` remains measurable: its type is `function`,
/// and its eventual result must arrive in a separately assessed request.
fn is_hosted_context_tool_type(value: &str) -> bool {
    [
        "web_search",
        "web_fetch",
        "file_search",
        "x_search",
        "code_execution",
        "code_interpreter",
        "mcp",
    ]
    .iter()
    .any(|kind| {
        value == *kind
            || value
                .strip_prefix(kind)
                .is_some_and(|suffix| suffix.starts_with('_'))
    })
}

fn is_media_field(key: &str) -> bool {
    matches!(
        key,
        "image_url"
            | "image_file"
            | "image"
            | "images"
            | "input_image"
            | "audio_url"
            | "audio"
            | "input_audio"
            | "audio_data"
            | "video_url"
            | "video"
            | "input_video"
            | "file_data"
            | "fileData"
            | "file_id"
            | "file_url"
            | "file_uri"
            | "fileUri"
            | "input_file"
            | "inline_data"
            | "inlineData"
            | "media"
            | "media_url"
            | "document"
            | "documents"
            | "attachment"
            | "attachments"
    )
}

fn is_remote_context_field(key: &str) -> bool {
    matches!(
        key,
        "previous_response_id"
            | "previous_message_id"
            | "conversation"
            | "conversation_id"
            | "context"
            | "context_id"
            | "item_reference"
            | "input_reference"
            | "reference_id"
            | "session_id"
            | "thread_id"
            | "container"
            | "container_id"
            | "remote_context"
            | "cached_content"
            | "cached_content_id"
            | "cachedContent"
            | "encrypted_content"
    )
}

/// Only exact native argument locations are literal JSON. A similarly named
/// `args`/`input` field elsewhere (especially in a tool result or a media part)
/// cannot opt out of media/retained-context detection.
fn is_json_argument_container(
    parent: &str,
    key: &str,
    object: &serde_json::Map<String, Value>,
    schema_layout: SchemaLayout,
) -> bool {
    let segments = parent.split('/').skip(1).collect::<Vec<_>>();
    match schema_layout {
        SchemaLayout::Standard => match segments.as_slice() {
            ["messages", message, "content", part]
                if message.parse::<usize>().is_ok() && part.parse::<usize>().is_ok() =>
            {
                key == "input" && object.get("type").and_then(Value::as_str) == Some("tool_use")
            }
            ["input", item] if item.parse::<usize>().is_ok() => {
                key == "arguments"
                    && object.get("type").and_then(Value::as_str) == Some("function_call")
            }
            _ => false,
        },
        SchemaLayout::Google { envelope } => {
            let segments = if envelope {
                let Some(remaining) = segments.strip_prefix(&["request"]) else {
                    return false;
                };
                remaining
            } else {
                segments.as_slice()
            };
            matches!(segments, ["contents", content, "parts", part, "functionCall"]
                if content.parse::<usize>().is_ok() && part.parse::<usize>().is_ok() && key == "args")
        }
    }
}

/// Whether `child` is the subtree of a JSON Schema declaration. The field
/// names cover OpenAI/Anthropic tool definitions and standard JSON Schema
/// composition; this prevents a schema property named `image_url` from being
/// treated as an actual image input.
fn is_json_schema_container(
    parent: &str,
    key: &str,
    child: &Value,
    schema_layout: SchemaLayout,
) -> bool {
    // Schema exemptions must come from the protocol location, not from
    // attacker-controlled keys such as `type: "string"` or `properties` on
    // an actual image/retained-context object. Adapters may ignore those
    // extra keys while still forwarding its URL to the provider.
    let segments = parent.split('/').skip(1).collect::<Vec<_>>();
    if let SchemaLayout::Google { envelope } = schema_layout {
        let segments = if envelope {
            let Some(remaining) = segments.strip_prefix(&["request"]) else {
                return false;
            };
            remaining
        } else {
            segments.as_slice()
        };
        let known_schema = match segments {
            ["tools", tool, "functionDeclarations", function]
                if tool.parse::<usize>().is_ok() && function.parse::<usize>().is_ok() =>
            {
                key == "parameters"
            }
            ["generationConfig"] => key == "responseSchema",
            _ => false,
        };
        // Google's native Schema enum commonly spells types in uppercase,
        // including ARRAY schemas whose sensitive-looking properties are
        // nested under `items` rather than present at the schema root.
        let native_schema_type = child
            .get("type")
            .and_then(Value::as_str)
            .is_some_and(|kind| {
                matches!(
                    kind,
                    "STRING" | "NUMBER" | "INTEGER" | "BOOLEAN" | "NULL" | "OBJECT" | "ARRAY"
                )
            });
        return known_schema && (looks_like_json_schema(child) || native_schema_type);
    }
    let tool_schema = match segments.as_slice() {
        ["tools", index] if index.parse::<usize>().is_ok() => {
            matches!(key, "parameters" | "input_schema" | "output_schema")
        }
        ["tools", index, "function"] if index.parse::<usize>().is_ok() => key == "parameters",
        _ => false,
    };
    let response_schema = matches!(
        (parent, key),
        ("/response_format", "json_schema")
            | ("/response_format/json_schema", "schema")
            | ("/text/format", "schema")
            | ("/generationConfig", "responseSchema")
            | ("/generation_config", "response_schema")
    );
    (tool_schema || response_schema) && looks_like_json_schema(child)
}

/// Recognize MIME/data shapes even when a future provider gives a part an
/// unfamiliar type name. A compact data URL or inline MIME payload can expand
/// into a much more expensive vision/audio/document representation upstream.
fn object_looks_like_inline_media(object: &serde_json::Map<String, Value>) -> bool {
    let has_mime = object.contains_key("mime_type") || object.contains_key("mimeType");
    let has_payload = object.contains_key("data")
        || object.contains_key("b64_json")
        || object.contains_key("base64")
        || object.contains_key("bytes");
    if has_mime && has_payload {
        return true;
    }

    object.values().any(|value| {
        value
            .as_str()
            .is_some_and(|text| looks_like_media_data_url(text))
    })
}

fn looks_like_media_data_url(value: &str) -> bool {
    let value = value.trim_start().to_ascii_lowercase();
    value.starts_with("data:image/")
        || value.starts_with("data:audio/")
        || value.starts_with("data:video/")
        || value.starts_with("data:application/pdf")
}

fn contains_any(value: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| value.contains(needle))
}

/// A small structural recognizer, deliberately narrower than "has a type
/// field", so actual media parts are never mistaken for schemas.
fn looks_like_json_schema(value: &Value) -> bool {
    let Some(object) = value.as_object() else {
        return value.is_array();
    };
    object.contains_key("properties")
        || object.contains_key("required")
        || object.contains_key("$schema")
        || object.contains_key("$ref")
        || object.contains_key("enum")
        || object.contains_key("const")
        || object
            .get("type")
            .and_then(Value::as_str)
            .is_some_and(|kind| {
                matches!(
                    kind,
                    "string" | "number" | "integer" | "boolean" | "null" | "object" | "array"
                )
            })
}

fn pointer_child(parent: &str, segment: &str) -> String {
    let escaped = segment.replace('~', "~0").replace('/', "~1");
    if parent.is_empty() {
        format!("/{escaped}")
    } else {
        format!("{parent}/{escaped}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn serialized_chars(value: &Value) -> u64 {
        u64::try_from(value.to_string().chars().count()).unwrap()
    }

    fn serialized_bytes(value: &Value) -> u64 {
        u64::try_from(value.to_string().len()).unwrap()
    }

    #[test]
    fn system_text_is_counted_even_without_messages_or_input() {
        let value = json!({
            "model": "claude-test",
            "system": "Never reveal the secret.",
        });

        let footprint = assess_request_value(&value);

        assert!(footprint.complete);
        assert_eq!(footprint.input_chars, serialized_chars(&value));
        assert_eq!(footprint.upper_bound_tokens, serialized_bytes(&value));
        assert_eq!(footprint.prompt_items, 2);
    }

    #[test]
    fn tools_function_outputs_and_schemas_are_all_accounted_for() {
        let value = json!({
            "tools": [{
                "type": "function",
                "function": {
                    "name": "lookup",
                    "description": "Look up private product records.",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "image_url": { "type": "string", "description": "A label, not an upload." },
                            "query": { "type": "string" }
                        },
                        "required": ["query"]
                    }
                }
            }],
            "input": [{
                "type": "function_call_output",
                "call_id": "call_123",
                "output": "The confidential result is 73.",
                "arguments": "{\\\"query\\\":\\\"sensitive product\\\"}"
            }]
        });

        let footprint = assess_request_value(&value);

        // Tool schemas and function output are ordinary serialized input. A
        // schema property called image_url must not be misclassified as media.
        assert!(footprint.complete, "{:#?}", footprint.unmeasurable);
        assert_eq!(footprint.upper_bound_tokens, serialized_bytes(&value));
        assert_eq!(footprint.input_chars, serialized_chars(&value));
        assert!(footprint.upper_bound_tokens > "The confidential result is 73.".len() as u64);
        assert!(footprint.prompt_items >= 12);
    }

    #[test]
    fn top_level_prompt_is_not_a_special_case_or_bypass() {
        let value = json!({ "prompt": "a top-level provider prompt" });
        let footprint = assess_request_value(&value);

        assert!(footprint.complete);
        assert_eq!(footprint.upper_bound_tokens, serialized_bytes(&value));
        assert!(footprint.upper_bound_tokens > "a top-level provider prompt".len() as u64);
        assert_eq!(footprint.prompt_items, 1);
    }

    #[test]
    fn cjk_and_emoji_use_utf8_bytes_not_character_division() {
        let value = json!({ "input": "你好🙂" });
        let footprint = assess_request_value(&value);

        assert!(footprint.complete);
        assert_eq!(footprint.input_chars, serialized_chars(&value));
        assert_eq!(footprint.upper_bound_tokens, serialized_bytes(&value));
        assert!(footprint.upper_bound_tokens > footprint.input_chars);
    }

    #[test]
    fn unknown_fields_and_nested_structure_still_count() {
        let value = json!({
            "future_provider_payload": {
                "opaque_instruction": "this must not be ignored",
                "nested": [true, 42, { "new_field": "still counted" }]
            }
        });
        let footprint = assess_request_value(&value);

        assert!(footprint.complete);
        assert_eq!(footprint.input_chars, serialized_chars(&value));
        assert_eq!(footprint.upper_bound_tokens, serialized_bytes(&value));
        assert_eq!(footprint.prompt_items, 4);
        assert!(footprint.upper_bound_tokens > "this must not be ignored".len() as u64);
    }

    #[test]
    fn openai_image_content_is_incomplete_even_when_its_data_url_is_present() {
        let value = json!({
            "input": [{
                "type": "input_image",
                "image_url": "data:image/png;base64,AAAA"
            }]
        });
        let footprint = assess_request_value(&value);

        assert!(!footprint.complete);
        assert!(footprint.has_unmeasurable_media());
        assert_eq!(footprint.upper_bound_tokens, serialized_bytes(&value));
        assert_eq!(
            footprint.unmeasurable,
            vec![UnmeasurableInput {
                kind: UnmeasurableInputKind::Media,
                json_pointer: "/input/0".to_string(),
            }]
        );
    }

    #[test]
    fn bare_image_url_and_gemini_inline_data_are_detected_as_media() {
        let value = json!({
            "content": [{ "image_url": { "url": "https://example.test/a.png" } }],
            "parts": [{ "inlineData": { "mimeType": "image/png", "data": "AAAA" } }]
        });
        let footprint = assess_request_value(&value);

        assert!(!footprint.complete);
        assert!(footprint.has_unmeasurable_media());
        assert_eq!(footprint.upper_bound_tokens, serialized_bytes(&value));
        assert!(footprint.unmeasurable.iter().any(|item| {
            item.kind == UnmeasurableInputKind::Media && item.json_pointer == "/content/0/image_url"
        }));
        assert!(footprint.unmeasurable.iter().any(|item| {
            item.kind == UnmeasurableInputKind::Media && item.json_pointer == "/parts/0/inlineData"
        }));
    }

    #[test]
    fn an_items_array_is_not_mistaken_for_a_schema_and_cannot_hide_media() {
        let value = json!({
            "items": [{
                "type": "input_image",
                "image_url": "data:image/png;base64,AAAA"
            }]
        });
        let footprint = assess_request_value(&value);

        assert!(!footprint.complete);
        assert!(footprint.has_unmeasurable_media());
        assert!(footprint.unmeasurable.iter().any(|item| {
            item.kind == UnmeasurableInputKind::Media && item.json_pointer == "/items/0"
        }));
    }

    #[test]
    fn unfamiliar_media_type_and_inline_mime_data_fail_closed() {
        let value = json!({
            "input": [{
                "type": "future_image_asset",
                "mimeType": "image/webp",
                "data": "AAAA"
            }]
        });
        let footprint = assess_request_value(&value);

        assert!(!footprint.complete);
        assert!(footprint.has_unmeasurable_media());
        assert_eq!(footprint.upper_bound_tokens, serialized_bytes(&value));
    }

    #[test]
    fn provider_retained_context_is_detected_without_losing_its_json_bound() {
        let value = json!({
            "previous_response_id": "resp_abc",
            "input": "continue from the retained response"
        });
        let footprint = assess_request_value(&value);

        assert!(!footprint.complete);
        assert!(footprint.has_unmeasurable_remote_context());
        assert_eq!(footprint.upper_bound_tokens, serialized_bytes(&value));
        assert_eq!(
            footprint.unmeasurable,
            vec![UnmeasurableInput {
                kind: UnmeasurableInputKind::RemoteContext,
                json_pointer: "/previous_response_id".to_string(),
            }]
        );
    }

    #[test]
    fn item_reference_is_detected_as_remote_context() {
        let value = json!({
            "input": [{ "type": "item_reference", "id": "msg_123" }]
        });
        let footprint = assess_request_value(&value);

        assert!(!footprint.complete);
        assert!(footprint.has_unmeasurable_remote_context());
        assert!(footprint.unmeasurable.iter().any(|item| {
            item.kind == UnmeasurableInputKind::RemoteContext && item.json_pointer == "/input/0"
        }));
    }

    #[test]
    fn json_schema_names_that_resemble_media_or_context_do_not_hide_text() {
        let value = json!({
            "tools": [{
                "name": "configure",
                "input_schema": {
                    "type": "object",
                    "properties": {
                        "image_url": { "type": "string" },
                        "previous_response_id": { "type": "string" }
                    }
                }
            }]
        });
        let footprint = assess_request_value(&value);

        assert!(footprint.complete, "{:#?}", footprint.unmeasurable);
        assert_eq!(footprint.upper_bound_tokens, serialized_bytes(&value));
        assert_eq!(footprint.input_chars, serialized_chars(&value));
    }

    #[test]
    fn schema_looking_media_and_remote_values_cannot_evade_measurement() {
        for disguised in [
            json!({ "image_url": { "url": "https://example.test/image.png", "type": "string" } }),
            json!({ "image_url": { "url": "https://example.test/image.png", "properties": {} } }),
            json!({ "images": ["https://example.test/image.png"] }),
            json!({ "previous_response_id": { "id": "resp_private", "type": "string" } }),
        ] {
            let value = json!({ "input": [{ "role": "user", "content": [disguised] }] });
            let footprint = assess_request_value(&value);
            assert!(
                !footprint.complete,
                "disguised media/context was accepted: {value}"
            );
        }
    }

    #[test]
    fn arbitrary_schema_named_payload_does_not_suppress_media_scanning() {
        let value = json!({
            "input": [{
                "schema": {
                    "type": "object",
                    "image_url": { "url": "https://example.test/image.png" }
                }
            }]
        });
        let footprint = assess_request_value(&value);
        assert!(!footprint.complete);
        assert!(footprint.has_unmeasurable_media());
    }

    #[test]
    fn actual_tool_and_response_schemas_are_still_plain_input() {
        let schema = json!({
            "type": "object",
            "properties": {
                "image_url": { "type": "string" },
                "encrypted_content": { "type": "string" }
            }
        });
        for value in [
            json!({ "tools": [{ "type": "function", "function": { "name": "describe", "parameters": schema } }] }),
            json!({ "response_format": { "type": "json_schema", "json_schema": { "name": "result", "schema": schema } } }),
            json!({ "text": { "format": { "type": "json_schema", "name": "result", "schema": schema } } }),
        ] {
            let footprint = assess_request_value(&value);
            assert!(
                footprint.complete,
                "schema treated as media/context: {:#?}",
                footprint.unmeasurable
            );
        }
    }

    #[test]
    fn opaque_compaction_and_encrypted_context_require_measurement() {
        for value in [
            json!({ "input": [{ "type": "compaction", "encrypted_content": "opaque" }] }),
            json!({ "input": [{ "type": "reasoning", "encrypted_content": "opaque" }] }),
            json!({ "cachedContent": "cachedContents/private" }),
        ] {
            let footprint = assess_request_value(&value);
            assert!(!footprint.complete);
            assert!(footprint.has_unmeasurable_remote_context());
        }
    }

    #[test]
    fn provider_hosted_retrieval_and_execution_require_measurement() {
        for kind in [
            "web_search",
            "web_search_preview_2025_03_11",
            "web_search_20250305",
            "web_fetch_20250910",
            "file_search",
            "x_search",
            "code_execution",
            "code_execution_20250522",
            "code_interpreter",
            "mcp",
            "mcp_call",
        ] {
            let value = json!({ "input": "hello", "tools": [{ "type": kind }] });
            let footprint = assess_request_value(&value);
            assert!(
                !footprint.complete,
                "hosted tool {kind} must not appear fully measurable"
            );
            assert!(footprint.has_unmeasurable_remote_context());
        }
    }

    #[test]
    fn ordinary_function_names_and_text_do_not_become_hosted_tools() {
        let value = json!({
            "input": "please mention web_search and code_execution",
            "tools": [{
                "type": "function",
                "name": "web_search",
                "parameters": { "type": "object", "properties": { "mcp": { "type": "string" } } }
            }, {
                "type": "function",
                "function": { "name": "code_execution", "parameters": { "type": "object" } }
            }]
        });
        let footprint = assess_request_value(&value);
        assert!(
            footprint.complete,
            "ordinary client-side tool declarations are measurable"
        );
    }

    #[test]
    fn native_mcp_servers_require_remote_context_measurement() {
        let value = json!({
            "model":"claude-sonnet-4", "max_tokens":20,
            "messages":[{"role":"user","content":"use the remote service"}],
            "mcp_servers":[{"type":"url","url":"https://example.test/mcp","name":"fixture"}]
        });
        let footprint = assess_request_value(&value);
        assert!(!footprint.complete);
        assert!(footprint.has_unmeasurable_remote_context());
        assert_eq!(footprint.upper_bound_tokens, serialized_bytes(&value));
        assert!(footprint.unmeasurable.iter().any(|item| {
            item.kind == UnmeasurableInputKind::RemoteContext && item.json_pointer == "/mcp_servers"
        }));

        for empty in [json!([]), Value::Null] {
            assert!(assess_request_value(&json!({"input":"hello","mcp_servers":empty})).complete);
        }
        let schema = json!({
            "tools":[{"name":"describe", "input_schema":{
                "type":"object", "properties":{"mcp_servers":{"type":"array","items":{"type":"string"}}}
            }}]
        });
        assert!(assess_request_value(&schema).complete);
    }

    #[test]
    fn google_schema_layout_is_not_selected_by_client_request_fields() {
        let disguised = json!({
            "input":"hello",
            "request":{"tools":[{"functionDeclarations":[{"parameters":{
                "type":"object", "properties":{"image_url":{"type":"string"}}
            }}]}]}
        });
        assert!(!assess_request_value(&disguised).complete);
    }

    #[test]
    fn native_function_arguments_are_measurable_literal_json() {
        let arguments = json!({
            "context":"query", "file_id":"label", "image_url":"not an attachment",
            "nested":{"type":"input_image","mcp_servers":["a literal argument"]}
        });
        for value in [
            json!({"messages":[{"role":"assistant","content":[{
                "type":"tool_use","id":"call","name":"lookup","input":arguments
            }]}]}),
            json!({"input":[{"type":"function_call","call_id":"call","name":"lookup","arguments":arguments}]}),
        ] {
            let footprint = assess_request_value(&value);
            assert!(footprint.complete, "{:?}", footprint.unmeasurable);
            assert_eq!(footprint.upper_bound_tokens, serialized_bytes(&value));
        }
        let native = json!({"contents":[{"role":"model","parts":[{
            "functionCall":{"name":"lookup","args":arguments}
        }]}]});
        for value in [
            native.clone(),
            json!({"model":"gemini-2.5-flash","request":native}),
        ] {
            let footprint = assess_google_request_value(&value);
            assert!(footprint.complete, "{:?}", footprint.unmeasurable);
            assert_eq!(footprint.upper_bound_tokens, serialized_bytes(&value));
        }
    }

    #[test]
    fn function_argument_exemptions_cannot_hide_results_or_adjacent_media() {
        for value in [
            json!({"args":{"file_id":"private"}}),
            json!({"input":{"type":"tool_use","input":{"context":"private"}}}),
            json!({"messages":[{"content":[{"type":"tool_result","input":{"context":"private"}}]}]}),
            json!({"messages":[{"content":[{"type":"tool_result","content":[{"type":"image","source":{"type":"url","url":"https://example.test/a.png"}}]}]}]}),
            json!({"messages":[{"content":[{"type":"tool_use","input":{"context":"plain"},"file_id":"private"}]}]}),
            json!({"messages":[{"content":[{"type":"tool_use","input":{"context":"plain"}},{"type":"image"}]}]}),
            json!({"input":[{"type":"function_call_output","arguments":{"context":"private"}}]}),
        ] {
            assert!(!assess_request_value(&value).complete, "{value}");
        }
        for native in [
            json!({"functionCall":{"args":{"file_id":"private"}}}),
            json!({"contents":[{"parts":[{"functionResponse":{"args":{"context":"private"}}}]}]}),
            json!({"contents":[{"parts":[{"functionCall":{"name":"lookup","args":{"context":"plain"}},"inlineData":{"mimeType":"image/png","data":"AAAA"}}]}]}),
            json!({"contents":[{"parts":[{"functionCall":{"name":"lookup","args":{"context":"plain"}}},{"fileData":{"fileUri":"https://example.test/a.png"}}]}]}),
        ] {
            for value in [
                native.clone(),
                json!({"model":"gemini-2.5-flash","request":native}),
            ] {
                assert!(!assess_google_request_value(&value).complete, "{value}");
            }
        }
        let nested_request = json!({"request":{"request":{"contents":[{"parts":[{
            "functionCall":{"args":{"context":"private"}}
        }]}]}}});
        assert!(!assess_google_request_value(&nested_request).complete);
    }

    #[test]
    fn null_optional_references_do_not_reject_plain_text_requests() {
        let value = json!({
            "input": "hello", "previous_response_id": null,
            "conversation": null, "encrypted_content": null, "image_url": null,
        });
        let footprint = assess_request_value(&value);
        assert!(footprint.complete);
        assert_eq!(footprint.upper_bound_tokens, serialized_bytes(&value));

        let typed = json!({ "input": [{ "type": "compaction", "encrypted_content": null }] });
        assert!(!assess_request_value(&typed).complete);
    }

    #[test]
    fn empty_structures_are_visible_in_items_and_json_bound() {
        let value = json!({ "input": [] });
        let footprint = assess_request_value(&value);

        assert!(footprint.complete);
        assert_eq!(footprint.prompt_items, 1);
        assert_eq!(footprint.upper_bound_tokens, serialized_bytes(&value));
        assert!(footprint.upper_bound_tokens >= 12);
    }
}
