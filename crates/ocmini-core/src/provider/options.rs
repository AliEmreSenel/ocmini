use super::{ChatRequest, Protocol, Settings};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;

pub(super) fn safe_headers(
    headers: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, String>, String> {
    use reqwest::header::{HeaderName, HeaderValue};
    let mut safe = BTreeMap::new();
    for (name, value) in headers {
        let name = name.to_ascii_lowercase();
        if ["auth", "cookie", "token", "secret", "key"]
            .iter()
            .any(|word| name.contains(word))
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
fn apply_options(
    settings: &Settings,
    options: &Value,
    protocol: Protocol,
    body: &mut Value,
) -> Result<(), String> {
    let Some(options) = options.as_object() else {
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
    Ok(())
}

/// Resolve generation values once through the same codec for configuration
/// and request overrides. Raw vendor body overlays sit between the two.
pub(super) fn configure_body(
    settings: &Settings,
    request: &ChatRequest,
    protocol: Protocol,
    body: &mut Value,
) -> Result<(), String> {
    validate_generation_options(&request.options, protocol, settings.max_output)?;
    validate_protocol_options(&request.options, protocol, settings.codex)?;
    let supported = match protocol {
        Protocol::Chat => CHAT,
        Protocol::Responses => RESPONSES,
        Protocol::Anthropic => ANTHROPIC,
    };
    let mut configured = Map::new();
    if let Some(options) = settings.options.as_object() {
        for (key, value) in options {
            if COMMON.contains(&key.as_str()) || supported.contains(&key.as_str()) {
                configured.insert(key.clone(), value.clone());
            } else if protocol == Protocol::Chat && !RESERVED.contains(&key.as_str()) {
                body[key] = value.clone();
            }
        }
    }
    let fallback_thinking = protocol == Protocol::Anthropic && !configured.contains_key("thinking");
    if protocol != Protocol::Anthropic || fallback_thinking {
        if let Some(effort) = super::reasoning_effort(settings, request) {
            configured
                .entry("reasoningEffort")
                .or_insert_with(|| json!(effort));
        }
    } else if request.options.get("reasoningEffort").is_none() {
        configured.remove("reasoningEffort");
    }
    if protocol == Protocol::Anthropic {
        body["max_tokens"] = json!(configured
            .get("maxTokens")
            .or_else(|| configured.get("maxOutputTokens"))
            .and_then(Value::as_u64)
            .or(settings.max_output)
            .unwrap_or(16_000)
            .max(1));
    }
    apply_options(settings, &Value::Object(configured), protocol, body)?;
    if fallback_thinking && body["thinking"]["type"] == "enabled" {
        let budget = body["thinking"]["budget_tokens"].as_u64().unwrap_or(0);
        let max = body["max_tokens"].as_u64().unwrap_or(0);
        let expanded = max.max(budget + 1024);
        if settings.max_output.is_some_and(|limit| expanded > limit) {
            return Err("Anthropic reasoning budget would exceed the model output limit".into());
        }
        if (request.options.get("maxTokens").is_some()
            || request.options.get("maxOutputTokens").is_some())
            && expanded != max
        {
            return Err(
                "Anthropic reasoning budget exceeds the requested output token limit".into(),
            );
        }
        body["max_tokens"] = json!(expanded);
    }
    if protocol == Protocol::Chat && settings.options["includeUsage"] == false {
        body.as_object_mut().unwrap().remove("stream_options");
    }
    if protocol == Protocol::Responses {
        for key in ["reasoning", "text"] {
            if body
                .get(key)
                .is_some_and(|v| !v.is_null() && !v.is_object())
            {
                return Err(format!("Responses option {key} must be an object"));
            }
        }
        if body.get("reasoning").is_some() && body["reasoning"].get("summary").is_none() {
            body["reasoning"]["summary"] = json!("auto");
        }
    }
    settings.overlay_body(body);
    apply_options(settings, &request.options, protocol, body)?;
    let options = request.options.as_object();
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
                .and_then(|o| o.get("temperature"))
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
    if options.is_some_and(|o| !o.is_empty())
        && body[limit_key]
            .as_u64()
            .is_some_and(|max| settings.max_output.is_some_and(|limit| max > limit))
    {
        return Err("model generation output token limit exceeds the model output limit".into());
    }

    body["model"] = json!(settings.api_model);
    body["stream"] = json!(true);
    if !settings.tools_supported {
        body.as_object_mut().unwrap().remove("tools");
        body.as_object_mut().unwrap().remove("tool_choice");
    }
    if settings.codex && protocol == Protocol::Responses {
        body["store"] = json!(false);
        body.as_object_mut().unwrap().remove("max_output_tokens");
        let mut include = body["include"].as_array().cloned().unwrap_or_default();
        if !include.iter().any(|v| v == "reasoning.encrypted_content") {
            include.push(json!("reasoning.encrypted_content"));
        }
        body["include"] = json!(include);
    }
    Ok(())
}

const RESERVED: &[&str] = &[
    "baseURL",
    "apiKey",
    "authToken",
    "accessToken",
    "headers",
    "body",
    "extraBody",
    "timeout",
    "headerTimeout",
    "chunkTimeout",
    "endpoint",
    "api",
    "transport",
    "encoding",
    "disabled",
    "includeUsage",
    "textVerbosity",
    "reasoningSummary",
    "setCacheKey",
    "enterpriseUrl",
    "thinking",
];
