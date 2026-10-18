use super::{
    error_message, json_response, read_response, usage_openai, ChatRequest, EventStream,
    ProviderError, Settings,
};
use crate::types::{ProviderEvent, Tokens};
use async_stream::stream;
use futures::StreamExt;
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
    for (input, output) in [
        ("temperature", "temperature"),
        ("topP", "top_p"),
        ("frequencyPenalty", "frequency_penalty"),
        ("presencePenalty", "presence_penalty"),
        ("seed", "seed"),
        ("stopSequences", "stop"),
        ("parallelToolCalls", "parallel_tool_calls"),
        ("user", "user"),
        ("reasoningEffort", "reasoning_effort"),
    ] {
        if let Some(value) = settings.options.get(input) {
            body[output] = value.clone();
        }
    }
    let max_tokens = settings.options["maxTokens"]
        .as_u64()
        .or_else(|| settings.options["maxOutputTokens"].as_u64());
    if let Some(max_tokens) = max_tokens {
        let key = if settings.provider_id == "openai" {
            "max_completion_tokens"
        } else {
            "max_tokens"
        };
        body[key] = json!(max_tokens);
    }
    if body.get("reasoning_effort").is_none() {
        if let Some(effort) = super::reasoning_effort(settings, req) {
            body["reasoning_effort"] = json!(effort);
        }
    }
    if settings.options["includeUsage"] == false {
        body.as_object_mut().unwrap().remove("stream_options");
    }
    for key in [
        "reasoning_effort",
        "max_tokens",
        "max_completion_tokens",
        "enable_thinking",
        "chat_template_kwargs",
        "min_p",
        "top_k",
        "repetition_penalty",
        "response_format",
        "tool_choice",
        "stream_options",
        "reasoning",
        "provider",
        "plugins",
        "transforms",
    ] {
        if let Some(value) = settings.options.get(key) {
            body[key] = value.clone();
        }
    }
    if let Some(options) = settings.options.as_object() {
        for (key, value) in options {
            if ![
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
                "reasoningEffort",
                "textVerbosity",
                "reasoningSummary",
                "maxTokens",
                "maxOutputTokens",
                "topP",
                "frequencyPenalty",
                "presencePenalty",
                "stopSequences",
                "parallelToolCalls",
                "setCacheKey",
                "enterpriseUrl",
                "thinking",
            ]
            .contains(&key.as_str())
            {
                body[key] = value.clone();
            }
        }
    }
    settings.overlay_body(&mut body);
    body["model"] = json!(settings.api_model);
    body["stream"] = json!(true);
    if !settings.tools_supported {
        body.as_object_mut().unwrap().remove("tools");
        body.as_object_mut().unwrap().remove("tool_choice");
    }
    Ok(body)
}

pub(super) async fn stream(
    settings: Settings,
    req: &ChatRequest,
) -> Result<EventStream, ProviderError> {
    let response = settings
        .send(
            "chat/completions",
            &request_body(&settings, req).map_err(ProviderError::fatal)?,
        )
        .await?;
    let json = json_response(&response);
    let stream = stream! {
        let mut state = ChatState::default();
        if json {
            let value = match read_response(response, settings.chunk_timeout, 16 * 1024 * 1024).await.and_then(|bytes| serde_json::from_slice::<Value>(&bytes).map_err(|e| format!("invalid provider JSON response: {e}"))) {
                Ok(value) => value,
                Err(error) => { yield ProviderEvent::Error(error); return; }
            };
            match state.consume(&value) {
                Ok(events) => { for event in events { yield event; } }
                Err(error) => { yield ProviderEvent::Error(error); return; }
            }
            state.terminal = true;
        } else {
            let mut frames = super::sse::frames(response, settings.chunk_timeout);
            while let Some(frame) = frames.next().await {
                let frame = match frame { Ok(frame) => frame, Err(error) => { yield ProviderEvent::Error(error); return; } };
                if frame.data.trim() == "[DONE]" {
                    if state.finish.is_none() { yield ProviderEvent::Error("provider stream ended before a finish reason".into()); return; }
                    state.terminal = true; break;
                }
                if frame.data.trim().is_empty() { continue; }
                let value: Value = match serde_json::from_str(&frame.data) {
                    Ok(value) => value,
                    Err(error) => { yield ProviderEvent::Error(format!("invalid chat completion SSE JSON: {error}")); return; }
                };
                match state.consume(&value) {
                    Ok(events) => { for event in events { yield event; } }
                    Err(error) => { yield ProviderEvent::Error(error); return; }
                }
            }
        }
        match state.finish() {
            Ok(events) => { for event in events { yield event; } }
            Err(error) => yield ProviderEvent::Error(error),
        }
    };
    Ok(Box::new(Box::pin(stream)))
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

impl ChatState {
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
                                let mut joined = self.reasoning_details[index][key]
                                    .as_str()
                                    .unwrap_or_default()
                                    .to_string();
                                joined.push_str(text);
                                self.reasoning_details[index][key] = json!(joined);
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

    fn finish(mut self) -> Result<Vec<ProviderEvent>, String> {
        if !self.terminal && self.finish.is_none() {
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
            finish: self.finish.take().unwrap_or_else(|| {
                if calls.is_empty() {
                    "stop".into()
                } else {
                    "tool-calls".into()
                }
            }),
            usage: self.usage,
        });
        Ok(events)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn assembles_parallel_calls_and_does_not_finish_truncated_streams() {
        let mut state = ChatState::default();
        state.consume(&json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":1,"id":"two","function":{"name":"read","arguments":"{\"file"}},{"index":0,"id":"one","function":{"name":"glob","arguments":"{}"}}]}}]})).unwrap();
        state.consume(&json!({"choices":[{"delta":{"tool_calls":[{"index":1,"function":{"arguments":"Path\":\"a\"}"}}]},"finish_reason":"tool_calls"}]})).unwrap();
        let events = state.finish().unwrap();
        assert!(
            matches!(&events[0], ProviderEvent::ToolCallStart { call_id, .. } if call_id == "one")
        );
        assert!(
            matches!(&events[1], ProviderEvent::ToolCallStart { call_id, args, .. } if call_id == "two" && args == "{\"filePath\":\"a\"}")
        );
        assert!(ChatState::default().finish().is_err());
    }
    #[test]
    fn api_errors_and_bad_arguments_never_become_success() {
        assert!(ChatState::default()
            .consume(&json!({"error":{"message":"denied"}}))
            .is_err());
        let mut state = ChatState::default();
        state.consume(&json!({"choices":[{"delta":{"tool_calls":[{"function":{"name":"write","arguments":"{"}}]},"finish_reason":"tool_calls"}]})).unwrap();
        assert!(state.finish().is_err());
    }
}
