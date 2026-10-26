use super::stream::Decoder;
use super::{error_message, usage_openai, ChatRequest, EventStream, ProviderError, Settings};
use crate::types::{ProviderEvent, Tokens};
use serde_json::{json, Value};
use std::collections::BTreeMap;

fn request_body(settings: &Settings, req: &ChatRequest) -> Result<Value, String> {
    let mut messages = Vec::new();
    if !req.system.is_empty() {
        messages.push(json!({"role":"system","content":req.system}));
    }
    for message in &req.messages {
        if message["content"].as_array().is_some_and(|parts| {
            parts.iter().any(|part| {
                part["type"] == "file"
                    && part["file"]["file_data"].as_str().is_some_and(|url| {
                        url.starts_with("https://") || url.starts_with("http://")
                    })
            })
        }) {
            return Err("Chat Completions file attachments require a base64 data URL or file ID. Use a local/data attachment, or select a Responses or Anthropic endpoint for remote files.".into());
        }
        let mut message = message.clone();
        if let Some(map) = message.as_object_mut() {
            map.remove("is_error");
            map.remove(super::TEXT_TRANSFORMED);
            if let Some(metadata) = map.remove("providerMetadata") {
                if metadata["provider"] == "openai-chat" {
                    for key in [
                        "reasoning_content",
                        "reasoning",
                        "reasoning_text",
                        "reasoning_details",
                    ] {
                        if let Some(value) = metadata["message"].get(key) {
                            map.insert(key.into(), value.clone());
                        }
                    }
                }
            }
        }
        messages.push(message);
    }
    let mut body = json!({"model":settings.api_model,"stream":true,"stream_options":{"include_usage":true},"messages":null});
    body["messages"] = Value::Array(messages);
    if settings.tools_supported && !req.tools.is_empty() {
        body["tools"] = json!(req.tools);
    }
    super::options::configure_body(settings, req, super::Protocol::Chat, &mut body)?;
    Ok(body)
}

pub(super) async fn stream(
    settings: Settings,
    req: &ChatRequest,
) -> Result<EventStream, ProviderError> {
    super::stream::request::<ChatState>(settings, req, "chat/completions", request_body).await
}

#[derive(Default)]
struct Call {
    id: String,
    name: String,
    arguments: String,
}

#[derive(Default)]
struct ChatState {
    calls: BTreeMap<u64, Call>,
    usage: Tokens,
    finish: Option<String>,
    terminal: bool,
    text: String,
    reasoning: String,
    reasoning_key: String,
    reasoning_details: Vec<Value>,
    seen: bool,
}

impl Decoder for ChatState {
    const PROTOCOL: super::Protocol = super::Protocol::Chat;
    fn json(&mut self, value: Value) -> Result<Vec<ProviderEvent>, String> {
        self.consume(&value)
    }
    fn terminal(&self) -> bool {
        false
    } // Chat may send usage after its finish reason.
    fn end(&mut self) {
        self.terminal = true;
    }
    fn consume(&mut self, value: &Value) -> Result<Vec<ProviderEvent>, String> {
        if value
            .get("error")
            .filter(|value| !value.is_null())
            .is_some()
        {
            return Err(error_message(value));
        }
        if value["usage"].is_object() {
            self.usage = usage_openai(&value["usage"], false);
        }
        let Some(choice) = value["choices"].as_array().and_then(|choices| {
            choices
                .iter()
                .find(|choice| choice["index"].as_u64().unwrap_or(0) == 0)
        }) else {
            return Ok(Vec::new());
        };
        self.seen = true;
        let delta = choice
            .get("delta")
            .or_else(|| choice.get("message"))
            .unwrap_or(&Value::Null);
        let mut events = Vec::new();
        for key in ["content", "refusal"] {
            if let Some(text) = delta[key].as_str().filter(|text| !text.is_empty()) {
                self.text.push_str(text);
                events.push(ProviderEvent::TextDelta(text.into()));
            }
        }
        for key in ["reasoning_content", "reasoning", "reasoning_text"] {
            if let Some(text) = delta[key].as_str().filter(|text| !text.is_empty()) {
                self.reasoning_key = key.to_string();
                self.reasoning.push_str(text);
                events.push(ProviderEvent::ReasoningDelta(text.into()));
                break;
            }
        }
        if let Some(details) = delta["reasoning_details"].as_array() {
            for detail in details {
                let index = detail["index"]
                    .as_u64()
                    .unwrap_or(self.reasoning_details.len() as u64)
                    as usize;
                if index > 1024 {
                    return Err("provider returned an invalid reasoning detail index".into());
                }
                if self.reasoning_details.len() <= index {
                    self.reasoning_details.resize(index + 1, json!({}));
                }
                if let Some(map) = detail.as_object() {
                    for (key, value) in map {
                        if ["text", "data", "signature"].contains(&key.as_str()) {
                            if let Some(text) = value.as_str() {
                                super::append_text(&mut self.reasoning_details[index][key], text);
                                continue;
                            }
                        }
                        self.reasoning_details[index][key] = value.clone();
                    }
                }
            }
        }
        if let Some(calls) = delta["tool_calls"].as_array() {
            for (position, call) in calls.iter().enumerate() {
                let index = call["index"].as_u64().unwrap_or(position as u64);
                if index > 1024 {
                    return Err("provider returned an invalid tool call index".into());
                }
                let entry = self.calls.entry(index).or_default();
                if let Some(id) = call["id"].as_str().filter(|id| !id.is_empty()) {
                    entry.id = id.into();
                }
                if let Some(name) = call["function"]["name"].as_str() {
                    entry.name.push_str(name);
                }
                if let Some(arguments) = call["function"]["arguments"].as_str() {
                    entry.arguments.push_str(arguments);
                }
                if entry.arguments.len() > 16 * 1024 * 1024 {
                    return Err("provider tool arguments exceed 16 MiB".into());
                }
            }
        }
        if let Some(reason) = choice["finish_reason"].as_str() {
            self.finish = Some(
                match reason {
                    "tool_calls" | "function_call" => "tool-calls",
                    "content_filter" => "content-filter",
                    other => other,
                }
                .into(),
            );
        }
        Ok(events)
    }

    fn finish(mut self, _model: &str) -> Result<Vec<ProviderEvent>, String> {
        if self.finish.is_none() {
            return Err("provider stream ended before a finish reason or [DONE] marker".into());
        }
        if !self.seen {
            return Err("provider returned no completion choice".into());
        }
        let mut events = Vec::new();
        let mut calls = Vec::new();
        for (_, mut call) in self.calls {
            if call.name.is_empty() {
                return Err("provider returned a tool call without a name".into());
            }
            if call.id.is_empty() {
                call.id = crate::types::hex_id("call_");
            }
            if call.arguments.is_empty() {
                call.arguments = "{}".into();
            }
            let input: Value = serde_json::from_str(&call.arguments)
                .map_err(|e| format!("invalid arguments for tool {}: {e}", call.name))?;
            if !input.is_object() {
                return Err(format!(
                    "arguments for tool {} must be a JSON object",
                    call.name
                ));
            }
            calls.push(json!({"id":call.id,"type":"function","function":{"name":call.name,"arguments":call.arguments}}));
            events.push(ProviderEvent::ToolCallStart {
                call_id: call.id,
                name: call.name,
                args: call.arguments,
            });
        }
        let mut message = json!({"role":"assistant","content":self.text});
        if !calls.is_empty() {
            message["tool_calls"] = json!(calls);
        }
        if !self.reasoning.is_empty() {
            message[if self.reasoning_key.is_empty() {
                "reasoning_content"
            } else {
                &self.reasoning_key
            }] = json!(self.reasoning);
        }
        if !self.reasoning_details.is_empty() {
            message["reasoning_details"] = json!(self.reasoning_details);
        }
        events.push(ProviderEvent::Metadata(
            json!({"provider":"openai-chat","message":message}),
        ));
        events.push(ProviderEvent::Finished {
            finish: self.finish.take().unwrap(),
            usage: self.usage,
        });
        Ok(events)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn request_options_override_presets_and_internal_markers_stay_local() {
        let settings =
            super::super::test_settings(json!({"body":{"temperature":1.5,"max_tokens":999}}));
        let req = ChatRequest {
            messages: vec![
                json!({"role":"assistant","content":"edited","ocminiTextTransformed":true,"providerMetadata":{"provider":"openai-chat","message":{"reasoning_content":"opaque","reasoning_details":[{"signature":"signed"}]}}}),
            ],
            options: json!({"temperature":0.25,"topP":0.8,"topK":10,"maxTokens":100,"serviceTier":"priority"}),
            ..Default::default()
        };
        let body = request_body(&settings, &req).unwrap();
        assert_eq!(body["temperature"], 0.25);
        assert_eq!(body["top_p"], 0.8);
        assert_eq!(body["top_k"], 10);
        assert_eq!(body["max_tokens"], 100);
        assert_eq!(body["service_tier"], "priority");
        assert_eq!(body["messages"][0]["content"], "edited");
        assert_eq!(
            body["messages"][0]["reasoning_details"][0]["signature"],
            "signed"
        );
        assert!(body["messages"][0].get("ocminiTextTransformed").is_none());
        assert!(body.get("topK").is_none());
        assert!(body.get("serviceTier").is_none());
    }
    #[test]
    fn assembles_parallel_calls_and_does_not_finish_truncated_streams() {
        let mut state = ChatState::default();
        state.consume(&json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":1,"id":"two","function":{"name":"read","arguments":"{\"file"}},{"index":0,"id":"one","function":{"name":"glob","arguments":"{}"}}]}}]})).unwrap();
        state.consume(&json!({"choices":[{"delta":{"tool_calls":[{"index":1,"function":{"arguments":"Path\":\"a\"}"}}]},"finish_reason":"tool_calls"}]})).unwrap();
        let events = state.finish("test-model").unwrap();
        assert!(
            matches!(&events[0], ProviderEvent::ToolCallStart { call_id, .. } if call_id == "one")
        );
        assert!(
            matches!(&events[1], ProviderEvent::ToolCallStart { call_id, args, .. } if call_id == "two" && args == "{\"filePath\":\"a\"}")
        );
        assert!(ChatState::default().finish("test-model").is_err());
    }
    #[test]
    fn api_errors_and_bad_arguments_never_become_success() {
        assert!(ChatState::default()
            .consume(&json!({"error":{"message":"denied"}}))
            .is_err());
        let mut state = ChatState::default();
        state.consume(&json!({"choices":[{"delta":{"tool_calls":[{"function":{"name":"write","arguments":"{"}}]},"finish_reason":"tool_calls"}]})).unwrap();
        assert!(state.finish("test-model").is_err());
    }
}
