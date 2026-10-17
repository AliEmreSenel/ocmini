use super::{ChatRequest, ProviderAdapter};
use crate::auth::{AuthEntry, AuthStore};
use crate::config::ProviderCfg;
use crate::types::{ProviderEvent, Tokens};
use async_stream::stream;
use async_trait::async_trait;
use futures::StreamExt;
use serde_json::{json, Value};
use std::collections::BTreeMap;

pub struct AnthropicAdapter {
    pub base_url: String,
    pub api_key: String,
}

impl AnthropicAdapter {
    pub fn new(_provider_id: &str, cfg: Option<&ProviderCfg>, auth: &AuthStore) -> Self {
        let mut base_url = cfg.map(|c| c.options.base_url.clone()).unwrap_or_default();
        if base_url.is_empty() {
            base_url = "https://api.anthropic.com/v1".to_string();
        }
        let mut api_key = cfg.map(|c| c.options.api_key.clone()).unwrap_or_default();
        if api_key.is_empty() {
            if let Some(AuthEntry::Api { key }) = auth.get("anthropic") {
                api_key = key.clone();
            } else if let Some(AuthEntry::Oauth { access, .. }) = auth.get("anthropic") {
                api_key = access.clone();
            }
        }
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key,
        }
    }
}

fn openai_to_anthropic(req: &ChatRequest) -> Value {
    let mut messages: Vec<Value> = Vec::new();
    for m in &req.messages {
        let role = m["role"].as_str().unwrap_or("user");
        let content = &m["content"];
        if role == "tool" {
            let call_id = m["tool_call_id"].as_str().unwrap_or_default();
            messages.push(json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": call_id, "content": content}]}));
        } else if let Some(tcs) = m["tool_calls"].as_array() {
            let mut blocks: Vec<Value> = Vec::new();
            if let Some(txt) = content.as_str() {
                if !txt.is_empty() {
                    blocks.push(json!({"type": "text", "text": txt}));
                }
            }
            for tc in tcs {
                blocks.push(json!({
                    "type": "tool_use",
                    "id": tc["id"],
                    "name": tc["function"]["name"],
                    "input": serde_json::from_str(tc["function"]["arguments"].as_str().unwrap_or("{}")).unwrap_or(json!({})),
                }));
            }
            messages.push(json!({"role": "assistant", "content": blocks}));
        } else {
            messages.push(json!({"role": role, "content": content}));
        }
    }
    let mut body = json!({
        "model": req.model,
        "max_tokens": 16000,
        "stream": true,
        "system": req.system,
        "messages": messages,
        "tools": req.tools.iter().map(|t| json!({
            "name": t["function"]["name"],
            "description": t["function"]["description"],
            "input_schema": t["function"]["parameters"],
        })).collect::<Vec<_>>(),
    });
    if let Some(v) = req.variant.as_deref().filter(|v| *v != "none") {
        let budget = match v {
            "low" => 1024,
            "medium" => 4096,
            "high" => 10240,
            _ => 24576,
        };
        body["thinking"] = json!({"type": "enabled", "budget_tokens": budget});
        body["temperature"] = json!(1);
    }
    body
}

fn map_stop_reason(r: &str) -> String {
    match r {
        "tool_use" => "tool_calls".to_string(),
        "max_tokens" => "length".to_string(),
        "end_turn" | "stop_sequence" => "stop".to_string(),
        other => other.to_string(),
    }
}

#[async_trait]
impl ProviderAdapter for AnthropicAdapter {
    async fn stream(
        &self,
        req: ChatRequest,
    ) -> Result<Box<dyn futures::Stream<Item = ProviderEvent> + Send + Unpin>, String> {
        let resp = reqwest::Client::new()
            .post(format!("{}/messages", self.base_url))
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .json(&openai_to_anthropic(&req))
            .send()
            .await
            .map_err(|e| format!("anthropic request failed: {e}"))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(format!("anthropic {status}: {text}"));
        }
        let bytes = resp.bytes_stream();
        let s = stream! {
            let mut bytes = bytes;
            let mut buf = String::new();
            let mut usage = Tokens::default();
            let mut tool_blocks: BTreeMap<u64, (String, String, String)> = Default::default();
            let mut finish = String::new();
            let mut done = false;
            while let Some(chunk) = bytes.next().await {
                match chunk {
                    Ok(c) => buf.push_str(&String::from_utf8_lossy(&c)),
                    Err(e) => {
                        yield ProviderEvent::Error(format!("stream error: {e}"));
                        done = true;
                        break;
                    }
                }
                while let Some(nl) = buf.find('\n') {
                    let line: String = buf[..nl].trim().to_string();
                    buf.drain(..=nl);
                    let Some(data) = line.strip_prefix("data:").map(str::trim) else {
                        continue;
                    };
                    let Ok(v) = serde_json::from_str::<Value>(data) else {
                        continue;
                    };
                    match v["type"].as_str().unwrap_or_default() {
                        "content_block_delta" => match &v["delta"] {
                            d if d["type"] == "text_delta" => {
                                let t = d["text"].as_str().unwrap_or_default();
                                if !t.is_empty() {
                                    yield ProviderEvent::TextDelta(t.to_string());
                                }
                            }
                            d if d["type"] == "thinking_delta" => {
                                let t = d["thinking"].as_str().unwrap_or_default();
                                if !t.is_empty() {
                                    yield ProviderEvent::ReasoningDelta(t.to_string());
                                }
                            }
                            d if d["type"] == "input_json_delta" => {
                                let idx = v["index"].as_u64().unwrap_or(0);
                                let e = tool_blocks
                                    .entry(idx)
                                    .or_insert_with(|| (String::new(), String::new(), String::new()));
                                e.2.push_str(d["partial_json"].as_str().unwrap_or_default());
                            }
                            _ => {}
                        },
                        "content_block_start" => {
                            if v["content_block"]["type"] == "tool_use" {
                                let idx = v["index"].as_u64().unwrap_or(0);
                                tool_blocks.insert(
                                    idx,
                                    (
                                        v["content_block"]["id"].as_str().unwrap_or_default().to_string(),
                                        v["content_block"]["name"].as_str().unwrap_or_default().to_string(),
                                        String::new(),
                                    ),
                                );
                            }
                        }
                        "message_start" => {
                            let u = &v["message"]["usage"];
                            usage.input = u["input_tokens"].as_i64().unwrap_or(0);
                            usage.cache.read = u["cache_read_input_tokens"].as_i64().unwrap_or(0);
                            usage.cache.write = u["cache_creation_input_tokens"].as_i64().unwrap_or(0);
                        }
                        "message_delta" => {
                            usage.output = v["usage"]["output_tokens"].as_i64().unwrap_or(usage.output);
                            if let Some(sr) = v["delta"]["stop_reason"].as_str() {
                                finish = sr.to_string();
                            }
                        }
                        "message_stop" => {
                            done = true;
                            break;
                        }
                        "error" => {
                            yield ProviderEvent::Error(
                                v["error"]["message"].as_str().unwrap_or("anthropic error").to_string(),
                            );
                        }
                        _ => {}
                    }
                }
                if done {
                    break;
                }
            }
            for (_, (id, name, args)) in tool_blocks {
                if !name.is_empty() {
                    yield ProviderEvent::ToolCallStart {
                        call_id: id,
                        name,
                        args: if args.is_empty() { "{}".into() } else { args },
                    };
                }
            }
            yield ProviderEvent::Finished {
                finish: if finish.is_empty() { "stop".to_string() } else { map_stop_reason(&finish) },
                usage,
            };
        };
        { let boxed: Box<dyn futures::Stream<Item = ProviderEvent> + Send + Unpin> = Box::new(Box::pin(s)); Ok(boxed) }
    }
}
