use super::{
    error_message, json_response, read_response, usage_openai, ChatRequest, EventStream,
    ProviderError, Settings,
};
use crate::types::{ProviderEvent, Tokens};
use async_stream::stream;
use futures::StreamExt;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};

fn content_parts(value: &Value, assistant: bool) -> Result<Vec<Value>, String> {
    let text_type = if assistant {
        "output_text"
    } else {
        "input_text"
    };
    if let Some(text) = value.as_str() {
        return Ok(if text.is_empty() {
            Vec::new()
        } else {
            vec![json!({"type":text_type,"text":text})]
        });
    }
    if value.is_null() {
        return Ok(Vec::new());
    }
    let parts = value
        .as_array()
        .ok_or("Responses message content must be text or an array")?;
    let mut output = Vec::new();
    for part in parts {
        match part["type"].as_str().unwrap_or_default() {
            "text" | "input_text" | "output_text" => {
                output.push(json!({"type":text_type,"text":part["text"]}))
            }
            "image_url" => {
                let mut image = json!({"type":"input_image","image_url":part["image_url"]["url"]});
                if let Some(detail) = part["image_url"].get("detail") {
                    image["detail"] = detail.clone();
                }
                output.push(image);
            }
            "input_image" | "input_file" => output.push(part.clone()),
            "file" => {
                let file = part
                    .get("file")
                    .ok_or("file attachment is missing its file object")?;
                if !file.is_object() {
                    return Err("file attachment must be an object".into());
                }
                let mut file = file.clone();
                if file["file_data"]
                    .as_str()
                    .is_some_and(|url| url.starts_with("https://") || url.starts_with("http://"))
                {
                    let url = file.as_object_mut().unwrap().remove("file_data").unwrap();
                    file["file_url"] = url;
                }
                file["type"] = json!("input_file");
                output.push(file);
            }
            other => {
                return Err(format!(
                    "OpenAI Responses does not support message part type {other}"
                ))
            }
        }
    }
    Ok(output)
}

fn request_body(settings: &Settings, req: &ChatRequest) -> Result<Value, String> {
    let mut input = Vec::new();
    for message in &req.messages {
        let role = message["role"].as_str().ok_or("message has no role")?;
        if role == "tool" {
            let output = if let Some(text) = message["content"].as_str() {
                json!(text)
            } else {
                json!(content_parts(&message["content"], false)?)
            };
            input.push(json!({"type":"function_call_output","call_id":message["tool_call_id"],"output":output}));
            continue;
        }
        if role == "assistant" && message["providerMetadata"]["provider"] == "openai-responses" {
            if let Some(items) = message["providerMetadata"]["output"]
                .as_array()
                .filter(|items| !items.is_empty())
            {
                input.extend(items.iter().cloned());
                continue;
            }
        }
        let content = content_parts(&message["content"], role == "assistant")?;
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
    for (key, wire) in [
        ("temperature", "temperature"),
        ("topP", "top_p"),
        ("parallelToolCalls", "parallel_tool_calls"),
        ("store", "store"),
        ("include", "include"),
        ("serviceTier", "service_tier"),
        ("user", "user"),
        ("reasoning", "reasoning"),
        ("text", "text"),
        ("tool_choice", "tool_choice"),
        ("metadata", "metadata"),
    ] {
        if let Some(value) = settings.options.get(key) {
            body[wire] = value.clone();
        }
    }
    for key in ["reasoning", "text"] {
        if body
            .get(key)
            .is_some_and(|value| !value.is_object() && !value.is_null())
        {
            return Err(format!("Responses option {key} must be an object"));
        }
    }
    if let Some(effort) = super::reasoning_effort(settings, req) {
        body["reasoning"]["effort"] = json!(effort);
    }
    if let Some(summary) = settings.options["reasoningSummary"].as_str() {
        body["reasoning"]["summary"] = json!(summary);
    } else if body.get("reasoning").is_some() {
        body["reasoning"]["summary"] = json!("auto");
    }
    if let Some(verbosity) = settings.options["textVerbosity"].as_str() {
        body["text"]["verbosity"] = json!(verbosity);
    }
    if let Some(limit) = settings.options["maxTokens"]
        .as_u64()
        .or_else(|| settings.options["maxOutputTokens"].as_u64())
    {
        body["max_output_tokens"] = json!(limit);
    }
    settings.overlay_body(&mut body);
    body["model"] = json!(settings.api_model);
    body["stream"] = json!(true);
    if !settings.tools_supported {
        body.as_object_mut().unwrap().remove("tools");
        body.as_object_mut().unwrap().remove("tool_choice");
    }
    if settings.codex {
        body["store"] = json!(false);
        body.as_object_mut().unwrap().remove("max_output_tokens");
        let mut include = body["include"].as_array().cloned().unwrap_or_default();
        if !include.iter().any(|v| v == "reasoning.encrypted_content") {
            include.push(json!("reasoning.encrypted_content"));
        }
        body["include"] = json!(include);
    }
    Ok(body)
}

pub(super) async fn stream(
    settings: Settings,
    req: &ChatRequest,
) -> Result<EventStream, ProviderError> {
    let response = settings
        .send(
            "responses",
            &request_body(&settings, req).map_err(ProviderError::fatal)?,
        )
        .await?;
    let json = json_response(&response);
    let stream = stream! {
        let mut state = ResponseState::default();
        if json {
            let value = match read_response(response, settings.chunk_timeout, 16 * 1024 * 1024).await.and_then(|bytes| serde_json::from_slice::<Value>(&bytes).map_err(|e| format!("invalid Responses JSON: {e}"))) {
                Ok(value) => value,
                Err(error) => { yield ProviderEvent::Error(error); return; }
            };
            let event = if value["status"] == "failed" || value.get("error").filter(|v| !v.is_null()).is_some() { "response.failed" } else if value["status"] == "incomplete" { "response.incomplete" } else if value["status"] == "completed" { "response.completed" } else {
                yield ProviderEvent::Error("Responses JSON response has no terminal status".into()); return;
            };
            match state.consume(&json!({"type":event,"response":value})) {
                Ok(events) => { for event in events { yield event; } }
                Err(error) => { yield ProviderEvent::Error(error); return; }
            }
        } else {
            let mut frames = super::sse::frames(response, settings.chunk_timeout);
            while let Some(frame) = frames.next().await {
                let frame = match frame { Ok(frame) => frame, Err(error) => { yield ProviderEvent::Error(error); return; } };
                if frame.data.trim().is_empty() { continue; }
                if frame.data.trim() == "[DONE]" { break; }
                let mut value: Value = match serde_json::from_str(&frame.data) {
                    Ok(value) => value,
                    Err(error) => { yield ProviderEvent::Error(format!("invalid Responses SSE JSON: {error}")); return; }
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
                let arguments = if kind.ends_with(".done") {
                    value["arguments"]
                        .as_str()
                        .ok_or("Responses arguments.done is missing arguments")?
                        .to_string()
                } else {
                    let mut arguments = item["arguments"].as_str().unwrap_or_default().to_string();
                    arguments.push_str(value["delta"].as_str().unwrap_or_default());
                    arguments
                };
                if arguments.len() > 16 * 1024 * 1024 {
                    return Err("Responses tool arguments exceed 16 MiB".into());
                }
                item["arguments"] = json!(arguments);
                Ok(Vec::new())
            }
            "response.completed" | "response.incomplete" => {
                let response = &value["response"];
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
    fn remote_files_use_file_url_and_unsupported_parts_fail() {
        let parts = content_parts(&json!([{"type":"file","file":{"file_data":"https://example.test/spec.pdf","filename":"spec.pdf"}}]), false).unwrap();
        assert_eq!(parts[0]["file_url"], "https://example.test/spec.pdf");
        assert!(parts[0].get("file_data").is_none());
        assert!(content_parts(
            &json!([{"type":"input_audio","input_audio":{"data":"AA==","format":"wav"}}]),
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
