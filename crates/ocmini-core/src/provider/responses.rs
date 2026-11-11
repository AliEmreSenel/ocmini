use super::stream::Decoder;
use super::{error_message, usage_openai, ChatRequest, EventStream, ProviderError, Settings};
use crate::types::{ProviderEvent, Tokens};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};

fn request_body(settings: &Settings, req: &ChatRequest) -> Result<Value, String> {
    let mut input = Vec::new();
    for message in &req.messages {
        let role = message["role"].as_str().ok_or("message has no role")?;
        if role == "tool" {
            let output = if let Some(text) = message["content"].as_str() {
                json!(text)
            } else {
                json!(crate::provider::content::blocks(
                    &message["content"],
                    crate::provider::Protocol::Responses,
                    false
                )?)
            };
            input.push(json!({"type":"function_call_output","call_id":message["tool_call_id"],"output":output}));
            continue;
        }
        if role == "assistant" && message["providerMetadata"]["provider"] == "openai-responses" {
            let metadata = super::replay::replay_metadata(message)?;
            if let Some(items) = metadata["output"]
                .as_array()
                .filter(|items| !items.is_empty())
            {
                input.extend(items.iter().cloned());
                continue;
            }
        }
        let content = crate::provider::content::blocks(
            &message["content"],
            crate::provider::Protocol::Responses,
            role == "assistant",
        )?;
        if !content.is_empty() {
            input.push(json!({"role":role,"content":content}));
        }
        if let Some(calls) = message["tool_calls"].as_array() {
            for call in calls {
                input.push(json!({"type":"function_call","call_id":call["id"],"name":call["function"]["name"],"arguments":call["function"]["arguments"]}));
            }
        }
    }
    let mut body = json!({"model":settings.api_model,"instructions":req.system,"input":null,"stream":true,"store":false,"include":["reasoning.encrypted_content"]});
    body["input"] = Value::Array(input);
    if settings.tools_supported && !req.tools.is_empty() {
        body["tools"] = json!(req.tools.iter().map(|tool| json!({"type":"function","name":tool["function"]["name"],"description":tool["function"]["description"],"parameters":tool["function"]["parameters"],"strict":false})).collect::<Vec<_>>());
    }
    super::options::configure_body(
        settings,
        req,
        crate::provider::Protocol::Responses,
        &mut body,
    )?;
    Ok(body)
}

pub(super) async fn stream(
    settings: Settings,
    req: &ChatRequest,
) -> Result<EventStream, ProviderError> {
    super::stream::request::<ResponseState>(settings, req, "responses", request_body).await
}

#[derive(Default)]
struct ResponseState {
    output: BTreeMap<u64, Value>,
    item_indices: HashMap<String, u64>,
    delivered: HashMap<(u64, u64, bool), String>,
    usage: Tokens,
    terminal: bool,
    incomplete: Option<String>,
}

impl ResponseState {
    fn deliver(
        &mut self,
        index: u64,
        part: u64,
        reasoning: bool,
        text: &str,
        complete: bool,
    ) -> Vec<ProviderEvent> {
        let previous = self.delivered.entry((index, part, reasoning)).or_default();
        let delta = if complete {
            text.strip_prefix(previous.as_str())
                .unwrap_or("")
                .to_string()
        } else {
            text.to_string()
        };
        previous.push_str(&delta);
        if delta.is_empty() {
            return Vec::new();
        }
        vec![if reasoning {
            ProviderEvent::ReasoningDelta(delta)
        } else {
            ProviderEvent::TextDelta(delta)
        }]
    }

    fn store_item(&mut self, index: u64, item: Value) -> Vec<ProviderEvent> {
        if let Some(id) = item["id"].as_str() {
            self.item_indices.insert(id.to_string(), index);
        }
        let mut events = Vec::new();
        let reasoning = item["type"] == "reasoning";
        if let Some(parts) = item[if reasoning { "summary" } else { "content" }].as_array() {
            for (part, block) in parts.iter().enumerate() {
                if let Some(text) = block["text"].as_str() {
                    events.extend(self.deliver(index, part as u64, reasoning, text, true));
                }
                if let Some(text) = block["refusal"].as_str() {
                    events.extend(self.deliver(index, part as u64, false, text, true));
                }
            }
        }
        self.output.insert(index, item);
        events
    }
}

impl Decoder for ResponseState {
    const PROTOCOL: super::Protocol = super::Protocol::Responses;
    fn json(&mut self, value: Value) -> Result<Vec<ProviderEvent>, String> {
        let event =
            if value["status"] == "failed" || value.get("error").is_some_and(|v| !v.is_null()) {
                "response.failed"
            } else {
                match value["status"].as_str() {
                    Some("incomplete") => "response.incomplete",
                    Some("completed") => "response.completed",
                    _ => return Err("Responses JSON response has no terminal status".into()),
                }
            };
        self.consume(&json!({"type":event,"response":value}))
    }
    fn terminal(&self) -> bool {
        self.terminal
    }
    fn consume(&mut self, value: &Value) -> Result<Vec<ProviderEvent>, String> {
        let kind = value["type"].as_str().unwrap_or_default();
        if kind == "error" {
            return Err(error_message(value));
        }
        if kind == "response.failed" {
            return Err(error_message(&value["response"]));
        }
        let index = value["output_index"]
            .as_u64()
            .or_else(|| {
                value["item_id"]
                    .as_str()
                    .and_then(|id| self.item_indices.get(id).copied())
            })
            .unwrap_or(0);
        if index > 4096 {
            return Err("Responses output item index exceeds limit".into());
        }
        match kind {
            "response.output_item.added" | "response.output_item.done" => {
                if !value["item"].is_object() {
                    return Err("Responses returned an invalid output item".into());
                }
                Ok(self.store_item(index, value["item"].clone()))
            }
            "response.output_text.delta"
            | "response.refusal.delta"
            | "response.reasoning_summary_text.delta"
            | "response.reasoning_text.delta" => {
                let reasoning = kind.contains("reasoning");
                let part = value[if reasoning {
                    "summary_index"
                } else {
                    "content_index"
                }]
                .as_u64()
                .unwrap_or(0);
                let delta = value["delta"]
                    .as_str()
                    .ok_or("Responses text delta is not a string")?;
                Ok(self.deliver(index, part, reasoning, delta, false))
            }
            "response.output_text.done"
            | "response.reasoning_summary_text.done"
            | "response.reasoning_text.done" => {
                let reasoning = kind.contains("reasoning");
                let part = value[if reasoning {
                    "summary_index"
                } else {
                    "content_index"
                }]
                .as_u64()
                .unwrap_or(0);
                Ok(self.deliver(
                    index,
                    part,
                    reasoning,
                    value["text"].as_str().unwrap_or_default(),
                    true,
                ))
            }
            "response.function_call_arguments.delta" | "response.function_call_arguments.done" => {
                let item = self
                    .output
                    .entry(index)
                    .or_insert_with(|| json!({"type":"function_call","arguments":""}));
                if kind.ends_with(".done") {
                    item["arguments"] = json!(value["arguments"]
                        .as_str()
                        .ok_or("Responses arguments.done is missing arguments")?);
                } else {
                    super::append_text(
                        &mut item["arguments"],
                        value["delta"].as_str().unwrap_or_default(),
                    );
                }
                if item["arguments"].as_str().unwrap_or_default().len() > 16 * 1024 * 1024 {
                    return Err("Responses tool arguments exceed 16 MiB".into());
                }
                Ok(Vec::new())
            }
            "response.completed" | "response.incomplete" => {
                let response = &value["response"];
                if !response["output"].is_array() {
                    return Err("Responses terminal event has no response output".into());
                }
                if response
                    .get("error")
                    .filter(|value| !value.is_null())
                    .is_some()
                {
                    return Err(error_message(response));
                }
                let mut events = Vec::new();
                if let Some(items) = response["output"].as_array() {
                    for (index, item) in items.iter().enumerate() {
                        events.extend(self.store_item(index as u64, item.clone()));
                    }
                }
                self.usage = usage_openai(&response["usage"], true);
                if kind == "response.incomplete" {
                    self.incomplete = Some(
                        response["incomplete_details"]["reason"]
                            .as_str()
                            .unwrap_or("unknown")
                            .into(),
                    );
                }
                self.terminal = true;
                Ok(events)
            }
            _ => Ok(Vec::new()),
        }
    }

    fn finish(mut self, model: &str) -> Result<Vec<ProviderEvent>, String> {
        if !self.terminal {
            return Err(
                "Responses stream ended before a completed/incomplete response event".into(),
            );
        }
        let mut events = Vec::new();
        for (index, item) in &mut self.output {
            if item["type"] == "function_call" {
                let id = item["call_id"]
                    .as_str()
                    .filter(|id| !id.is_empty())
                    .ok_or("Responses tool call has no call_id")?;
                let name = item["name"]
                    .as_str()
                    .filter(|name| !name.is_empty())
                    .ok_or("Responses tool call has no name")?;
                let arguments = item["arguments"].as_str().unwrap_or("{}");
                let parsed: Value = serde_json::from_str(arguments)
                    .map_err(|e| format!("invalid Responses tool arguments for {name}: {e}"))?;
                if !parsed.is_object() {
                    return Err(format!(
                        "Responses tool arguments for {name} must be a JSON object"
                    ));
                }
                if item["status"] == "incomplete" || item["status"] == "in_progress" {
                    return Err(format!("Responses tool call {name} did not complete"));
                }
                events.push(ProviderEvent::ToolCallStart {
                    call_id: id.into(),
                    name: name.into(),
                    args: arguments.into(),
                });
            }
            // Some compatible endpoints omit final content but send all text deltas.
            if item["type"] == "message"
                && item["content"]
                    .as_array()
                    .map(|parts| parts.is_empty())
                    .unwrap_or(true)
            {
                let mut content = Vec::new();
                let mut pieces: Vec<_> = self
                    .delivered
                    .iter()
                    .filter(|((output, _, reasoning), _)| output == index && !reasoning)
                    .collect();
                pieces.sort_by_key(|((_, part, _), _)| *part);
                for (_, text) in pieces {
                    content.push(json!({"type":"output_text","text":text,"annotations":[]}));
                }
                item["content"] = json!(content);
            }
        }
        let finish = match self.incomplete.as_deref() {
            Some("max_output_tokens") => "length",
            Some("content_filter") => "content-filter",
            Some(_) => "unknown",
            None if !events.is_empty() => "tool-calls",
            None => "stop",
        };
        events.push(ProviderEvent::Metadata(json!({"provider":"openai-responses","model":model,"output":self.output.into_values().collect::<Vec<_>>()})));
        events.push(ProviderEvent::Finished {
            finish: finish.into(),
            usage: self.usage,
        });
        Ok(events)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn transformed_public_text_reaches_replay_without_changing_reasoning_or_calls() {
        let settings = super::super::test_settings(json!({}));
        let metadata = json!({"provider":"openai-responses","output":[{"type":"reasoning","id":"reason","encrypted_content":"opaque","summary":[]},{"type":"message","id":"msg","role":"assistant","content":[{"type":"output_text","text":"original","annotations":[]}]},{"type":"function_call","id":"fc","call_id":"call","name":"read","arguments":"{}"}]});
        let mut req = ChatRequest {
            messages: vec![
                json!({"role":"assistant","content":"edited","providerMetadata":metadata,"ocminiTextTransformed":true}),
                json!({"role":"tool","tool_call_id":"call","content":"result"}),
            ],
            ..Default::default()
        };
        let body = request_body(&settings, &req).unwrap();
        assert_eq!(body["input"][0], metadata["output"][0]);
        assert_eq!(body["input"][1]["id"], "msg");
        assert_eq!(body["input"][1]["content"][0]["text"], "edited");
        assert_eq!(body["input"][2], metadata["output"][2]);
        assert_eq!(body["input"][3]["call_id"], "call");
        assert_eq!(req.messages[0]["providerMetadata"], metadata);
        req.messages[0]
            .as_object_mut()
            .unwrap()
            .remove("ocminiTextTransformed");
        assert_eq!(
            request_body(&settings, &req).unwrap()["input"][1]["content"][0]["text"],
            "original"
        );
        req.messages[0]["ocminiTextTransformed"] = json!(true);
        req.messages[0]["content"] = json!("");
        let body = request_body(&settings, &req).unwrap();
        assert_eq!(body["input"][1]["id"], "msg");
        assert_eq!(body["input"][1]["content"][0]["text"], "");
        assert_eq!(body["input"][0], metadata["output"][0]);
    }

    #[test]
    fn replay_uses_effective_tool_arguments_without_replacing_call_ids() {
        let settings = super::super::test_settings(json!({}));
        let metadata = json!({"provider":"openai-responses","output":[{"type":"reasoning","id":"reason","encrypted_content":"opaque","summary":[]},{"type":"function_call","id":"item","call_id":"call","name":"read","arguments":"{\"filePath\":\"original\"}","opaque":"keep"}]});
        let mut req = ChatRequest {
            messages: vec![
                json!({"role":"assistant","content":null,"providerMetadata":metadata,"tool_calls":[{"id":"call","type":"function","function":{"name":"read","arguments":"{\"filePath\":\"effective\"}"}}]}),
                json!({"role":"tool","tool_call_id":"call","content":"result"}),
            ],
            ..Default::default()
        };
        let body = request_body(&settings, &req).unwrap();
        assert_eq!(body["input"][0], metadata["output"][0]);
        assert_eq!(
            body["input"][1]["arguments"],
            "{\"filePath\":\"effective\"}"
        );
        assert_eq!(body["input"][1]["id"], "item");
        assert_eq!(body["input"][1]["call_id"], "call");
        assert_eq!(body["input"][1]["opaque"], "keep");
        assert_eq!(req.messages[0]["providerMetadata"], metadata);
        req.messages[0]["tool_calls"][0]["id"] = json!("different-call");
        assert!(request_body(&settings, &req).unwrap_err().contains("ID"));
    }

    #[test]
    fn normalizes_request_parameters_and_rejects_unsupported_ones() {
        let settings = super::super::test_settings(json!({"body":{"temperature":1.5}}));
        let mut req = ChatRequest {
            options: json!({"temperature":0.5,"maxOutputTokens":100,"reasoningEffort":"high","reasoning":{"summary":"concise"},"textVerbosity":"low","serviceTier":"priority"}),
            ..Default::default()
        };
        let body = request_body(&settings, &req).unwrap();
        assert_eq!(body["temperature"], 0.5);
        assert_eq!(body["max_output_tokens"], 100);
        assert_eq!(body["reasoning"]["effort"], "high");
        assert_eq!(body["reasoning"]["summary"], "concise");
        assert_eq!(body["text"]["verbosity"], "low");
        req.options = json!({"stopSequences":["stop"]});
        assert!(request_body(&settings, &req)
            .unwrap_err()
            .contains("unsupported"));
    }
    #[test]
    fn remote_files_use_file_url_and_unsupported_parts_fail() {
        let parts = crate::provider::content::blocks(&json!([{"type":"file","file":{"file_data":"https://example.test/spec.pdf","filename":"spec.pdf"}}]), crate::provider::Protocol::Responses, false).unwrap();
        assert_eq!(parts[0]["file_url"], "https://example.test/spec.pdf");
        assert!(parts[0].get("file_data").is_none());
        assert!(crate::provider::content::blocks(
            &json!([{"type":"input_audio","input_audio":{"data":"AA==","format":"wav"}}]),
            crate::provider::Protocol::Responses,
            false
        )
        .is_err());
    }
    #[test]
    fn preserves_encrypted_reasoning_and_terminal_tool_calls() {
        let mut state = ResponseState::default();
        state.consume(&json!({"type":"response.output_item.added","output_index":1,"item":{"type":"function_call","id":"item","call_id":"call","name":"read","arguments":""}})).unwrap();
        state.consume(&json!({"type":"response.function_call_arguments.delta","item_id":"item","delta":"{\"filePath\":\"a\"}"})).unwrap();
        state.consume(&json!({"type":"response.completed","response":{"output":[{"type":"reasoning","encrypted_content":"opaque","summary":[]},{"type":"function_call","call_id":"call","name":"read","arguments":"{\"filePath\":\"a\"}","status":"completed"}],"usage":{"input_tokens":20,"output_tokens":10}}})).unwrap();
        let events = state.finish("m").unwrap();
        assert!(
            matches!(&events[0], ProviderEvent::ToolCallStart { call_id, .. } if call_id == "call")
        );
        assert!(
            matches!(&events[1], ProviderEvent::Metadata(value) if value["output"][0]["encrypted_content"] == "opaque")
        );
    }
    #[test]
    fn avoids_duplicate_text_and_reports_truncation() {
        let mut state = ResponseState::default();
        assert_eq!(
            state
                .consume(
                    &json!({"type":"response.output_text.delta","output_index":0,"delta":"Hi"})
                )
                .unwrap()
                .len(),
            1
        );
        assert!(state.consume(&json!({"type":"response.completed","response":{"output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Hi"}]}]}})).unwrap().is_empty());
        assert!(ResponseState::default().finish("m").is_err());
        assert!(ResponseState::default()
            .consume(&json!({"type":"response.failed","response":{"error":{"message":"denied"}}}))
            .is_err());
    }
}
