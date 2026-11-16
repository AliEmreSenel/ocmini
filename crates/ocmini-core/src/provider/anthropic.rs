use super::stream::Decoder;
use super::{error_message, ChatRequest, EventStream, ProviderError, Settings};
use crate::types::{ProviderEvent, Tokens};
use serde_json::{json, Value};
use std::collections::BTreeMap;

fn request_body(settings: &Settings, req: &ChatRequest) -> Result<Value, String> {
    let mut messages: Vec<Value> = Vec::new();
    for message in &req.messages {
        let role = message["role"].as_str().ok_or("message has no role")?;
        if role == "system" || role == "developer" {
            continue;
        }
        let mut blocks = if role == "tool" {
            vec![
                json!({"type":"tool_result","tool_use_id":message["tool_call_id"],"content":crate::provider::content::blocks(&message["content"], crate::provider::Protocol::Anthropic, false)?,"is_error":message["is_error"].as_bool().unwrap_or(false)}),
            ]
        } else if role == "assistant" && message["providerMetadata"]["provider"] == "anthropic" {
            super::replay::replay_metadata(message)?["content"]
                .as_array()
                .cloned()
                .ok_or("invalid stored Anthropic message metadata")?
        } else {
            let mut blocks = crate::provider::content::blocks(
                &message["content"],
                crate::provider::Protocol::Anthropic,
                false,
            )?;
            if let Some(calls) = message["tool_calls"].as_array() {
                for call in calls {
                    let input: Value = serde_json::from_str(
                        call["function"]["arguments"].as_str().unwrap_or("{}"),
                    )
                    .map_err(|e| format!("invalid stored tool arguments: {e}"))?;
                    blocks.push(json!({"type":"tool_use","id":call["id"],"name":call["function"]["name"],"input":input}));
                }
            }
            blocks
        };
        if blocks.is_empty() {
            continue;
        }
        let role = if role == "tool" { "user" } else { role };
        if !["user", "assistant"].contains(&role) {
            return Err(format!("unsupported Anthropic message role {role}"));
        }
        if let Some(previous) = messages.last_mut().filter(|m| m["role"] == role) {
            previous["content"]
                .as_array_mut()
                .unwrap()
                .append(&mut blocks);
        } else {
            messages.push(json!({"role":role,"content":blocks}));
        }
    }
    let mut body = json!({"model":settings.api_model,"stream":true,"messages":null});
    body["messages"] = Value::Array(messages);
    if !req.system.is_empty() {
        body["system"] = json!(req.system);
    }
    if settings.tools_supported && !req.tools.is_empty() {
        body["tools"] = json!(req.tools.iter().map(|tool| json!({"name":tool["function"]["name"],"description":tool["function"]["description"],"input_schema":tool["function"]["parameters"]})).collect::<Vec<_>>());
    }
    super::options::configure_body(
        settings,
        req,
        crate::provider::Protocol::Anthropic,
        &mut body,
    )?;
    Ok(body)
}

pub(super) async fn stream(
    settings: Settings,
    req: &ChatRequest,
) -> Result<EventStream, ProviderError> {
    super::stream::request::<MessageState>(settings, req, "messages", request_body).await
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
        if let Some(value) = usage["input_tokens"].as_i64() {
            self.usage.input = value.max(0);
        }
        if let Some(value) = usage["output_tokens"].as_i64() {
            self.usage.output = value.max(0);
        }
        if let Some(value) = usage["cache_read_input_tokens"].as_i64() {
            self.usage.cache.read = value.max(0);
        }
        if let Some(value) = usage["cache_creation_input_tokens"].as_i64() {
            self.usage.cache.write = value.max(0);
        }
        if let Some(value) = usage["output_tokens_details"]["thinking_tokens"].as_i64() {
            self.usage.reasoning = value.max(0);
        }
    }

    fn finish_arguments(&mut self, index: u64) -> Result<(), String> {
        if let Some(arguments) = self.arguments.remove(&index) {
            let arguments: Value = serde_json::from_str(&arguments)
                .map_err(|e| format!("Anthropic tool arguments are incomplete or invalid: {e}"))?;
            if !arguments.is_object() {
                return Err("Anthropic tool arguments must be a JSON object".into());
            }
            self.blocks
                .get_mut(&index)
                .ok_or("Anthropic tool block is missing")?["input"] = arguments;
        }
        Ok(())
    }
}

impl Decoder for MessageState {
    const PROTOCOL: super::Protocol = super::Protocol::Anthropic;
    fn json(&mut self, value: Value) -> Result<Vec<ProviderEvent>, String> {
        if value.get("error").is_some_and(|error| !error.is_null()) {
            return Err(error_message(&value));
        }
        let blocks = value["content"]
            .as_array()
            .ok_or("Anthropic JSON response is not a completed message")?;
        self.finish = Some(
            value["stop_reason"]
                .as_str()
                .ok_or("Anthropic JSON response has no stop reason")?
                .into(),
        );
        self.update_usage(&value["usage"]);
        let mut events = Vec::new();
        for (index, block) in blocks.iter().enumerate() {
            events.extend(self.consume(
                &json!({"type":"content_block_start","index":index,"content_block":block}),
            )?);
        }
        self.terminal = true;
        Ok(events)
    }
    fn terminal(&self) -> bool {
        self.terminal
    }
    fn consume(&mut self, value: &Value) -> Result<Vec<ProviderEvent>, String> {
        let index = value["index"].as_u64().unwrap_or(0);
        if index > 4096 {
            return Err("Anthropic content block index exceeds limit".into());
        }
        let mut events = Vec::new();
        match value["type"].as_str().unwrap_or_default() {
            "error" => return Err(error_message(value)),
            "message_start" => self.update_usage(&value["message"]["usage"]),
            "content_block_start" => {
                let block = value["content_block"].clone();
                if !block.is_object() {
                    return Err("Anthropic returned an invalid content block".into());
                }
                if let Some(text) = block["text"].as_str().filter(|text| !text.is_empty()) {
                    events.push(ProviderEvent::TextDelta(text.into()));
                }
                if let Some(thinking) = block["thinking"].as_str().filter(|text| !text.is_empty()) {
                    events.push(ProviderEvent::ReasoningDelta(thinking.into()));
                }
                self.blocks.insert(index, block);
            }
            "content_block_delta" => {
                let block = self
                    .blocks
                    .get_mut(&index)
                    .ok_or("Anthropic delta refers to an unknown content block")?;
                let delta = &value["delta"];
                let key = match delta["type"].as_str().unwrap_or_default() {
                    "text_delta" => Some("text"),
                    "thinking_delta" => Some("thinking"),
                    "signature_delta" => Some("signature"),
                    "input_json_delta" => {
                        let args = self.arguments.entry(index).or_default();
                        args.push_str(delta["partial_json"].as_str().unwrap_or_default());
                        if args.len() > 16 * 1024 * 1024 {
                            return Err("Anthropic tool arguments exceed 16 MiB".into());
                        }
                        None
                    }
                    _ => None,
                };
                if let Some(key) = key {
                    let text = delta[key].as_str().unwrap_or_default();
                    super::append_text(&mut block[key], text);
                    if key == "text" && !text.is_empty() {
                        events.push(ProviderEvent::TextDelta(text.into()));
                    }
                    if key == "thinking" && !text.is_empty() {
                        events.push(ProviderEvent::ReasoningDelta(text.into()));
                    }
                }
            }
            "content_block_stop" => self.finish_arguments(index)?,
            "message_delta" => {
                self.update_usage(&value["usage"]);
                if let Some(reason) = value["delta"]["stop_reason"].as_str() {
                    self.finish = Some(reason.into());
                }
            }
            "message_stop" => self.terminal = true,
            _ => {}
        }
        Ok(events)
    }

    fn finish(mut self, model: &str) -> Result<Vec<ProviderEvent>, String> {
        if !self.terminal {
            return Err("Anthropic stream ended before message_stop".into());
        }
        if self.finish.is_none() {
            return Err("Anthropic stream ended without a stop reason".into());
        }
        let pending: Vec<_> = self.arguments.keys().copied().collect();
        for index in pending {
            self.finish_arguments(index)?;
        }
        let mut events = Vec::new();
        for block in self.blocks.values() {
            if block["type"] == "tool_use" {
                let id = block["id"]
                    .as_str()
                    .filter(|id| !id.is_empty())
                    .ok_or("Anthropic tool call has no ID")?;
                let name = block["name"]
                    .as_str()
                    .filter(|name| !name.is_empty())
                    .ok_or("Anthropic tool call has no name")?;
                if !block["input"].is_object() {
                    return Err("Anthropic tool call input is not an object".into());
                }
                events.push(ProviderEvent::ToolCallStart {
                    call_id: id.into(),
                    name: name.into(),
                    args: block["input"].to_string(),
                });
            }
        }
        let finish = match self.finish.as_deref().unwrap_or("end_turn") {
            "tool_use" => "tool-calls",
            "max_tokens" => "length",
            "end_turn" | "stop_sequence" => "stop",
            "refusal" => "content-filter",
            other => other,
        }
        .to_string();
        self.usage.reasoning = self.usage.reasoning.min(self.usage.output);
        self.usage.output -= self.usage.reasoning;
        events.push(ProviderEvent::Metadata(json!({"provider":"anthropic","model":model,"content":self.blocks.into_values().collect::<Vec<_>>()})));
        events.push(ProviderEvent::Finished {
            finish,
            usage: self.usage,
        });
        Ok(events)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn transformed_public_text_preserves_signed_and_redacted_blocks() {
        let settings = super::super::test_settings(json!({}));
        let metadata = json!({"provider":"anthropic","content":[{"type":"thinking","thinking":"private","signature":"signed"},{"type":"redacted_thinking","data":"opaque"},{"type":"text","text":"original"},{"type":"tool_use","id":"call","name":"read","input":{}}]});
        let mut req = ChatRequest {
            messages: vec![
                json!({"role":"assistant","content":"edited","providerMetadata":metadata,"ocminiTextTransformed":true}),
                json!({"role":"tool","tool_call_id":"call","content":"result"}),
            ],
            ..Default::default()
        };
        let body = request_body(&settings, &req).unwrap();
        let blocks = &body["messages"][0]["content"];
        assert_eq!(blocks[0], metadata["content"][0]);
        assert_eq!(blocks[1], metadata["content"][1]);
        assert_eq!(blocks[2]["text"], "edited");
        assert_eq!(blocks[3], metadata["content"][3]);
        assert_eq!(body["messages"][1]["content"][0]["tool_use_id"], "call");
        assert_eq!(req.messages[0]["providerMetadata"], metadata);
        req.messages[0]["content"] = json!("");
        let body = request_body(&settings, &req).unwrap();
        assert_eq!(body["messages"][0]["content"].as_array().unwrap().len(), 3);
        assert_eq!(body["messages"][0]["content"][2], metadata["content"][3]);
    }

    #[test]
    fn replay_uses_effective_tool_arguments_and_preserves_signed_identity() {
        let settings = super::super::test_settings(json!({}));
        let metadata = json!({"provider":"anthropic","content":[{"type":"thinking","thinking":"private","signature":"signed"},{"type":"tool_use","id":"call","name":"read","input":{"filePath":"original"},"opaque":"keep"}]});
        let mut req = ChatRequest {
            messages: vec![
                json!({"role":"assistant","content":null,"providerMetadata":metadata,"tool_calls":[{"id":"call","type":"function","function":{"name":"read","arguments":"{\"filePath\":\"effective\"}"}}]}),
                json!({"role":"tool","tool_call_id":"call","content":"result"}),
            ],
            ..Default::default()
        };
        let body = request_body(&settings, &req).unwrap();
        assert_eq!(body["messages"][0]["content"][0], metadata["content"][0]);
        assert_eq!(
            body["messages"][0]["content"][1]["input"]["filePath"],
            "effective"
        );
        assert_eq!(body["messages"][0]["content"][1]["id"], "call");
        assert_eq!(body["messages"][0]["content"][1]["opaque"], "keep");
        assert_eq!(req.messages[0]["providerMetadata"], metadata);
        req.messages[0]["tool_calls"][0]["function"]["name"] = json!("write");
        assert!(request_body(&settings, &req)
            .unwrap_err()
            .contains("identity"));
    }

    #[test]
    fn request_generation_options_reach_wire_and_cannot_silently_change_limits() {
        let settings = super::super::test_settings(json!({"body":{"temperature":0.9}}));
        let mut req = ChatRequest {
            options: json!({"temperature":0.5,"topP":0.8,"topK":8,"maxTokens":100,"stopSequences":["end"]}),
            ..Default::default()
        };
        let body = request_body(&settings, &req).unwrap();
        assert_eq!(body["temperature"], 0.5);
        assert_eq!(body["top_p"], 0.8);
        assert_eq!(body["top_k"], 8);
        assert_eq!(body["max_tokens"], 100);
        assert_eq!(body["stop_sequences"], json!(["end"]));
        req.options = json!({"reasoningEffort":"high","maxTokens":100});
        assert!(request_body(&settings, &req).is_err());
        req.options = json!({"thinking":{"type":"enabled","budgetTokens":1024},"maxTokens":2048});
        let body = request_body(&settings, &req).unwrap();
        assert_eq!(body["thinking"]["budget_tokens"], 1024);
        assert_eq!(body["temperature"], 1);
        req.options = json!({"frequencyPenalty":0.5});
        assert!(request_body(&settings, &req)
            .unwrap_err()
            .contains("unsupported"));
    }
    #[test]
    fn preserves_signed_thinking_and_split_tool_input() {
        let mut state = MessageState::default();
        for event in [
            json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"consider"}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"opaque"}}),
            json!({"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"call","name":"read","input":{}}}),
            json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"filePath\":\"a\"}"}}),
            json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":10}}),
            json!({"type":"message_stop"}),
        ] {
            state.consume(&event).unwrap();
        }
        let events = state.finish("model").unwrap();
        assert!(
            matches!(&events[0], ProviderEvent::ToolCallStart { args, .. } if args == "{\"filePath\":\"a\"}")
        );
        assert!(
            matches!(&events[1], ProviderEvent::Metadata(value) if value["content"][0]["signature"] == "opaque")
        );
    }
    #[test]
    fn maps_pdf_and_image_attachments() {
        let blocks = crate::provider::content::blocks(&json!([{"type":"image_url","image_url":{"url":"data:image/png;base64,AAAA"}},{"type":"file","file":{"file_data":"data:application/pdf;base64,BBBB","filename":"spec.pdf"}}]), crate::provider::Protocol::Anthropic, false).unwrap();
        assert_eq!(blocks[0]["source"]["media_type"], "image/png");
        assert_eq!(blocks[1]["type"], "document");
        assert_eq!(blocks[1]["source"]["media_type"], "application/pdf");
        assert!(MessageState::default().finish("m").is_err());
    }
}
