use crate::hooks::{Hook, HookName, Shape};
use crate::hooks::{MessageHook, ModelHook};
use tokio_util::sync::CancellationToken;
pub(crate) const MODEL_CONTEXT_TRANSFORM: Hook = Hook::new(
    HookName::Model(ModelHook::ContextTransform),
    &[("system", Shape::Text), ("messages", Shape::Array)],
)
.request("primary")
.validate(validate_context);
pub(crate) const MODEL_PARAMS_TRANSFORM: Hook = Hook::new(
    HookName::Model(ModelHook::ParamsTransform),
    &[("options", Shape::Object), ("variant", Shape::NullableText)],
)
.request("primary")
.validate(validate_params);
pub(crate) const MODEL_HEADERS_TRANSFORM: Hook = Hook::new(
    HookName::Model(ModelHook::HeadersTransform),
    &[("headers", Shape::Object)],
)
.request("primary")
.validate(validate_headers);
pub(crate) const MODEL_RETRY_BEFORE: Hook = Hook::new(
    HookName::Model(ModelHook::RetryBefore),
    &[("delayMs", Shape::Integer(0, 120_000))],
)
.request("primary");
pub(crate) const MESSAGE_TEXT_COMPLETE: Hook = Hook::new(
    HookName::Message(MessageHook::TextComplete),
    &[("text", Shape::Text)],
)
.request("primary");
pub(crate) const HOOKS: &[Hook] = &[
    MODEL_CONTEXT_TRANSFORM,
    MODEL_PARAMS_TRANSFORM,
    MODEL_HEADERS_TRANSFORM,
    MODEL_RETRY_BEFORE,
    MESSAGE_TEXT_COMPLETE,
];
use super::{options::*, ChatRequest};
use crate::engine::App;
use crate::types::OperationError;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;

/// Persist on transformed text parts, and propagate to their normalized
/// assistant message when rebuilding history. Never send this field on the wire.
pub const TEXT_TRANSFORMED: &str = "ocminiTextTransformed";

#[derive(Serialize, Deserialize)]
struct Context {
    system: String,
    messages: Vec<Value>,
}
#[derive(Serialize, Deserialize)]
struct Parameters {
    options: Value,
    variant: Option<String>,
}
#[derive(Serialize, Deserialize)]
struct Headers {
    headers: BTreeMap<String, String>,
}
#[derive(Serialize, Deserialize)]
struct Text {
    text: String,
}

/// Project only public fields; opaque replay metadata stays in the request.
pub async fn prepare_hooks(
    app: &App,
    context: &Value,
    request: &mut ChatRequest,
    cancel: Option<&CancellationToken>,
) -> Result<(), OperationError> {
    if app.hooks.matches(MODEL_CONTEXT_TRANSFORM, context) {
        let transformed = app
            .hooks
            .transform(
                MODEL_CONTEXT_TRANSFORM,
                context,
                Context {
                    system: request.system.clone(),
                    messages: request.messages.iter().map(public_message).collect(),
                },
                cancel,
            )
            .await?;
        apply_messages(&mut request.messages, &transformed.messages)?;
        request.system = transformed.system;
    }
    if app.hooks.matches(MODEL_PARAMS_TRANSFORM, context) {
        validate_options(&request.options)?;
        let parameters = app
            .hooks
            .transform(
                MODEL_PARAMS_TRANSFORM,
                context,
                Parameters {
                    options: if request.options.is_null() {
                        serde_json::json!({})
                    } else {
                        request.options.take()
                    },
                    variant: request.variant.take(),
                },
                cancel,
            )
            .await?;
        request.options = parameters.options;
        request.variant = parameters.variant;
    }
    if app.hooks.matches(MODEL_HEADERS_TRANSFORM, context) {
        let headers = app
            .hooks
            .transform(
                MODEL_HEADERS_TRANSFORM,
                context,
                Headers {
                    headers: safe_headers(&request.headers)?,
                },
                cancel,
            )
            .await?;
        request.headers = safe_headers(&headers.headers)?;
    }
    Ok(())
}

pub async fn complete_text(
    app: &App,
    context: &Value,
    text: String,
    cancel: Option<&CancellationToken>,
) -> Result<String, OperationError> {
    Ok(app
        .hooks
        .transform(MESSAGE_TEXT_COMPLETE, context, Text { text }, cancel)
        .await?
        .text)
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

fn validate_messages(original: &[Value], transformed: &[Value]) -> Result<(), String> {
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
    Ok(())
}
fn apply_messages(original: &mut [Value], transformed: &[Value]) -> Result<(), String> {
    validate_messages(original, transformed)?;
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

fn validate_params(_: &Value, set: &serde_json::Map<String, Value>) -> Result<(), String> {
    if let Some(options) = set.get("options") {
        validate_options(options)?;
    }
    if set.get("variant").is_some_and(|v| v.as_str() == Some("")) {
        return Err("variant must be nonempty or null".into());
    }
    Ok(())
}
fn validate_headers(_: &Value, set: &serde_json::Map<String, Value>) -> Result<(), String> {
    if let Some(headers) = set.get("headers").and_then(Value::as_object) {
        let headers: BTreeMap<String, String> =
            serde_json::from_value(Value::Object(headers.clone()))
                .map_err(|_| "header values must be strings")?;
        safe_headers(&headers)?;
    }
    Ok(())
}
fn validate_context(input: &Value, set: &serde_json::Map<String, Value>) -> Result<(), String> {
    if let Some(messages) = set.get("messages").and_then(Value::as_array) {
        validate_messages(
            input["messages"]
                .as_array()
                .ok_or("context has no messages")?,
            messages,
        )?;
    }
    Ok(())
}

/// Check and normalize request-local headers both before hooks and at the final
/// adapter boundary. Configured auth/scope headers are never included here.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::Protocol;

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
