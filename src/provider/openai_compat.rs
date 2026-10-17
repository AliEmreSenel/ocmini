use super::{ChatRequest, ProviderAdapter};
use crate::auth::{AuthEntry, AuthStore};
use crate::config::ProviderCfg;
use crate::types::{ProviderEvent, Tokens};
use async_stream::stream;
use async_trait::async_trait;
use futures::StreamExt;
use serde_json::{json, Value};
use std::collections::BTreeMap;

pub struct OpenAiCompatAdapter {
    pub provider_id: String,
    pub base_url: String,
    pub api_key: String,
    pub extra_headers: Vec<(String, String)>,
}

impl OpenAiCompatAdapter {
    pub fn new(provider_id: &str, cfg: Option<&ProviderCfg>, auth: &AuthStore) -> Self {
        let mut base_url = cfg.map(|c| c.options.base_url.clone()).unwrap_or_default();
        let mut api_key = cfg.map(|c| c.options.api_key.clone()).unwrap_or_default();
        if base_url.is_empty() {
            base_url = match provider_id {
                "openrouter" => "https://openrouter.ai/api/v1".to_string(),
                "github-copilot" => "https://api.githubcopilot.com".to_string(),
                _ => "https://api.openai.com/v1".to_string(),
            };
        }
        if api_key.is_empty() {
            match auth.get(provider_id) {
                Some(AuthEntry::Api { key }) => api_key = key.clone(),
                Some(AuthEntry::Oauth { access, .. }) => api_key = access.clone(),
                Some(AuthEntry::WellKnown { key, .. }) => api_key = key.clone(),
                None => {}
            }
        }
        let mut extra_headers: Vec<(String, String)> = Vec::new();
        if provider_id == "github-copilot" {
            if api_key.is_empty() {
                if let Some(AuthEntry::Oauth { access, .. }) = auth.get(provider_id) {
                    api_key = access.clone();
                }
            }
            extra_headers.push(("OpenAI-Intent".into(), "agent".into()));
            extra_headers.push(("x-initiator".into(), "agent".into()));
            extra_headers.push(("User-Agent".into(), "ocmini".into()));
        }
        Self {
            provider_id: provider_id.to_string(),
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key,
            extra_headers,
        }
    }
}

#[async_trait]
impl ProviderAdapter for OpenAiCompatAdapter {
    async fn stream(
        &self,
        req: ChatRequest,
    ) -> Result<Box<dyn futures::Stream<Item = ProviderEvent> + Send + Unpin>, String> {
        let mut all_messages = vec![json!({"role": "system", "content": req.system})];
        all_messages.extend(req.messages);
        let mut body = json!({
            "model": req.model,
            "stream": true,
            "stream_options": {"include_usage": true},
            "messages": all_messages,
        });
        if !req.tools.is_empty() {
            body["tools"] = Value::Array(req.tools);
        }
        let mut request = reqwest::Client::new()
            .post(format!("{}/chat/completions", self.base_url))
            .bearer_auth(&self.api_key)
            .json(&body);
        for (k, v) in &self.extra_headers {
            request = request.header(k.as_str(), v.as_str());
        }
        let resp = request
            .send()
            .await
            .map_err(|e| format!("provider request failed: {e}"))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(format!("provider {status}: {text}"));
        }
        let bytes = resp.bytes_stream();
        let s = stream! {
            let mut bytes = bytes;
            let mut buf = String::new();
            let mut acc: BTreeMap<usize, (String, String, String)> = Default::default();
            let mut usage = Tokens::default();
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
                    if data == "[DONE]" {
                        done = true;
                        break;
                    }
                    let Ok(v) = serde_json::from_str::<Value>(data) else {
                        continue;
                    };
                    if let Some(u) = v.get("usage") {
                        usage.input = u["prompt_tokens"].as_i64().unwrap_or(usage.input);
                        usage.output = u["completion_tokens"].as_i64().unwrap_or(usage.output);
                        usage.reasoning = u["completion_tokens_details"]["reasoning_tokens"]
                            .as_i64()
                            .unwrap_or(usage.reasoning);
                        usage.cache.read = u["prompt_tokens_details"]["cached_tokens"]
                            .as_i64()
                            .unwrap_or(usage.cache.read);
                    }
                    let Some(choice) = v["choices"].get(0) else {
                        continue;
                    };
                    if let Some(d) = choice["delta"]["content"].as_str() {
                        if !d.is_empty() {
                            yield ProviderEvent::TextDelta(d.to_string());
                        }
                    }
                    if let Some(d) = choice["delta"]["reasoning_content"].as_str() {
                        if !d.is_empty() {
                            yield ProviderEvent::ReasoningDelta(d.to_string());
                        }
                    }
                    if let Some(calls) = choice["delta"]["tool_calls"].as_array() {
                        for c in calls {
                            let idx = c["index"].as_u64().unwrap_or(0) as usize;
                            let e = acc
                                .entry(idx)
                                .or_insert_with(|| (String::new(), String::new(), String::new()));
                            if let Some(id) = c["id"].as_str() {
                                if !id.is_empty() {
                                    e.0 = id.to_string();
                                }
                            }
                            if let Some(n) = c["function"]["name"].as_str() {
                                e.1.push_str(n);
                            }
                            if let Some(a) = c["function"]["arguments"].as_str() {
                                e.2.push_str(a);
                            }
                        }
                    }
                    if let Some(fr) = choice["finish_reason"].as_str() {
                        finish = fr.to_string();
                    }
                }
                if done {
                    break;
                }
            }
            for (_, (id, name, args)) in acc {
                if !name.is_empty() {
                    yield ProviderEvent::ToolCallStart {
                        call_id: id,
                        name,
                        args,
                    };
                }
            }
            yield ProviderEvent::Finished {
                finish: if finish.is_empty() { "stop".to_string() } else { finish },
                usage,
            };
        };
        { let boxed: Box<dyn futures::Stream<Item = ProviderEvent> + Send + Unpin> = Box::new(Box::pin(s)); Ok(boxed) }
    }
}
