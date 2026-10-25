use super::{ChatRequest, Protocol, Settings};
use crate::engine::App;
use serde_json::{json, Map, Value};
use std::borrow::Cow;
use std::collections::BTreeMap;
use std::sync::{atomic::AtomicBool, Arc};

/// Persist on transformed text parts, and propagate to their normalized
/// assistant message when rebuilding history. Never send this field on the wire.
pub const TEXT_TRANSFORMED: &str = "ocminiTextTransformed";

/// Prepare a logical request once, before the transport retry loop. Only public
/// message fields are offered to hooks; replay metadata stays in the request.
pub async fn prepare_hooks(
    app: &App,
    context: &Value,
    request: &mut ChatRequest,
    cancel: Option<&Arc<AtomicBool>>,
) -> Result<(), String> {
    if app.hooks.matches("model.context.transform", context) {
        let public: Vec<_> = request.messages.iter().map(public_message).collect();
        let input = json!({"system":request.system,"messages":public});
        let mut result = app
            .hooks
            .run("model.context.transform", context, input, cancel)
            .await?
            .input;
        let messages = result["messages"]
            .as_array()
            .ok_or("model.context.transform messages must be an array")?;
        apply_messages(&mut request.messages, messages)?;
        request.system = take_string(&mut result, "system", "model.context.transform")?;
    }
    if app.hooks.matches("model.params.transform", context) {
        // These are request-local generation options, never resolved provider
        // settings (which may contain credentials and endpoints).
        validate_options(&request.options)?;
        let input = json!({
            "options":if request.options.is_null() { json!({}) } else { request.options.take() },
            "variant":request.variant.take()
        });
        let mut result = app
            .hooks
            .run("model.params.transform", context, input, cancel)
            .await?
            .input;
        let options = result
            .as_object_mut()
            .and_then(|map| map.remove("options"))
            .ok_or("model.params.transform requires options")?;
        if !options.is_object() {
            return Err("model.params.transform options must be an object".into());
        }
        validate_options(&options)?;
        request.options = options;
        request.variant = match &result["variant"] {
            Value::Null => None,
            Value::String(value) if !value.is_empty() => Some(value.clone()),
            _ => {
                return Err(
                    "model.params.transform variant must be a nonempty string or null".into(),
                )
            }
        };
    }
    if app.hooks.matches("model.headers.transform", context) {
        let input = json!({"headers":safe_headers(&request.headers)?});
        let result = app
            .hooks
            .run("model.headers.transform", context, input, cancel)
            .await?
            .input;
        let map = result["headers"]
            .as_object()
            .ok_or("model.headers.transform headers must be an object")?;
        let mut headers = BTreeMap::new();
        for (name, value) in map {
            let value = value
                .as_str()
                .ok_or("model.headers.transform header values must be strings")?;
            headers.insert(name.clone(), value.into());
        }
        request.headers = safe_headers(&headers)?;
    }
    Ok(())
}

/// Transform one completed public text part. The caller persists the returned
/// text and TEXT_TRANSFORMED when it differs, before publishing the full update.
pub async fn complete_text(
    app: &App,
    context: &Value,
    text: String,
    cancel: Option<&Arc<AtomicBool>>,
) -> Result<String, String> {
    if !app.hooks.matches("message.text.complete", context) {
        return Ok(text);
    }
    let mut input = Map::new();
    input.insert("text".into(), Value::String(text));
    let mut result = app
        .hooks
        .run(
            "message.text.complete",
            context,
            Value::Object(input),
            cancel,
        )
        .await?
        .input;
    take_string(&mut result, "text", "message.text.complete")
}

fn take_string(value: &mut Value, key: &str, hook: &str) -> Result<String, String> {
    match value.as_object_mut().and_then(|map| map.remove(key)) {
        Some(Value::String(text)) => Ok(text),
        _ => Err(format!("{hook} {key} must be a string")),
    }
}

fn public_message(message: &Value) -> Value {
    let mut public = Map::new();
    for key in [
        "id",
        "messageID",
        "role",
        "name",
        "content",
        "tool_calls",
        "tool_call_id",
        "is_error",
    ] {
        if let Some(value) = message.get(key) {
            public.insert(key.into(), value.clone());
        }
    }
    Value::Object(public)
}

fn apply_messages(original: &mut [Value], transformed: &[Value]) -> Result<(), String> {
    if original.len() != transformed.len() {
        return Err("model.context.transform cannot add, remove, or reorder messages".into());
    }
    // Validate the whole replacement before changing any message. All identity,
    // call arguments/linkage, attachments, and opaque metadata remain original.
    for (before, after) in original.iter().zip(transformed) {
        let public = public_message(before);
        let expected = public.as_object().unwrap();
        let actual = after
            .as_object()
            .ok_or("model.context.transform messages must be objects")?;
        if expected.len() != actual.len()
            || actual.keys().any(|key| !expected.contains_key(key))
            || expected
                .iter()
                .any(|(key, value)| key != "content" && actual.get(key) != Some(value))
        {
            return Err(
                "model.context.transform cannot change message identity, metadata, or tool linkage"
                    .into(),
            );
        }
        validate_content(&before["content"], &after["content"])?;
    }
    for (before, after) in original.iter_mut().zip(transformed) {
        if before.get("content") != after.get("content") {
            before["content"] = after["content"].clone();
            if before["role"] == "assistant" {
                before[TEXT_TRANSFORMED] = json!(true);
            }
        }
    }
    Ok(())
}

fn validate_content(before: &Value, after: &Value) -> Result<(), String> {
    if before == after || (before.is_string() && after.is_string()) {
        return Ok(());
    }
    if before.is_null() && after.is_string() {
        return Ok(());
    }
    if let (Some(before), Some(after)) = (before.as_array(), after.as_array()) {
        if before.len() == after.len() {
            for (before, after) in before.iter().zip(after) {
                if before == after {
                    continue;
                }
                let mut expected = before.clone();
                if ["text", "input_text", "output_text"]
                    .contains(&before["type"].as_str().unwrap_or_default())
                    && after["text"].is_string()
                {
                    expected["text"] = after["text"].clone();
                    if &expected == after {
                        continue;
                    }
                }
                return Err(
                    "model.context.transform may change only public text, preserving attachments"
                        .into(),
                );
            }
            return Ok(());
        }
    }
    Err("model.context.transform must preserve message content shape and attachments".into())
}

/// Check and normalize request-local headers both before hooks and at the final
/// adapter boundary. Configured auth/scope headers are never included here.
pub(super) fn safe_headers(
    headers: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, String>, String> {
    use reqwest::header::{HeaderName, HeaderValue};
    let mut safe = BTreeMap::new();
    for (name, value) in headers {
        let name = name.to_ascii_lowercase();
        if name.contains("auth")
            || name.contains("cookie")
            || name.contains("token")
            || name.contains("secret")
            || name.contains("key")
            || name.starts_with("proxy-")
            || name.starts_with("content-")
            || [
                "host",
                "connection",
                "transfer-encoding",
                "trailer",
                "te",
                "upgrade",
                "chatgpt-account-id",
                "openai-organization",
                "openai-project",
            ]
            .contains(&name.as_str())
        {
            return Err(format!(
                "model.headers.transform cannot set protected header {name}"
            ));
        }
        HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| "model.headers.transform has an invalid header name".to_string())?;
        HeaderValue::from_str(value).map_err(|_| {
            format!("model.headers.transform has an invalid value for header {name}")
        })?;
        if safe.insert(name.clone(), value.clone()).is_some() {
            return Err(format!(
                "model.headers.transform repeats header {name} with different casing"
            ));
        }
    }
    Ok(safe)
}

const COMMON: &[&str] = &[
    "temperature",
    "topP",
    "maxTokens",
    "maxOutputTokens",
    "reasoningEffort",
    "tool_choice",
    "serviceTier",
];
const CHAT: &[&str] = &[
    "topK",
    "frequencyPenalty",
    "presencePenalty",
    "seed",
    "stopSequences",
    "parallelToolCalls",
    "user",
    "reasoning",
    "response_format",
    "enable_thinking",
    "chat_template_kwargs",
    "min_p",
    "repetition_penalty",
];
const RESPONSES: &[&str] = &[
    "parallelToolCalls",
    "user",
    "reasoning",
    "reasoningSummary",
    "textVerbosity",
    "text",
    "metadata",
    "include",
    "store",
];
const ANTHROPIC: &[&str] = &[
    "topK",
    "stopSequences",
    "thinking",
    "output_config",
    "metadata",
];

pub(super) fn validate_options(options: &Value) -> Result<(), String> {
    if options.is_null() {
        return Ok(());
    }
    let options = options
        .as_object()
        .ok_or("model generation options must be an object")?;
    for (key, value) in options {
        if !COMMON.contains(&key.as_str())
            && !CHAT.contains(&key.as_str())
            && !RESPONSES.contains(&key.as_str())
            && !ANTHROPIC.contains(&key.as_str())
        {
            return Err(format!("unsupported model generation option {key}; provider/model, credentials, transport, and limits are immutable"));
        }
        let valid = match key.as_str() {
            "temperature" => value.as_f64().is_some_and(|v| (0.0..=2.0).contains(&v)),
            "topP" | "min_p" => value.as_f64().is_some_and(|v| (0.0..=1.0).contains(&v)),
            "frequencyPenalty" | "presencePenalty" => {
                value.as_f64().is_some_and(|v| (-2.0..=2.0).contains(&v))
            }
            "maxTokens" | "maxOutputTokens" | "topK" => value.as_u64().is_some_and(|v| v > 0),
            "seed" => value.as_i64().is_some(),
            "parallelToolCalls" | "store" | "enable_thinking" => value.is_boolean(),
            "stopSequences" | "include" => value
                .as_array()
                .is_some_and(|v| v.iter().all(Value::is_string)),
            "reasoningEffort" => value.as_str().is_some_and(|v| {
                [
                    "none", "minimal", "low", "medium", "high", "xhigh", "max", "ultra",
                ]
                .contains(&v)
            }),
            "textVerbosity" => value
                .as_str()
                .is_some_and(|v| ["low", "medium", "high"].contains(&v)),
            "reasoningSummary" => value
                .as_str()
                .is_some_and(|v| ["auto", "concise", "detailed"].contains(&v)),
            "serviceTier" | "user" => value.as_str().is_some_and(|v| !v.is_empty()),
            "tool_choice" => value.is_object() || value.is_string(),
            "repetition_penalty" => value.as_f64().is_some_and(|v| v > 0.0),
            _ => value.is_object(),
        };
        if !valid {
            return Err(format!("invalid model generation option {key}"));
        }
    }
    if options.contains_key("maxTokens") && options.contains_key("maxOutputTokens") {
        return Err("set only one of maxTokens and maxOutputTokens".into());
    }
    if options.contains_key("reasoningEffort") && options.contains_key("thinking") {
        return Err("set only one of reasoningEffort and thinking".into());
    }
    Ok(())
}

pub(super) fn validate_protocol_options(
    options: &Value,
    protocol: Protocol,
    codex: bool,
) -> Result<(), String> {
    let Some(options) = options.as_object() else {
        return Ok(());
    };
    let supported = match protocol {
        Protocol::Chat => CHAT,
        Protocol::Responses => RESPONSES,
        Protocol::Anthropic => ANTHROPIC,
    };
    for key in options.keys() {
        if !COMMON.contains(&key.as_str()) && !supported.contains(&key.as_str())
            || protocol == Protocol::Anthropic && key == "serviceTier"
            || codex && ["maxTokens", "maxOutputTokens", "store"].contains(&key.as_str())
        {
            return Err(format!(
                "model generation option {key} is unsupported by {protocol:?}{}",
                if codex { " (Codex OAuth)" } else { "" }
            ));
        }
    }
    Ok(())
}

/// Validate normalized request-local options before transport. The adapter also
/// runs this validation against its resolved model output limit automatically.
pub fn validate_generation_options(
    options: &Value,
    protocol: Protocol,
    max_output: Option<u64>,
) -> Result<(), String> {
    validate_options(options)?;
    validate_protocol_options(options, protocol, false)?;
    validate_limits(options, options, protocol, max_output)
}

pub(super) fn validate_limits(
    request: &Value,
    options: &Value,
    protocol: Protocol,
    limit: Option<u64>,
) -> Result<(), String> {
    if let Some(max) = options["maxTokens"]
        .as_u64()
        .or_else(|| options["maxOutputTokens"].as_u64())
    {
        if limit.is_some_and(|limit| max > limit) {
            return Err(format!(
                "model generation output token limit {max} exceeds model limit {}",
                limit.unwrap()
            ));
        }
    }
    if protocol == Protocol::Anthropic {
        if request["temperature"].as_f64().is_some_and(|v| v > 1.0) {
            return Err("Anthropic temperature must be between 0 and 1".into());
        }
        let max = options["maxTokens"]
            .as_u64()
            .or_else(|| options["maxOutputTokens"].as_u64())
            .or(limit)
            .unwrap_or(16_000);
        let budget = options["thinking"]["budget_tokens"]
            .as_u64()
            .or_else(|| options["thinking"]["budgetTokens"].as_u64());
        if request.get("thinking").is_some()
            && options["thinking"]["type"] == "enabled"
            && !budget.is_some_and(|budget| budget >= 1024 && budget < max)
        {
            return Err("Anthropic thinking budget must be at least 1024 and smaller than max output tokens".into());
        }
        if request.get("temperature").is_some()
            && options["thinking"]["type"] == "enabled"
            && request["temperature"].as_f64() != Some(1.0)
        {
            return Err("Anthropic thinking requires temperature 1".into());
        }
    }
    Ok(())
}

/// Request-local values are authoritative over configured raw body overlays.
/// Running this after the overlay also prevents accepted options from silently
/// disappearing when a provider preset contains the same wire parameter.
pub(super) fn apply_request_options(
    settings: &Settings,
    request: &ChatRequest,
    protocol: Protocol,
    body: &mut Value,
) -> Result<(), String> {
    validate_generation_options(&request.options, protocol, settings.max_output)?;
    validate_protocol_options(&request.options, protocol, settings.codex)?;
    let Some(options) = request.options.as_object() else {
        return Ok(());
    };
    let normalized = ["reasoningEffort", "reasoningSummary", "textVerbosity"];
    for (key, value) in options
        .iter()
        .filter(|(key, _)| !normalized.contains(&key.as_str()))
        .chain(
            options
                .iter()
                .filter(|(key, _)| normalized.contains(&key.as_str())),
        )
    {
        let wire = match key.as_str() {
            "topP" => "top_p",
            "topK" => "top_k",
            "frequencyPenalty" => "frequency_penalty",
            "presencePenalty" => "presence_penalty",
            "parallelToolCalls" => "parallel_tool_calls",
            "serviceTier" => "service_tier",
            "stopSequences" if protocol == Protocol::Chat => "stop",
            "stopSequences" => "stop_sequences",
            "maxTokens" | "maxOutputTokens" => match protocol {
                Protocol::Chat if settings.provider_id == "openai" => "max_completion_tokens",
                Protocol::Responses => "max_output_tokens",
                _ => "max_tokens",
            },
            "reasoningEffort" => {
                if protocol == Protocol::Responses {
                    if !body["reasoning"].is_object() {
                        body["reasoning"] = json!({});
                    }
                    body["reasoning"]["effort"] = value.clone();
                } else if protocol == Protocol::Chat {
                    body["reasoning_effort"] = value.clone();
                } else if value == "none" {
                    body["thinking"] = json!({"type":"disabled"});
                } else if body["thinking"]["type"] == "adaptive" {
                    if !body["output_config"].is_object() {
                        body["output_config"] = json!({});
                    }
                    body["output_config"]["effort"] = value.clone();
                } else {
                    let budget = match value.as_str().unwrap_or_default() {
                        "minimal" | "low" => 1024,
                        "medium" => 4096,
                        "high" => 8192,
                        "xhigh" => 16384,
                        _ => 24576,
                    };
                    body["thinking"] = json!({"type":"enabled","budget_tokens":budget});
                }
                continue;
            }
            "reasoningSummary" => {
                if !body["reasoning"].is_object() {
                    body["reasoning"] = json!({});
                }
                body["reasoning"]["summary"] = value.clone();
                continue;
            }
            "textVerbosity" => {
                if !body["text"].is_object() {
                    body["text"] = json!({});
                }
                body["text"]["verbosity"] = value.clone();
                continue;
            }
            key => key,
        };
        if ["maxTokens", "maxOutputTokens"].contains(&key.as_str()) {
            for other in ["max_tokens", "max_completion_tokens", "max_output_tokens"] {
                if other != wire {
                    body.as_object_mut().unwrap().remove(other);
                }
            }
        }
        body[wire] = value.clone();
    }
    if protocol == Protocol::Anthropic {
        if let Some(budget) = body["thinking"]["budgetTokens"].as_u64() {
            body["thinking"]
                .as_object_mut()
                .unwrap()
                .remove("budgetTokens");
            body["thinking"]["budget_tokens"] = json!(budget);
        }
        if body["thinking"]["type"] == "enabled" {
            if options
                .get("temperature")
                .is_some_and(|v| v.as_f64() != Some(1.0))
            {
                return Err("Anthropic thinking requires temperature 1".into());
            }
            body["temperature"] = json!(1);
            let budget = body["thinking"]["budget_tokens"]
                .as_u64()
                .ok_or("Anthropic thinking requires budget_tokens")?;
            if budget < 1024 || body["max_tokens"].as_u64().is_none_or(|max| budget >= max) {
                return Err("Anthropic thinking budget must be at least 1024 and smaller than max output tokens".into());
            }
        }
    }
    let limit_key = match protocol {
        Protocol::Chat if settings.provider_id == "openai" => "max_completion_tokens",
        Protocol::Responses => "max_output_tokens",
        _ => "max_tokens",
    };
    if !options.is_empty()
        && body[limit_key]
            .as_u64()
            .is_some_and(|max| settings.max_output.is_some_and(|limit| max > limit))
    {
        return Err("model generation output token limit exceeds the model output limit".into());
    }
    Ok(())
}

fn public_text(content: &Value) -> Result<String, String> {
    if let Some(text) = content.as_str() {
        return Ok(text.into());
    }
    if content.is_null() {
        return Ok(String::new());
    }
    let mut text = String::new();
    for part in content
        .as_array()
        .ok_or("transformed message content must be text or parts")?
    {
        if ["text", "input_text", "output_text"]
            .contains(&part["type"].as_str().unwrap_or_default())
        {
            text.push_str(
                part["text"]
                    .as_str()
                    .ok_or("transformed text part must contain text")?,
            );
        }
    }
    Ok(text)
}

/// Reconcile explicitly transformed public text into provider replay metadata.
/// Unknown blocks and signed/encrypted reasoning are retained byte-for-byte.
pub fn reconcile_text(message: &mut Value) -> Result<(), String> {
    if message[TEXT_TRANSFORMED] != true || message["role"] != "assistant" {
        return Ok(());
    }
    let text = public_text(&message["content"])?;
    if let Some(metadata) = message.get_mut("providerMetadata") {
        reconcile_metadata(metadata, &text)?;
    }
    Ok(())
}

pub(super) fn replay_metadata(message: &Value) -> Result<Cow<'_, Value>, String> {
    let mut metadata = Cow::Borrowed(&message["providerMetadata"]);
    if message[TEXT_TRANSFORMED] == true {
        reconcile_metadata(metadata.to_mut(), &public_text(&message["content"])?)?;
    }
    reconcile_tool_calls(&mut metadata, &message["tool_calls"])?;
    Ok(metadata)
}

/// Replay the effective public arguments, rather than the model's original
/// arguments, after tool.execute.before has changed the persisted tool input.
/// Calls keep their provider IDs, names, and all other replay fields.
fn reconcile_tool_calls(metadata: &mut Cow<'_, Value>, public: &Value) -> Result<(), String> {
    let Some(calls) = public.as_array() else {
        return Ok(());
    };
    let (field, kind, id_key, args_key) = match metadata["provider"].as_str().unwrap_or_default() {
        "anthropic" => ("content", "tool_use", "id", "input"),
        "openai-responses" => ("output", "function_call", "call_id", "arguments"),
        _ => return Ok(()),
    };
    let blocks = metadata[field]
        .as_array()
        .ok_or("invalid stored provider tool replay metadata")?;
    if blocks.iter().filter(|block| block["type"] == kind).count() != calls.len() {
        return Err("public tool calls do not match stored provider replay call linkage".into());
    }
    let mut seen = std::collections::HashSet::new();
    for call in calls {
        let id = call["id"]
            .as_str()
            .filter(|id| !id.is_empty())
            .ok_or("public tool call has no ID")?;
        if !seen.insert(id) {
            return Err("public tool call IDs must be unique".into());
        }
        let name = call["function"]["name"]
            .as_str()
            .filter(|name| !name.is_empty())
            .ok_or("public tool call has no name")?;
        let arguments = call["function"]["arguments"]
            .as_str()
            .ok_or("public tool call arguments must be JSON text")?;
        let blocks = metadata[field].as_array().unwrap();
        let index = blocks
            .iter()
            .position(|block| block["type"] == kind && block[id_key] == id)
            .ok_or("public tool call ID does not match stored provider replay")?;
        let block = &blocks[index];
        if block["name"] != name
            || blocks
                .iter()
                .filter(|block| block["type"] == kind && block[id_key] == id)
                .count()
                != 1
        {
            return Err("public tool call identity does not match stored provider replay".into());
        }
        if kind == "function_call" && block[args_key].as_str() == Some(arguments) {
            continue;
        }
        let effective: Value = serde_json::from_str(arguments)
            .map_err(|_| "public tool call arguments must be valid JSON".to_string())?;
        if !effective.is_object() {
            return Err("public tool call arguments must be a JSON object".into());
        }
        let changed = if kind == "function_call" {
            block[args_key]
                .as_str()
                .and_then(|args| serde_json::from_str::<Value>(args).ok())
                .as_ref()
                != Some(&effective)
        } else {
            block[args_key] != effective
        };
        if changed {
            metadata.to_mut()[field][index][args_key] = if kind == "function_call" {
                json!(arguments)
            } else {
                effective
            };
        }
    }
    Ok(())
}

fn replace_text(blocks: &mut Vec<Value>, text_type: &str, text: &str, placed: &mut bool) {
    blocks.retain_mut(|block| {
        if block["type"] != text_type {
            return true;
        }
        if *placed || text.is_empty() {
            // Responses message items retain their identity and valid content
            // shape even when a completed-text hook removes all public text.
            if text_type == "output_text" {
                block["text"] = json!("");
                return true;
            }
            return false;
        }
        block["text"] = json!(text);
        *placed = true;
        true
    });
}

fn reconcile_metadata(metadata: &mut Value, text: &str) -> Result<(), String> {
    match metadata["provider"].as_str().unwrap_or_default() {
        "anthropic" => {
            let blocks = metadata["content"]
                .as_array_mut()
                .ok_or("invalid stored Anthropic message metadata")?;
            let mut placed = false;
            replace_text(blocks, "text", text, &mut placed);
            if !placed && !text.is_empty() {
                let index = blocks
                    .iter()
                    .position(|b| b["type"] == "tool_use")
                    .unwrap_or(blocks.len());
                blocks.insert(index, json!({"type":"text","text":text}));
            }
        }
        "openai-responses" => {
            let items = metadata["output"]
                .as_array_mut()
                .ok_or("invalid stored Responses message metadata")?;
            let mut placed = false;
            for item in items.iter_mut() {
                if item["type"] == "message" && item["role"] == "assistant" {
                    if let Some(blocks) = item["content"].as_array_mut() {
                        replace_text(blocks, "output_text", text, &mut placed);
                    }
                }
            }
            if !placed && !text.is_empty() {
                let index = items
                    .iter()
                    .position(|b| b["type"] == "function_call")
                    .unwrap_or(items.len());
                items.insert(index, json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":text,"annotations":[]}]}));
            }
        }
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_changes_only_public_text_and_retains_opaque_data() {
        let mut messages = vec![
            json!({"id":"a","role":"assistant","content":"old","tool_calls":[{"id":"call","function":{"name":"read","arguments":"{}"}}],"providerMetadata":{"secret":"opaque"},"reasoning_content":"hidden"}),
            json!({"role":"tool","tool_call_id":"call","content":"result"}),
        ];
        let mut public: Vec<_> = messages.iter().map(public_message).collect();
        assert!(public[0].get("providerMetadata").is_none());
        assert!(public[0].get("reasoning_content").is_none());
        public[0]["content"] = json!("new");
        apply_messages(&mut messages, &public).unwrap();
        assert_eq!(messages[0]["providerMetadata"]["secret"], "opaque");
        assert_eq!(messages[0]["content"], "new");
        assert_eq!(messages[0][TEXT_TRANSFORMED], true);
        public[1]["tool_call_id"] = json!("wrong");
        assert!(apply_messages(&mut messages, &public).is_err());
        public[1]["tool_call_id"] = json!("call");
        public[0]["tool_calls"][0]["function"]["arguments"] = json!("{\"x\":1}");
        assert!(apply_messages(&mut messages, &public).is_err());
    }

    #[test]
    fn rejects_attachment_changes_and_message_reordering() {
        let mut messages = vec![
            json!({"id":"u","role":"user","content":[{"type":"text","text":"old"},{"type":"image_url","image_url":{"url":"data:image/png;base64,AA"}}]}),
        ];
        let mut public: Vec<_> = messages.iter().map(public_message).collect();
        public[0]["content"][0]["text"] = json!("new");
        apply_messages(&mut messages, &public).unwrap();
        public[0]["content"][1]["image_url"]["url"] = json!("https://elsewhere");
        assert!(apply_messages(&mut messages, &public).is_err());
        assert!(apply_messages(&mut messages, &[]).is_err());
    }

    #[test]
    fn rejects_credentials_transport_headers_and_generation_options() {
        for name in [
            "Authorization",
            "X-API-Key",
            "x-goog-api-key",
            "Cookie",
            "Host",
            "Content-Length",
            "ChatGPT-Account-ID",
        ] {
            assert!(
                safe_headers(&BTreeMap::from([(name.into(), "secret".into())])).is_err(),
                "{name}"
            );
        }
        assert_eq!(
            safe_headers(&BTreeMap::from([("X-Trace".into(), "ok".into())])).unwrap()["x-trace"],
            "ok"
        );
        assert!(
            safe_headers(&BTreeMap::from([("x-trace".into(), "bad\r\nvalue".into())])).is_err()
        );
        for key in [
            "model",
            "provider",
            "apiKey",
            "headers",
            "baseURL",
            "body",
            "max_tokens",
        ] {
            assert!(validate_options(&json!({key:"bad"})).is_err(), "{key}");
        }
        assert!(validate_options(&json!({"maxTokens":0})).is_err());
        assert!(
            validate_protocol_options(&json!({"topK":10}), Protocol::Responses, false).is_err()
        );
        assert!(
            validate_protocol_options(&json!({"maxTokens":10}), Protocol::Responses, true).is_err()
        );
        assert!(validate_limits(
            &json!({"maxTokens":101}),
            &json!({"maxTokens":101}),
            Protocol::Chat,
            Some(100)
        )
        .is_err());
    }

    #[test]
    fn generation_validation_checks_types_ranges_and_model_output_upper_bound() {
        assert!(validate_generation_options(
            &json!({"temperature":"hot"}),
            Protocol::Chat,
            Some(1000)
        )
        .is_err());
        assert!(validate_generation_options(
            &json!({"temperature":2.1}),
            Protocol::Chat,
            Some(1000)
        )
        .is_err());
        assert!(
            validate_generation_options(&json!({"topP":1.1}), Protocol::Responses, Some(1000))
                .is_err()
        );
        assert!(validate_generation_options(
            &json!({"maxTokens":1001}),
            Protocol::Responses,
            Some(1000)
        )
        .is_err());
        assert!(validate_generation_options(
            &json!({"maxTokens":1000}),
            Protocol::Responses,
            Some(1000)
        )
        .is_ok());
        assert!(validate_generation_options(
            &json!({"temperature":1.1}),
            Protocol::Anthropic,
            Some(1000)
        )
        .is_err());
    }
}
