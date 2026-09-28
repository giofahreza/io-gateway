//! Lossless, provider-aware quota accounting.
//!
//! Values in this module are observations, not billing guesses. In particular,
//! absence never means zero, cache is a subset of input, and reasoning is a
//! subset of output. The ledger may release a reservation only when the
//! observation is final and trustworthy.

use serde_json::{Map, Value};

const PROTOCOL: &str = "_quota_usage_protocol";
const FINAL: &str = "_quota_final_usage";

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct NormalizedUsage {
    pub input_tokens: Option<u64>,
    pub uncached_input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cache_read_tokens: Option<u64>,
    pub cache_write_tokens: Option<u64>,
    pub cache_tokens: Option<u64>,
    pub reasoning_tokens: Option<u64>,
    pub trustworthy_final: bool,
}

fn count(value: &Value, key: &str) -> Option<u64> {
    value.get(key).and_then(Value::as_u64)
}

fn detail(value: &Value, group: &str, key: &str) -> Option<u64> {
    value.get(group).and_then(|value| count(value, key))
}

fn sum(left: Option<u64>, right: Option<u64>) -> Option<u64> {
    left?.checked_add(right?)
}

fn difference(total: Option<u64>, read: Option<u64>, write: Option<u64>) -> Option<u64> {
    total?.checked_sub(read?)?.checked_sub(write?)
}

/// Normalize the *native* usage object, before any client-protocol conversion.
/// Partial/estimated observations may be displayed but must not refund holds.
pub(crate) fn normalize(provider: &str, raw_usage: &Value, final_usage: bool) -> NormalizedUsage {
    let usage = raw_usage.get("_quota_native_usage").unwrap_or(raw_usage);
    let mut normalized = NormalizedUsage {
        trustworthy_final: final_usage
            && raw_usage.get(FINAL).and_then(Value::as_bool) != Some(false)
            && usage.get(FINAL).and_then(Value::as_bool) != Some(false)
            && raw_usage.get("estimated_usage").is_none()
            && usage.get("estimated_usage").is_none()
            && usage.is_object(),
        ..NormalizedUsage::default()
    };
    let protocol = raw_usage
        .get(PROTOCOL)
        .or_else(|| usage.get(PROTOCOL))
        .and_then(Value::as_str);
    let google = protocol == Some("google")
        || (protocol.is_none()
            && (usage.get("promptTokenCount").is_some()
                || usage.get("candidatesTokenCount").is_some()));
    let anthropic = protocol == Some("anthropic")
        || (protocol.is_none()
            && (usage.get("cache_read_input_tokens").is_some()
                || usage.get("cache_creation_input_tokens").is_some()
                || provider == "claude"));

    if google {
        normalized.input_tokens = count(usage, "promptTokenCount");
        normalized.cache_read_tokens = count(usage, "cachedContentTokenCount");
        // Google reports no per-generation cache-write count. Creating an
        // explicit cache is a distinct API operation; absent isn't a zero.
        normalized.cache_write_tokens = count(usage, "cacheWriteTokenCount");
        normalized.reasoning_tokens = count(usage, "thoughtsTokenCount");
        normalized.output_tokens = sum(
            count(usage, "candidatesTokenCount"),
            normalized.reasoning_tokens,
        );
        // The reported total can recover omitted zero-only breakdowns without
        // guessing that an omitted reasoning count was zero.
        if normalized.output_tokens.is_none()
            && count(usage, "toolUsePromptTokenCount").unwrap_or(0) == 0
        {
            normalized.output_tokens = count(usage, "totalTokenCount")
                .and_then(|total| total.checked_sub(normalized.input_tokens?));
        }
        normalized.uncached_input_tokens = normalized
            .input_tokens
            .and_then(|total| total.checked_sub(normalized.cache_read_tokens?));
    } else if anthropic {
        normalized.uncached_input_tokens = count(usage, "input_tokens");
        normalized.cache_read_tokens = count(usage, "cache_read_input_tokens");
        normalized.cache_write_tokens = count(usage, "cache_creation_input_tokens");
        normalized.input_tokens = sum(
            normalized.uncached_input_tokens,
            sum(normalized.cache_read_tokens, normalized.cache_write_tokens),
        );
        normalized.output_tokens = count(usage, "output_tokens");
        normalized.reasoning_tokens = detail(usage, "output_tokens_details", "reasoning_tokens")
            .or_else(|| count(usage, "reasoning_tokens"));
    } else {
        normalized.input_tokens =
            count(usage, "input_tokens").or_else(|| count(usage, "prompt_tokens"));
        if normalized.input_tokens.is_none() {
            normalized.input_tokens = sum(
                count(usage, "prompt_cache_hit_tokens"),
                count(usage, "prompt_cache_miss_tokens"),
            );
        }
        normalized.output_tokens =
            count(usage, "output_tokens").or_else(|| count(usage, "completion_tokens"));
        normalized.cache_read_tokens = detail(usage, "input_tokens_details", "cached_tokens")
            .or_else(|| detail(usage, "prompt_tokens_details", "cached_tokens"))
            .or_else(|| count(usage, "prompt_cache_hit_tokens"));
        normalized.cache_write_tokens = detail(usage, "input_tokens_details", "cache_write_tokens")
            .or_else(|| detail(usage, "prompt_tokens_details", "cache_write_tokens"))
            .or_else(|| count(usage, "cache_write_tokens"));
        normalized.reasoning_tokens = detail(usage, "output_tokens_details", "reasoning_tokens")
            .or_else(|| detail(usage, "completion_tokens_details", "reasoning_tokens"))
            .or_else(|| count(usage, "reasoning_tokens"));
        normalized.uncached_input_tokens = count(usage, "prompt_cache_miss_tokens").or_else(|| {
            difference(
                normalized.input_tokens,
                normalized.cache_read_tokens,
                normalized.cache_write_tokens,
            )
        });
    }
    normalized.cache_tokens = sum(normalized.cache_read_tokens, normalized.cache_write_tokens);
    // A malformed/inconsistent breakdown must never release a reservation.
    if let Some(input) = normalized.input_tokens {
        if [
            normalized.uncached_input_tokens,
            normalized.cache_read_tokens,
            normalized.cache_write_tokens,
        ]
        .into_iter()
        .flatten()
        .any(|part| part > input)
        {
            normalized.trustworthy_final = false;
        }
        if let (Some(uncached), Some(cache)) =
            (normalized.uncached_input_tokens, normalized.cache_tokens)
        {
            if uncached.checked_add(cache) != Some(input) {
                normalized.trustworthy_final = false;
            }
        }
    }
    if let (Some(input), Some(cache)) = (normalized.input_tokens, normalized.cache_tokens) {
        if cache > input {
            normalized.trustworthy_final = false;
        }
    }
    if let (Some(output), Some(reasoning)) = (normalized.output_tokens, normalized.reasoning_tokens)
    {
        if reasoning > output {
            normalized.trustworthy_final = false;
        }
    }
    normalized
}

/// Capture native usage independently of the compatibility response's usage.
/// Only a usage object is retained; no response content or model strings.
pub(crate) fn preserve_native_usage(
    metrics: &mut crate::UsageMetrics,
    response: &Value,
    protocol: &str,
) {
    let response = response.get("response").unwrap_or(response);
    let native = response
        .get("usage")
        .or_else(|| response.get("usageMetadata"));
    let explicitly_partial = response
        .get("status")
        .and_then(Value::as_str)
        .is_some_and(|status| matches!(status, "queued" | "in_progress" | "failed" | "cancelled"))
        || metrics
            .raw_usage
            .as_ref()
            .and_then(|usage| usage.get(FINAL))
            .and_then(Value::as_bool)
            == Some(false);
    metrics.raw_usage = native.filter(|value| value.is_object()).map(|native| {
        let mut native = native.clone();
        native
            .as_object_mut()
            .unwrap()
            .insert(PROTOCOL.to_string(), Value::String(protocol.to_string()));
        if explicitly_partial {
            native
                .as_object_mut()
                .unwrap()
                .insert(FINAL.to_string(), Value::Bool(false));
        }
        native
    });
}

/// Merge cumulative SSE usage: start events carry input/cache and later deltas
/// carry cumulative output. Repeated events must not add the same tokens twice.
pub(crate) fn merge_cumulative_usage(current: &mut Option<Value>, incoming: &Value) {
    let Some(incoming) = incoming.as_object() else {
        return;
    };
    let current = current.get_or_insert_with(|| Value::Object(Map::new()));
    let Some(current) = current.as_object_mut() else {
        return;
    };
    for (key, value) in incoming {
        if let Some(new_count) = value.as_u64() {
            let count = current
                .get(key)
                .and_then(Value::as_u64)
                .unwrap_or(0)
                .max(new_count);
            current.insert(key.clone(), Value::from(count));
        } else if value.is_object() {
            let mut nested = current.remove(key);
            merge_cumulative_usage(&mut nested, value);
            if let Some(nested) = nested {
                current.insert(key.clone(), nested);
            }
        } else if !value.is_null() {
            current.insert(key.clone(), value.clone());
        }
    }
}

pub(crate) fn mark_stream_usage(metrics: &mut crate::UsageMetrics, protocol: &str, complete: bool) {
    if let Some(Value::Object(raw)) = metrics.raw_usage.as_mut() {
        raw.insert(PROTOCOL.to_string(), Value::String(protocol.to_string()));
        raw.insert(FINAL.to_string(), Value::Bool(complete));
    }
}

/// A lossless Chat Completions -> Responses usage projection. Preserve native
/// fields and only introduce aliases when the source value actually exists.
/// This avoids turning omitted usage/cache/reasoning into fabricated zeros.
pub(crate) fn openai_compat_usage(native: &Value) -> Value {
    let mut usage = native.as_object().cloned().unwrap_or_default();
    for (source, destination) in [
        ("prompt_tokens", "input_tokens"),
        ("completion_tokens", "output_tokens"),
        ("prompt_tokens_details", "input_tokens_details"),
        ("completion_tokens_details", "output_tokens_details"),
    ] {
        if !usage.contains_key(destination) {
            if let Some(value) = native.get(source) {
                usage.insert(destination.to_string(), value.clone());
            }
        }
    }
    if !usage.contains_key("total_tokens") {
        if let Some(total) = usage
            .get("input_tokens")
            .and_then(Value::as_u64)
            .and_then(|input| input.checked_add(usage.get("output_tokens")?.as_u64()?))
        {
            usage.insert("total_tokens".to_string(), Value::from(total));
        }
    }
    Value::Object(usage)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PreparedBounds {
    pub input_upper_bound: u64,
    pub input_measurable: bool,
    pub output_upper_bound: Option<u64>,
}

/// Set by the adapter that chooses the upstream endpoint, never inferred from
/// client fields. Different native APIs can ignore each other's cap aliases.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PreparedProtocol {
    ProviderDefault,
    OpenAiResponses,
    ChatCompletions,
    AnthropicMessages,
    GoogleGenerateContent,
}

fn positive(value: &Value, key: &str) -> Option<u64> {
    count(value, key).filter(|count| *count > 0 && *count <= i64::MAX as u64)
}

/// A terminal SSE marker does not turn an earlier partial output counter into
/// a final one. Require an actual output observation on the final usage event.
pub(crate) fn has_output_observation(usage: &Value) -> bool {
    count(usage, "output_tokens")
        .or_else(|| count(usage, "completion_tokens"))
        .is_some()
}

/// Bounds are based on the exact final native payload, never the client body.
/// Unsupported output-cap semantics return `None`, not an invented default.
pub(crate) fn prepared_bounds(
    provider: &str,
    payload: &Value,
) -> Result<PreparedBounds, &'static str> {
    prepared_bounds_for_protocol(provider, payload, PreparedProtocol::ProviderDefault)
}

pub(crate) fn prepared_bounds_for_protocol(
    provider: &str,
    payload: &Value,
    protocol: PreparedProtocol,
) -> Result<PreparedBounds, &'static str> {
    if !payload.is_object() {
        return Err("quota measurement requires a JSON object");
    }
    // Only Google's gateway envelope puts the actual generation payload in
    // `request`. Treating this name specially for native OpenAI/Anthropic
    // payloads lets a client hide a fake small cap in an ignored extension.
    let body = if matches!(provider, "gemini" | "antigravity") {
        payload.get("request").unwrap_or(payload)
    } else {
        payload
    };
    let generation = body.get("generationConfig").unwrap_or(&Value::Null);
    let multiple = payload
        .get("n")
        .or_else(|| body.get("n"))
        .or_else(|| generation.get("candidateCount"))
        .is_some_and(|value| value.as_u64() != Some(1));
    let assessment = if matches!(provider, "gemini" | "antigravity") {
        crate::input_assessment::assess_google_request_value(payload)
    } else {
        crate::input_assessment::assess_request_value(payload)
    };
    let model = payload
        .get("model")
        .or_else(|| body.get("model"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let non_text_output = generation
        .get("responseModalities")
        .and_then(Value::as_array)
        .is_some_and(|modalities| {
            modalities
                .iter()
                .any(|value| value.as_str() != Some("TEXT"))
        })
        || body
            .get("modalities")
            .and_then(Value::as_array)
            .is_some_and(|modalities| {
                modalities
                    .iter()
                    .any(|value| value.as_str() != Some("text"))
            });
    let hosted_google_tools = body
        .get("tools")
        .and_then(Value::as_array)
        .is_some_and(|tools| {
            tools.iter().any(|tool| {
                tool.as_object().is_some_and(|tool| {
                    [
                        "googleSearch",
                        "googleSearchRetrieval",
                        "codeExecution",
                        "urlContext",
                    ]
                    .iter()
                    .any(|key| tool.contains_key(*key))
                })
            })
        });
    let asynchronous = ["background", "async"].iter().any(|key| {
        payload
            .get(*key)
            .or_else(|| body.get(*key))
            .is_some_and(|value| !value.is_null() && value.as_bool() != Some(false))
    });
    let output = if multiple
        || non_text_output
        || hosted_google_tools
        || asynchronous
        || assessment.has_unmeasurable_remote_context()
    {
        None
    } else {
        match provider {
            "codex" | "grok" => None,
            "claude" if model.starts_with("claude-") => positive(body, "max_tokens"),
            "deepseek" if model.starts_with("deepseek-") => positive(body, "max_tokens"),
            "gemini" | "antigravity" if model.starts_with("gemini-2.5-") => {
                // Reserve candidate output plus the separately capped thought
                // budget. Dynamic thinking has no locally proven upper bound.
                let thinking = generation
                    .get("thinkingConfig")
                    .and_then(|value| count(value, "thinkingBudget"));
                positive(generation, "maxOutputTokens")
                    .and_then(|output| output.checked_add(thinking?))
                    .filter(|output| *output <= i64::MAX as u64)
            }
            "copilot" if known_copilot_output_model(model) => match protocol {
                PreparedProtocol::OpenAiResponses => positive(body, "max_output_tokens"),
                PreparedProtocol::ChatCompletions => positive(body, "max_completion_tokens")
                    .or_else(|| {
                        known_nonreasoning_copilot_model(model)
                            .then(|| positive(body, "max_tokens"))
                            .flatten()
                    }),
                _ => None,
            },
            // These adapters send Chat Completions max_tokens. Only the
            // explicitly disabled reasoning mode gives a defensible total
            // output ceiling across the currently supported models.
            "glm" if model.to_ascii_lowercase().starts_with("glm-") => {
                let disabled = body
                    .get("thinking")
                    .and_then(|value| value.get("type"))
                    .and_then(Value::as_str)
                    == Some("disabled");
                disabled.then(|| positive(body, "max_tokens")).flatten()
            }
            "minimax"
                if model.eq_ignore_ascii_case("MiniMax-M3")
                    && matches!(
                        protocol,
                        PreparedProtocol::ChatCompletions | PreparedProtocol::AnthropicMessages
                    ) =>
            {
                let disabled = body
                    .get("thinking")
                    .and_then(|value| value.get("type"))
                    .and_then(Value::as_str)
                    == Some("disabled");
                disabled.then(|| positive(body, "max_tokens")).flatten()
            }
            "qwen" if model.to_ascii_lowercase().starts_with("qwen3") => {
                (body.get("enable_thinking").and_then(Value::as_bool) == Some(false))
                    .then(|| positive(body, "max_tokens"))
                    .flatten()
            }
            _ => None,
        }
    };
    Ok(PreparedBounds {
        input_upper_bound: assessment.upper_bound_tokens,
        input_measurable: assessment.complete && !multiple && !hosted_google_tools && !asynchronous,
        output_upper_bound: output,
    })
}

fn known_nonreasoning_copilot_model(model: &str) -> bool {
    let model = model.to_ascii_lowercase();
    (model.starts_with("gpt-3.5-turbo")
        || model.starts_with("gpt-4.1")
        || model.starts_with("gpt-4o")
        || model == "gpt-4-o-preview")
        && !model.contains("thinking")
        && !model.contains("reason")
        && !model.contains("audio")
        && !model.contains("realtime")
}

fn known_copilot_output_model(model: &str) -> bool {
    known_nonreasoning_copilot_model(model) || known_reasoning_copilot_model(model)
}

pub(crate) fn known_reasoning_copilot_model(model: &str) -> bool {
    (model.starts_with("gpt-5")
        || model.starts_with("o1")
        || model.starts_with("o3")
        || model.starts_with("o4"))
        && !model.contains("audio")
        && !model.contains("realtime")
        && !model.contains("image")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn google_native_schemas_do_not_become_media_or_retained_context() {
        let schema = json!({
            "type": "object",
            "properties": {
                "image_url": {"type": "string"},
                "file_id": {"type": "string"},
                "context": {"type": "string"}
            }
        });
        let native = json!({
            "model": "gemini-2.5-flash",
            "contents": [{"role":"user", "parts":[{"text":"hello"}]}],
            "tools": [{"functionDeclarations":[{"name":"describe", "parameters":schema}]}],
            "generationConfig": {"maxOutputTokens":20,"thinkingConfig":{"thinkingBudget":10},"responseSchema":schema}
        });
        for provider in ["gemini", "antigravity"] {
            for payload in [
                native.clone(),
                json!({"model":"gemini-2.5-flash", "request":native}),
            ] {
                let bounds = prepared_bounds(provider, &payload).unwrap();
                assert!(bounds.input_measurable, "{provider}: {payload}");
                assert_eq!(bounds.output_upper_bound, Some(30));
                assert_eq!(bounds.input_upper_bound, payload.to_string().len() as u64);
            }
        }
    }

    #[test]
    fn google_schema_exemptions_do_not_hide_non_schema_media_or_context() {
        for hidden in [
            json!({"request":{"tools":[{"functionDeclarations":[{"parameters":{"type":"object","properties":{"image_url":{"type":"string"}}}}]}]}}),
            json!({"parameters":{"type":"object","properties":{"file_id":{"type":"string"}}}}),
            json!({"contents":[{"parts":[{"inlineData":{"mimeType":"image/png","data":"AAAA"},"parameters":{"type":"object"}}]}]}),
            json!({"tools":[{"functionDeclarations":[{"name":"describe","parameters":{"type":"object"},"context":"retained"}]}]}),
        ] {
            for provider in ["gemini", "antigravity"] {
                let payload = json!({"model":"gemini-2.5-flash", "request":hidden});
                assert!(
                    !prepared_bounds(provider, &payload)
                        .unwrap()
                        .input_measurable,
                    "{provider}: {payload}"
                );
            }
        }
    }

    #[test]
    fn google_native_response_array_schema_accepts_uppercase_types() {
        let payload = json!({
            "model":"gemini-2.5-flash",
            "request":{
                "contents":[{"parts":[{"text":"hello"}]}],
                "generationConfig":{"responseSchema":{
                    "type":"ARRAY", "items":{
                        "type":"OBJECT", "properties":{"image_url":{"type":"STRING"}}
                    }
                }}
            }
        });
        for provider in ["gemini", "antigravity"] {
            assert!(
                prepared_bounds(provider, &payload)
                    .unwrap()
                    .input_measurable
            );
        }
    }

    #[test]
    fn openai_cache_and_reasoning_are_subsets() {
        let usage = normalize(
            "copilot",
            &json!({
                "input_tokens":100,"output_tokens":30,
                "input_tokens_details":{"cached_tokens":40,"cache_write_tokens":10},
                "output_tokens_details":{"reasoning_tokens":20}
            }),
            true,
        );
        assert_eq!(usage.input_tokens, Some(100));
        assert_eq!(usage.uncached_input_tokens, Some(50));
        assert_eq!(usage.cache_tokens, Some(50));
        assert_eq!(usage.output_tokens, Some(30));
        assert_eq!(usage.reasoning_tokens, Some(20));
        assert!(usage.trustworthy_final);
    }

    #[test]
    fn anthropic_input_adds_all_three_categories() {
        let usage = normalize(
            "claude",
            &json!({"input_tokens":20,"cache_read_input_tokens":60,"cache_creation_input_tokens":10,"output_tokens":15}),
            true,
        );
        assert_eq!(usage.input_tokens, Some(90));
        assert_eq!(usage.uncached_input_tokens, Some(20));
        assert_eq!(usage.cache_tokens, Some(70));
        assert_eq!(usage.output_tokens, Some(15));
    }

    #[test]
    fn google_thoughts_add_to_candidates_not_to_input() {
        let usage = normalize(
            "gemini",
            &json!({"promptTokenCount":100,"cachedContentTokenCount":40,"candidatesTokenCount":20,"thoughtsTokenCount":30,"totalTokenCount":150}),
            true,
        );
        assert_eq!(usage.input_tokens, Some(100));
        assert_eq!(usage.output_tokens, Some(50));
        assert_eq!(usage.uncached_input_tokens, Some(60));
        assert_eq!(usage.cache_read_tokens, Some(40));
        assert_eq!(usage.cache_write_tokens, None);
        assert_eq!(usage.cache_tokens, None);
    }

    #[test]
    fn deepseek_reports_hit_and_miss_without_guessing_writes() {
        let usage = normalize(
            "deepseek",
            &json!({"prompt_tokens":100,"prompt_cache_hit_tokens":60,"prompt_cache_miss_tokens":40,"completion_tokens":10}),
            true,
        );
        assert_eq!(usage.uncached_input_tokens, Some(40));
        assert_eq!(usage.cache_read_tokens, Some(60));
        assert_eq!(usage.cache_write_tokens, None);
    }

    #[test]
    fn absent_zero_partial_and_estimated_are_distinct() {
        assert_eq!(normalize("copilot", &json!({}), true).input_tokens, None);
        assert_eq!(
            normalize("copilot", &json!({"input_tokens":0}), true).input_tokens,
            Some(0)
        );
        assert!(!normalize("copilot", &json!({"input_tokens":8}), false).trustworthy_final);
        assert!(
            !normalize(
                "qwen",
                &json!({"input_tokens":8,"estimated_usage":{}}),
                true
            )
            .trustworthy_final
        );
        assert!(
            !normalize(
                "claude",
                &json!({"output_tokens":8,"_quota_final_usage":false}),
                true
            )
            .trustworthy_final
        );
        assert_eq!(
            normalize("claude", &json!({"input_tokens":8}), true).input_tokens,
            None
        );
    }

    #[test]
    fn cumulative_stream_usage_preserves_start_fields_and_does_not_double_count() {
        let mut usage = None;
        merge_cumulative_usage(
            &mut usage,
            &json!({"input_tokens":5,"cache_read_input_tokens":7,"cache_creation_input_tokens":3,"output_tokens":0}),
        );
        merge_cumulative_usage(&mut usage, &json!({"output_tokens":9}));
        merge_cumulative_usage(&mut usage, &json!({"output_tokens":9}));
        let normalized = normalize("claude", usage.as_ref().unwrap(), true);
        assert_eq!(normalized.input_tokens, Some(15));
        assert_eq!(normalized.output_tokens, Some(9));
    }

    #[test]
    fn native_usage_is_preserved_and_absence_does_not_keep_synthesized_zero() {
        let mut metrics = crate::UsageMetrics::default();
        preserve_native_usage(
            &mut metrics,
            &json!({"response":{"usageMetadata":{"promptTokenCount":8}}}),
            "google",
        );
        assert_eq!(metrics.raw_usage.as_ref().unwrap()["promptTokenCount"], 8);
        preserve_native_usage(&mut metrics, &json!({"content":[]}), "anthropic");
        assert!(metrics.raw_usage.is_none());
        preserve_native_usage(
            &mut metrics,
            &json!({"status":"in_progress","usage":{"input_tokens":5,"output_tokens":0}}),
            "openai",
        );
        assert!(!normalize("copilot", metrics.raw_usage.as_ref().unwrap(), true).trustworthy_final);
    }

    #[test]
    fn output_capability_is_based_on_the_final_native_payload() {
        assert_eq!(
            prepared_bounds(
                "claude",
                &json!({"model":"claude-sonnet-4","max_tokens":20})
            )
            .unwrap()
            .output_upper_bound,
            Some(20)
        );
        assert_eq!(
            prepared_bounds("codex", &json!({"model":"gpt-5","max_output_tokens":20}))
                .unwrap()
                .output_upper_bound,
            None
        );
        assert_eq!(
            prepared_bounds("grok", &json!({"model":"grok-4","max_output_tokens":20}))
                .unwrap()
                .output_upper_bound,
            None
        );
        assert_eq!(
            prepared_bounds("copilot", &json!({"model":"gpt-5","max_tokens":20}))
                .unwrap()
                .output_upper_bound,
            None
        );
        assert_eq!(
            prepared_bounds_for_protocol(
                "copilot",
                &json!({"model":"gpt-5","max_completion_tokens":20}),
                PreparedProtocol::ChatCompletions
            )
            .unwrap()
            .output_upper_bound,
            Some(20)
        );
        assert_eq!(prepared_bounds("gemini", &json!({"model":"gemini-2.5-flash","request":{"generationConfig":{"maxOutputTokens":20,"thinkingConfig":{"thinkingBudget":10}}}})).unwrap().output_upper_bound, Some(30));
        assert_eq!(
            prepared_bounds(
                "gemini",
                &json!({"request":{"generationConfig":{"maxOutputTokens":20}}})
            )
            .unwrap()
            .output_upper_bound,
            None
        );
    }

    #[test]
    fn multiplicity_remote_context_and_invalid_output_are_not_zero_cost() {
        let multiple = prepared_bounds("copilot", &json!({"n":2,"max_output_tokens":20})).unwrap();
        assert!(!multiple.input_measurable);
        assert_eq!(multiple.output_upper_bound, None);
        let context = prepared_bounds(
            "copilot",
            &json!({"previous_response_id":"retained","max_output_tokens":20}),
        )
        .unwrap();
        assert!(!context.input_measurable);
        assert_eq!(context.output_upper_bound, None);
        assert_eq!(
            prepared_bounds("claude", &json!({"model":"claude-sonnet-4","max_tokens":0}))
                .unwrap()
                .output_upper_bound,
            None
        );
    }

    #[test]
    fn malformed_breakdowns_and_overflow_cannot_refund_allowance() {
        assert!(!normalize("copilot", &json!({"input_tokens":2,"input_tokens_details":{"cached_tokens":8,"cache_write_tokens":0}}), true).trustworthy_final);
        assert!(
            !normalize(
                "copilot",
                &json!({"output_tokens":2,"output_tokens_details":{"reasoning_tokens":8}}),
                true
            )
            .trustworthy_final
        );
        assert_eq!(normalize("claude", &json!({"input_tokens":u64::MAX,"cache_read_input_tokens":1,"cache_creation_input_tokens":1}), true).input_tokens, None);
        assert!(
            !normalize(
                "copilot",
                &json!({"input_tokens":2,"input_tokens_details":{"cached_tokens":8}}),
                true
            )
            .trustworthy_final
        );
        assert!(!normalize("deepseek", &json!({"prompt_tokens":10,"prompt_cache_hit_tokens":7,"prompt_cache_miss_tokens":9,"cache_write_tokens":0}), true).trustworthy_final);
    }

    #[test]
    fn strict_output_capabilities_reject_guessed_models_and_dynamic_thinking() {
        for (provider, payload) in [
            (
                "copilot",
                json!({"model":"unknown-model","max_output_tokens":20}),
            ),
            (
                "copilot",
                json!({"model":"gpt-4o-audio-preview","max_completion_tokens":20}),
            ),
            ("claude", json!({"model":"unknown-model","max_tokens":20})),
            (
                "glm",
                json!({"model":"glm-4.5","max_tokens":20,"thinking":{"type":"enabled"}}),
            ),
            ("qwen", json!({"model":"qwen3-coder-plus","max_tokens":20})),
            (
                "minimax",
                json!({"model":"MiniMax-M2.5","max_tokens":20,"thinking":{"type":"disabled"}}),
            ),
            (
                "gemini",
                json!({"model":"gemini-2.5-flash","request":{"generationConfig":{"maxOutputTokens":20,"thinkingConfig":{"thinkingBudget":-1}}}}),
            ),
        ] {
            assert_eq!(
                prepared_bounds(provider, &payload)
                    .unwrap()
                    .output_upper_bound,
                None,
                "{provider}"
            );
        }
        for (provider, payload) in [
            ("copilot", json!({"model":"gpt-3.5-turbo","max_tokens":20})),
            (
                "glm",
                json!({"model":"glm-4.5","max_tokens":20,"thinking":{"type":"disabled"}}),
            ),
            (
                "qwen",
                json!({"model":"qwen3-coder-plus","max_tokens":20,"enable_thinking":false}),
            ),
            (
                "minimax",
                json!({"model":"MiniMax-M3","max_tokens":20,"thinking":{"type":"disabled"}}),
            ),
        ] {
            assert_eq!(
                prepared_bounds_for_protocol(provider, &payload, PreparedProtocol::ChatCompletions)
                    .unwrap()
                    .output_upper_bound,
                Some(20),
                "{provider}"
            );
        }
    }

    #[test]
    fn compatibility_usage_never_fabricates_missing_counts() {
        assert_eq!(openai_compat_usage(&json!({})), json!({}));
        let native = json!({"prompt_tokens":100,"completion_tokens":20,"prompt_tokens_details":{"cached_tokens":5,"cache_write_tokens":3},"completion_tokens_details":{"reasoning_tokens":8}});
        let compat = openai_compat_usage(&native);
        assert_eq!(
            normalize("copilot", &native, true),
            normalize("copilot", &compat, true)
        );
        assert_eq!(compat["total_tokens"], 120);
    }

    #[test]
    fn prepared_input_covers_every_final_serialized_byte() {
        let payload = json!({"model":"glm-4.5","max_tokens":5,"messages":[{"role":"system","content":"injected 中文"}],"tools":[{"type":"function","function":{"name":"f","parameters":{"type":"object","properties":{"text":{"type":"string"}}}}}]});
        let bounds = prepared_bounds("glm", &payload).unwrap();
        assert!(bounds.input_measurable);
        assert_eq!(
            bounds.input_upper_bound,
            serde_json::to_vec(&payload).unwrap().len() as u64
        );
    }

    #[test]
    fn ignored_request_extension_cannot_override_native_output_ceiling() {
        let payload =
            json!({"model":"claude-sonnet-4","max_tokens":1000,"request":{"max_tokens":1}});
        assert_eq!(
            prepared_bounds("claude", &payload)
                .unwrap()
                .output_upper_bound,
            Some(1000)
        );
        let payload = json!({"model":"glm-4.5","max_tokens":1000,"thinking":{"type":"disabled"},"request":{"max_tokens":1,"thinking":{"type":"disabled"}}});
        assert_eq!(
            prepared_bounds("glm", &payload).unwrap().output_upper_bound,
            Some(1000)
        );
        let payload =
            json!({"model":"gpt-4.1","max_output_tokens":1000,"request":{"max_output_tokens":1}});
        assert_eq!(
            prepared_bounds_for_protocol("copilot", &payload, PreparedProtocol::OpenAiResponses)
                .unwrap()
                .output_upper_bound,
            Some(1000)
        );
    }

    #[test]
    fn asynchronous_generation_has_no_safe_synchronous_token_settlement() {
        let bounds = prepared_bounds(
            "copilot",
            &json!({"model":"gpt-4.1","max_output_tokens":20,"background":true}),
        )
        .unwrap();
        assert!(!bounds.input_measurable);
        assert_eq!(bounds.output_upper_bound, None);
    }

    #[test]
    fn native_protocol_prevents_ignored_cap_alias_bypasses() {
        let payload = json!({"model":"gpt-4.1","max_tokens":1,"max_output_tokens":1000});
        assert_eq!(
            prepared_bounds_for_protocol("copilot", &payload, PreparedProtocol::OpenAiResponses)
                .unwrap()
                .output_upper_bound,
            Some(1000)
        );
        assert_eq!(
            prepared_bounds_for_protocol("copilot", &payload, PreparedProtocol::ChatCompletions)
                .unwrap()
                .output_upper_bound,
            Some(1)
        );
        assert_eq!(
            prepared_bounds("copilot", &payload)
                .unwrap()
                .output_upper_bound,
            None
        );
        let payload = json!({"model":"MiniMax-M3","max_tokens":1,"max_output_tokens":1000,"thinking":{"type":"disabled"}});
        assert_eq!(
            prepared_bounds_for_protocol("minimax", &payload, PreparedProtocol::OpenAiResponses)
                .unwrap()
                .output_upper_bound,
            None
        );
        assert_eq!(
            prepared_bounds_for_protocol("minimax", &payload, PreparedProtocol::ChatCompletions)
                .unwrap()
                .output_upper_bound,
            Some(1)
        );
        assert_eq!(
            prepared_bounds("minimax", &payload)
                .unwrap()
                .output_upper_bound,
            None
        );
    }

    #[test]
    fn explicit_native_protocol_takes_priority_over_provider_or_extension_names() {
        let usage = normalize(
            "claude",
            &json!({"_quota_usage_protocol":"openai","input_tokens":20,"output_tokens":5,"cache_read_input_tokens":10,"cache_creation_input_tokens":3}),
            true,
        );
        assert_eq!(usage.input_tokens, Some(20));
        assert_eq!(usage.output_tokens, Some(5));
        assert_eq!(usage.cache_read_tokens, None);
        let usage = normalize(
            "glm",
            &json!({"_quota_usage_protocol":"anthropic","input_tokens":20,"cache_read_input_tokens":10,"cache_creation_input_tokens":3,"output_tokens":5}),
            true,
        );
        assert_eq!(usage.input_tokens, Some(33));
    }
}
