use super::{ProviderError, error_message, json_response, read_response, ChatRequest, EventStream, Settings};
use crate::types::{ProviderEvent, Tokens};
use async_stream::stream;
use futures::StreamExt;
use serde_json::{json, Value};
use std::collections::BTreeMap;

fn source(url: &str, mime: &str) -> Result<Value, String> {
    if let Some(data) = url.strip_prefix("data:") {
        let (header, data) = data.split_once(',').ok_or("malformed data URL attachment")?;
        if !header.ends_with(";base64") { return Err("Anthropic attachments must use base64 data URLs".into()); }
        return Ok(json!({"type":"base64","media_type":header.trim_end_matches(";base64"),"data":data}));
    }
    if url.starts_with("https://") || url.starts_with("http://") { return Ok(json!({"type":"url","url":url})); }
    Err(format!("unsupported {mime} attachment source; use a data URL or HTTPS URL"))
}

fn content_blocks(content: &Value) -> Result<Vec<Value>, String> {
    if let Some(text) = content.as_str() { return Ok(if text.is_empty() { Vec::new() } else { vec![json!({"type":"text","text":text})] }); }
    if content.is_null() { return Ok(Vec::new()); }
    let parts = content.as_array().ok_or("message content must be text or an array of parts")?;
    let mut blocks = Vec::new();
    for part in parts {
        match part["type"].as_str().unwrap_or_default() {
            "text" | "input_text" | "output_text" => {
                let text = part["text"].as_str().ok_or("text attachment is missing text")?;
                if !text.is_empty() { blocks.push(json!({"type":"text","text":text})); }
            }
            "image_url" | "input_image" => {
                let url = part["image_url"]["url"].as_str().or_else(|| part["image_url"].as_str()).ok_or("image attachment is missing its URL")?;
                blocks.push(json!({"type":"image","source":source(url, "image")?}));
            }
            "file" | "input_file" => {
                let file = part.get("file").unwrap_or(part);
                let url = file["file_data"].as_str().or_else(|| file["file_url"].as_str()).ok_or("file attachment is missing file_data/file_url")?;
                let mut block = json!({"type":"document","source":source(url, "document")?});
                if let Some(title) = file["filename"].as_str() { block["title"] = json!(title); }
                blocks.push(block);
            }
            "image" | "document" => blocks.push(part.clone()),
            other => return Err(format!("Anthropic does not support message part type {other}")),
        }
    }
    Ok(blocks)
}

fn request_body(settings: &Settings, req: &ChatRequest) -> Result<Value, String> {
    let mut messages: Vec<Value> = Vec::new();
    for message in &req.messages {
        let role = message["role"].as_str().ok_or("message has no role")?;
        if role == "system" || role == "developer" { continue; }
        let mut blocks = if role == "tool" {
            vec![json!({"type":"tool_result","tool_use_id":message["tool_call_id"],"content":content_blocks(&message["content"])?,"is_error":message["is_error"].as_bool().unwrap_or(false)})]
        } else if role == "assistant" && message["providerMetadata"]["provider"] == "anthropic" {
            message["providerMetadata"]["content"].as_array().cloned().ok_or("invalid stored Anthropic message metadata")?
        } else {
            let mut blocks = content_blocks(&message["content"])?;
            if let Some(calls) = message["tool_calls"].as_array() {
                for call in calls {
                    let input: Value = serde_json::from_str(call["function"]["arguments"].as_str().unwrap_or("{}")).map_err(|e| format!("invalid stored tool arguments: {e}"))?;
                    blocks.push(json!({"type":"tool_use","id":call["id"],"name":call["function"]["name"],"input":input}));
                }
            }
            blocks
        };
        if blocks.is_empty() { continue; }
        let role = if role == "tool" { "user" } else { role };
        if !["user", "assistant"].contains(&role) { return Err(format!("unsupported Anthropic message role {role}")); }
        if let Some(previous) = messages.last_mut().filter(|m| m["role"] == role) {
            previous["content"].as_array_mut().unwrap().append(&mut blocks);
        } else { messages.push(json!({"role":role,"content":blocks})); }
    }
    let max_output = settings.options["maxTokens"].as_u64().or_else(|| settings.options["maxOutputTokens"].as_u64()).or(settings.max_output).unwrap_or(16_000).max(1);
    let mut body = json!({"model":settings.api_model,"max_tokens":max_output,"stream":true,"messages":messages});
    if !req.system.is_empty() { body["system"] = json!(req.system); }
    if settings.tools_supported && !req.tools.is_empty() {
        body["tools"] = json!(req.tools.iter().map(|tool| json!({"name":tool["function"]["name"],"description":tool["function"]["description"],"input_schema":tool["function"]["parameters"]})).collect::<Vec<_>>());
    }
    for (key, wire) in [("temperature", "temperature"), ("topP", "top_p"), ("topK", "top_k"), ("stopSequences", "stop_sequences"), ("thinking", "thinking"), ("output_config", "output_config"), ("tool_choice", "tool_choice"), ("metadata", "metadata")] {
        if let Some(value) = settings.options.get(key) { body[wire] = value.clone(); }
    }
    if let Some(budget) = body["thinking"]["budgetTokens"].as_u64() {
        body["thinking"].as_object_mut().unwrap().remove("budgetTokens");
        body["thinking"]["budget_tokens"] = json!(budget);
    }
    if body.get("thinking").is_none() {
        if let Some(variant) = super::reasoning_effort(settings, req).filter(|variant| *variant != "none") {
            let budget = match variant { "minimal" | "low" => 1024, "medium" => 4096, "high" => 8192, "xhigh" => 16384, _ => 24576 };
            let max = max_output.max(budget + 1024);
            body["max_tokens"] = json!(max);
            body["thinking"] = json!({"type":"enabled","budget_tokens":budget});
        }
    }
    if body["thinking"]["type"] == "enabled" {
        body["temperature"] = json!(1);
        if body["thinking"]["budget_tokens"].as_u64().unwrap_or(0) >= body["max_tokens"].as_u64().unwrap_or(0) {
            return Err("Anthropic thinking budget_tokens must be smaller than max_tokens".into());
        }
    }
    settings.overlay_body(&mut body);
    body["model"] = json!(settings.api_model);
    body["stream"] = json!(true);
    if !settings.tools_supported { body.as_object_mut().unwrap().remove("tools"); body.as_object_mut().unwrap().remove("tool_choice"); }
    Ok(body)
}

pub(super) async fn stream(settings: Settings, req: ChatRequest) -> Result<EventStream, ProviderError> {
    let response = settings.send("messages", &request_body(&settings, &req).map_err(ProviderError::fatal)?).await?;
    let json = json_response(&response);
    let stream = stream! {
        let mut state = MessageState::default();
        if json {
            let value = match read_response(response, settings.chunk_timeout, 16 * 1024 * 1024).await.and_then(|bytes| serde_json::from_slice::<Value>(&bytes).map_err(|e| format!("invalid Anthropic JSON response: {e}"))) {
                Ok(value) => value,
                Err(error) => { yield ProviderEvent::Error(error); return; }
            };
            if value.get("error").is_some_and(|error| !error.is_null()) { yield ProviderEvent::Error(error_message(&value)); return; }
            if !value["content"].is_array() || !value["stop_reason"].is_string() { yield ProviderEvent::Error("Anthropic JSON response is not a completed message".into()); return; }
            state.update_usage(&value["usage"]);
            state.finish = value["stop_reason"].as_str().map(String::from);
            if let Some(blocks) = value["content"].as_array() {
                for (index, block) in blocks.iter().enumerate() {
                    match state.consume(&json!({"type":"content_block_start","index":index,"content_block":block})) {
                        Ok(events) => { for event in events { yield event; } }
                        Err(error) => { yield ProviderEvent::Error(error); return; }
                    }
                }
            }
            state.terminal = true;
        } else {
            let mut frames = super::sse::frames(response, settings.chunk_timeout);
            while let Some(frame) = frames.next().await {
                let frame = match frame { Ok(frame) => frame, Err(error) => { yield ProviderEvent::Error(error); return; } };
                if frame.data.trim().is_empty() { continue; }
                let mut value: Value = match serde_json::from_str(&frame.data) {
                    Ok(value) => value,
                    Err(error) => { yield ProviderEvent::Error(format!("invalid Anthropic SSE JSON: {error}")); return; }
                };
                if value.get("type").is_none() { value["type"] = json!(frame.event); }
                match state.consume(&value) {
                    Ok(events) => { for event in events { yield event; } }
                    Err(error) => { yield ProviderEvent::Error(error); return; }
                }
                if state.terminal { break; }
            }
        }
        match state.finish(&settings.api_model) {
            Ok(events) => { for event in events { yield event; } }
            Err(error) => yield ProviderEvent::Error(error),
        }
    };
    Ok(Box::new(Box::pin(stream)))
}

#[derive(Default)]
struct MessageState {
    blocks: BTreeMap<u64, Value>,
    arguments: BTreeMap<u64, String>,
    usage: Tokens,
    finish: Option<String>,
    terminal: bool,
}

impl MessageState {
    fn update_usage(&mut self, usage: &Value) {
        if let Some(value) = usage["input_tokens"].as_i64() { self.usage.input = value.max(0); }
        if let Some(value) = usage["output_tokens"].as_i64() { self.usage.output = value.max(0); }
        if let Some(value) = usage["cache_read_input_tokens"].as_i64() { self.usage.cache.read = value.max(0); }
        if let Some(value) = usage["cache_creation_input_tokens"].as_i64() { self.usage.cache.write = value.max(0); }
        if let Some(value) = usage["output_tokens_details"]["thinking_tokens"].as_i64() { self.usage.reasoning = value.max(0); }
    }

    fn consume(&mut self, value: &Value) -> Result<Vec<ProviderEvent>, String> {
        let index = value["index"].as_u64().unwrap_or(0);
        if index > 4096 { return Err("Anthropic content block index exceeds limit".into()); }
        let mut events = Vec::new();
        match value["type"].as_str().unwrap_or_default() {
            "error" => return Err(error_message(value)),
            "message_start" => self.update_usage(&value["message"]["usage"]),
            "content_block_start" => {
                let block = value["content_block"].clone();
                if !block.is_object() { return Err("Anthropic returned an invalid content block".into()); }
                if let Some(text) = block["text"].as_str().filter(|text| !text.is_empty()) { events.push(ProviderEvent::TextDelta(text.into())); }
                if let Some(thinking) = block["thinking"].as_str().filter(|text| !text.is_empty()) { events.push(ProviderEvent::ReasoningDelta(thinking.into())); }
                self.blocks.insert(index, block);
            }
            "content_block_delta" => {
                let block = self.blocks.get_mut(&index).ok_or("Anthropic delta refers to an unknown content block")?;
                let delta = &value["delta"];
                let key = match delta["type"].as_str().unwrap_or_default() {
                    "text_delta" => Some("text"),
                    "thinking_delta" => Some("thinking"),
                    "signature_delta" => Some("signature"),
                    "input_json_delta" => {
                        let args = self.arguments.entry(index).or_default();
                        args.push_str(delta["partial_json"].as_str().unwrap_or_default());
                        if args.len() > 16 * 1024 * 1024 { return Err("Anthropic tool arguments exceed 16 MiB".into()); }
                        None
                    }
                    _ => None,
                };
                if let Some(key) = key {
                    let text = delta[key].as_str().unwrap_or_default();
                    let mut combined = block[key].as_str().unwrap_or_default().to_string();
                    combined.push_str(text);
                    block[key] = json!(combined);
                    if key == "text" && !text.is_empty() { events.push(ProviderEvent::TextDelta(text.into())); }
                    if key == "thinking" && !text.is_empty() { events.push(ProviderEvent::ReasoningDelta(text.into())); }
                }
            }
            "content_block_stop" => self.finish_arguments(index)?,
            "message_delta" => {
                self.update_usage(&value["usage"]);
                if let Some(reason) = value["delta"]["stop_reason"].as_str() { self.finish = Some(reason.into()); }
            }
            "message_stop" => self.terminal = true,
            _ => {}
        }
        Ok(events)
    }

    fn finish_arguments(&mut self, index: u64) -> Result<(), String> {
        if let Some(arguments) = self.arguments.remove(&index) {
            let arguments: Value = serde_json::from_str(&arguments).map_err(|e| format!("Anthropic tool arguments are incomplete or invalid: {e}"))?;
            if !arguments.is_object() { return Err("Anthropic tool arguments must be a JSON object".into()); }
            self.blocks.get_mut(&index).ok_or("Anthropic tool block is missing")?["input"] = arguments;
        }
        Ok(())
    }

    fn finish(mut self, model: &str) -> Result<Vec<ProviderEvent>, String> {
        if !self.terminal { return Err("Anthropic stream ended before message_stop".into()); }
        let pending: Vec<_> = self.arguments.keys().copied().collect();
        for index in pending { self.finish_arguments(index)?; }
        let mut events = Vec::new();
        for block in self.blocks.values() {
            if block["type"] == "tool_use" {
                let id = block["id"].as_str().filter(|id| !id.is_empty()).ok_or("Anthropic tool call has no ID")?;
                let name = block["name"].as_str().filter(|name| !name.is_empty()).ok_or("Anthropic tool call has no name")?;
                if !block["input"].is_object() { return Err("Anthropic tool call input is not an object".into()); }
                events.push(ProviderEvent::ToolCallStart { call_id: id.into(), name: name.into(), args: block["input"].to_string() });
            }
        }
        let finish = match self.finish.as_deref().unwrap_or("end_turn") {
            "tool_use" => "tool-calls", "max_tokens" => "length", "end_turn" | "stop_sequence" => "stop", "refusal" => "content-filter", other => other,
        }.to_string();
        self.usage.reasoning = self.usage.reasoning.min(self.usage.output);
        self.usage.output -= self.usage.reasoning;
        events.push(ProviderEvent::Metadata(json!({"provider":"anthropic","model":model,"content":self.blocks.into_values().collect::<Vec<_>>()})));
        events.push(ProviderEvent::Finished { finish, usage: self.usage });
        Ok(events)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn preserves_signed_thinking_and_split_tool_input() {
        let mut state = MessageState::default();
        for event in [json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}), json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"consider"}}), json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"opaque"}}), json!({"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"call","name":"read","input":{}}}), json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"filePath\":\"a\"}"}}), json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":10}}), json!({"type":"message_stop"})] { state.consume(&event).unwrap(); }
        let events = state.finish("model").unwrap();
        assert!(matches!(&events[0], ProviderEvent::ToolCallStart { args, .. } if args == "{\"filePath\":\"a\"}"));
        assert!(matches!(&events[1], ProviderEvent::Metadata(value) if value["content"][0]["signature"] == "opaque"));
    }
    #[test]
    fn maps_pdf_and_image_attachments() {
        let blocks = content_blocks(&json!([{"type":"image_url","image_url":{"url":"data:image/png;base64,AAAA"}},{"type":"file","file":{"file_data":"data:application/pdf;base64,BBBB","filename":"spec.pdf"}}])).unwrap();
        assert_eq!(blocks[0]["source"]["media_type"], "image/png");
        assert_eq!(blocks[1]["type"], "document");
        assert_eq!(blocks[1]["source"]["media_type"], "application/pdf");
        assert!(MessageState::default().finish("m").is_err());
    }
}
