pub mod anthropic;
pub mod openai_compat;
pub mod responses;
pub mod sse;
mod catalog;
pub use catalog::discover_models;

use crate::auth::{configured_token, AuthEntry, AuthStore};
use crate::config::{deep_merge, ProviderCfg};
use crate::types::{ProviderEvent, Tokens};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::time::Duration;

pub struct ChatRequest {
    pub model: String,
    pub system: String,
    pub messages: Vec<Value>,
    pub tools: Vec<Value>,
    pub variant: Option<String>,
    pub session_id: Option<String>,
}

pub type EventStream = Box<dyn futures::Stream<Item = ProviderEvent> + Send + Unpin>;

/// Provider failures carry their retry policy: flaky transports (connection
/// resets, mid-body deaths, 5xx/429, overloaded events) must be retried with
/// the provider's own Retry-After hint where available.
#[derive(Debug, Clone)]
pub struct ProviderError {
    pub message: String,
    pub retryable: bool,
    pub retry_after: Option<Duration>,
    /// The provider rejected the request because the prompt exceeds the
    /// model's context window. Retrying the same request can never work;
    /// the engine compacts the history and retries once.
    pub overflow: bool,
}

impl ProviderError {
    pub fn fatal(message: impl Into<String>) -> Self {
        Self { message: message.into(), retryable: false, retry_after: None, overflow: false }
    }
}

impl std::fmt::Display for ProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// Status codes that are safe to re-request: the request never completed.
/// One pooled HTTP client for the whole process: connection reuse matters
/// when many sessions request providers at the same time.
pub fn client() -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(|| reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .redirect(reqwest::redirect::Policy::limited(10))
        .pool_idle_timeout(Duration::from_secs(90))
        .build().expect("process HTTP client"))
}

pub fn status_retryable(status: reqwest::StatusCode) -> bool {
    matches!(status.as_u16(), 408 | 409 | 425 | 429 | 500 | 502 | 503 | 504)
}

/// Mid-stream failures observed before any tool executed. Transport deaths,
/// stalled streams and provider overload events are transient; malformed
/// protocol data is not.
pub fn stream_retryable(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    ["overload", "rate limit", "rate_limit", "stream failed", "stream timed out",
     "stream ended before", "decoding response body", "connection", "connection reset",
     "broken pipe", "timed out", "tls", "dns", "transport", "body stream"]
        .iter().any(|marker| lower.contains(marker))
}

/// Requests rejected for exceeding the model's context window. Retrying the
/// identical request can never succeed; the engine compacts and retries.
pub fn context_overflow(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    ["maximum context length", "context length exceeded", "prompt is too long",
     "reduce the length of the input prompt", "input tokens are too long",
     "exceeds the maximum context", "exceed the model's maximum", "context window exceeded",
     "token limit exceeded", "maximum context window"]
        .iter().any(|marker| lower.contains(marker))
}

/// The window size a provider names in its overflow error ("maximum context
/// length is 262144 tokens", "213000 tokens > 200000 maximum"), so compaction
/// can target the real limit even for models absent from the catalog.
pub fn parse_context_limit(message: &str) -> Option<u64> {
    fn first_number(text: &str) -> Option<u64> {
        let start = text.find(|c: char| c.is_ascii_digit())?;
        text[start..].chars().take_while(char::is_ascii_digit).collect::<String>().parse().ok()
    }
    let lower = message.to_ascii_lowercase();
    for marker in ["maximum context length is", "context length of", "maximum context window of", "tokens >"] {
        if let Some(index) = lower.find(marker) {
            if let Some(limit) = first_number(&lower[index + marker.len()..]) { return Some(limit); }
        }
    }
    None
}

#[async_trait]
pub trait ProviderAdapter: Send + Sync {
    async fn stream(&self, req: ChatRequest) -> Result<EventStream, ProviderError>;
}

struct Adapter {
    provider_id: String,
    cfg: ProviderCfg,
    configured: bool,
    auth: AuthStore,
}

pub fn build(provider_id: &str, cfg: Option<&ProviderCfg>, auth: &AuthStore) -> Box<dyn ProviderAdapter> {
    Box::new(Adapter { provider_id: provider_id.into(), cfg: cfg.cloned().unwrap_or_default(), configured: cfg.is_some(), auth: auth.clone() })
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Protocol { Chat, Responses, Anthropic }

pub(super) struct Settings {
    pub provider_id: String,
    pub base_url: String,
    pub api_model: String,
    pub token: String,
    pub headers: BTreeMap<String, String>,
    pub bearer: bool,
    pub codex: bool,
    pub options: Value,
    pub max_output: Option<u64>,
    pub tools_supported: bool,
    pub header_timeout: Option<Duration>,
    pub total_timeout: Option<Duration>,
    pub chunk_timeout: Duration,
}

#[async_trait]
impl ProviderAdapter for Adapter {
    async fn stream(&self, req: ChatRequest) -> Result<EventStream, ProviderError> {
        let (protocol, settings) = self.prepare(&req).await.map_err(ProviderError::fatal)?;
        match protocol {
            Protocol::Chat => openai_compat::stream(settings, req).await,
            Protocol::Responses => responses::stream(settings, req).await,
            Protocol::Anthropic => anthropic::stream(settings, req).await,
        }
    }
}

impl Adapter {
    async fn prepare(&self, req: &ChatRequest) -> Result<(Protocol, Settings), String> {
        let model = self.cfg.models.get(&req.model);
        let mut options = serde_json::to_value(&self.cfg.options).map_err(|e| e.to_string())?;
        if let Some(api) = self.cfg.extra.get("api").and_then(Value::as_str) { options["baseURL"] = json!(api); }
        if let Some(model) = model {
            for key in ["options", "settings", "request"] {
                if let Some(value) = model.extra.get(key) {
                    if !value.is_object() { return Err(format!("model {} {key} must be an object", req.model)); }
                    deep_merge(&mut options, value.clone());
                }
            }
            if let Some(headers) = model.extra.get("headers") { deep_merge(&mut options["headers"], headers.clone()); }
            if let Some(body) = model.extra.get("body") { deep_merge(&mut options["body"], body.clone()); }
            if let Some(api) = model.extra.get("provider").and_then(|v| v.get("api")).and_then(Value::as_str) { options["baseURL"] = json!(api); }
            if let Some(variant) = &req.variant {
                if let Some(value) = model.extra.get("variants").and_then(|v| v.get(variant)) {
                    if !value.is_object() { return Err(format!("variant {variant} for {} must be an object", req.model)); }
                    if value["disabled"] == true { return Err(format!("variant {variant} is disabled for model {}", req.model)); }
                    deep_merge(&mut options, value.clone());
                } else if !["none", "minimal", "low", "medium", "high", "xhigh", "max", "ultra"].contains(&variant.as_str()) {
                    return Err(format!("unknown model variant {variant} for {}", req.model));
                }
            }
            if model.extra.get("disabled").and_then(Value::as_bool) == Some(true) { return Err(format!("model {} is disabled", req.model)); }
        }
        for key in ["headers", "body", "extraBody"] {
            if options.get(key).is_some_and(|value| !value.is_object() && !value.is_null()) { return Err(format!("provider option {key} must be an object")); }
        }
        if let Some(transport) = options["transport"].as_str() {
            if !["http", "sse", "auto"].contains(&transport) {
                return Err(format!("provider transport {transport} is not supported; configure transport as http or sse"));
            }
        }
        let mut token = options["apiKey"].as_str().unwrap_or_default().to_string();
        let mut bearer = options["apiKey"].as_str().unwrap_or_default().is_empty() && ["authToken", "accessToken"].iter().any(|key| options[*key].as_str().is_some_and(|value| !value.is_empty()));
        if token.is_empty() { token = options["authToken"].as_str().or_else(|| options["accessToken"].as_str()).unwrap_or_default().into(); }
        if token.is_empty() {
            token = configured_token(&self.provider_id, Some(&self.cfg));
            if self.provider_id == "anthropic" && std::env::var("ANTHROPIC_API_KEY").unwrap_or_default().is_empty() && std::env::var("ANTHROPIC_AUTH_TOKEN").ok().as_deref() == Some(token.as_str()) && !token.is_empty() { bearer = true; }
        }
        let credential = if token.is_empty() { self.auth.credential(&self.provider_id).await? } else { None };
        let oauth = matches!(credential, Some(AuthEntry::Oauth { .. }));
        let codex = oauth && ["openai", "openai-codex"].contains(&self.provider_id.as_str());
        let mut headers = BTreeMap::new();
        for (key, value) in &self.cfg.headers { headers.insert(key.to_lowercase(), value.clone()); }
        if let Some(map) = options["headers"].as_object() {
            for (key, value) in map {
                if let Some(value) = value.as_str() { headers.insert(key.to_lowercase(), value.to_string()); }
                else { return Err(format!("provider header {key} must be a string")); }
            }
        }
        let mut base_url = options["baseURL"].as_str().unwrap_or_default().trim_end_matches('/').to_string();
        if base_url.is_empty() { base_url = self.cfg.extra.get("api").and_then(Value::as_str).unwrap_or_default().trim_end_matches('/').to_string(); }
        if base_url.is_empty() {
            let variable = match self.provider_id.as_str() { "openai" => "OPENAI_BASE_URL", "anthropic" => "ANTHROPIC_BASE_URL", "vllm" => "VLLM_BASE_URL", _ => "" };
            if !variable.is_empty() { base_url = std::env::var(variable).unwrap_or_default().trim_end_matches('/').to_string(); }
        }
        match credential {
            Some(AuthEntry::Api { key }) => token = key,
            Some(AuthEntry::Oauth { access, refresh, enterprise_url, account_id, metadata, .. }) => {
                token = if self.provider_id == "github-copilot" && !refresh.is_empty() { refresh } else { access };
                bearer = true;
                if codex {
                    if base_url.is_empty() || base_url.starts_with("https://api.openai.com") { base_url = "https://chatgpt.com/backend-api/codex".into(); }
                    let account = account_id.or_else(|| metadata["accountID"].as_str().or_else(|| metadata["accountId"].as_str()).map(String::from)).or_else(|| crate::auth::account_from_jwt(&token));
                    if let Some(account) = account { headers.insert("chatgpt-account-id".into(), account); }
                    headers.entry("originator".into()).or_insert_with(|| "opencode".into());
                    if let Some(session) = &req.session_id { headers.entry("session-id".into()).or_insert_with(|| session.clone()); }
                }
                if self.provider_id == "github-copilot" && base_url.is_empty() {
                    base_url = metadata["apiEndpoint"].as_str().filter(|value| !value.is_empty()).map(String::from).unwrap_or_else(|| {
                        let enterprise = enterprise_url.or_else(|| metadata["enterpriseUrl"].as_str().map(String::from));
                        enterprise.map(|url| format!("https://copilot-api.{}", url.trim_start_matches("https://").trim_start_matches("http://").trim_end_matches('/'))).unwrap_or_default()
                    });
                }
            }
            Some(AuthEntry::WellKnown { .. }) => return Err(format!("provider {} uses well-known discovery credentials; configure an explicit provider baseURL and API key instead of treating discovery data as an API token", self.provider_id)),
            None => {}
        }
        let npm = model.and_then(|m| m.extra.get("provider")).and_then(|v| v.get("npm")).and_then(Value::as_str).unwrap_or(&self.cfg.npm);
        let explicit = options["endpoint"].as_str().or_else(|| options["api"].as_str());
        let protocol = protocol(&self.provider_id, npm, explicit, &req.model, codex)?;
        if base_url.is_empty() {
            base_url = default_base_url(&self.provider_id, protocol).ok_or_else(|| format!("provider {} has no supported default endpoint; configure provider.{}.options.baseURL", self.provider_id, self.provider_id))?.into();
        }
        if !self.configured && !["openai", "openai-codex", "anthropic", "github-copilot", "openrouter", "groq", "xai", "deepseek", "mistral", "togetherai", "ollama", "vllm", "lmstudio"].contains(&self.provider_id.as_str()) {
            return Err(format!("unknown provider {}; define it in OpenCode configuration", self.provider_id));
        }
        let parsed = reqwest::Url::parse(&base_url).map_err(|e| format!("invalid provider baseURL: {e}"))?;
        if !["http", "https"].contains(&parsed.scheme()) { return Err("provider baseURL must use http or https".into()); }
        if token.is_empty() && !headers.contains_key("authorization") && !headers.contains_key("x-api-key") && parsed.host_str().map(|host| ["api.openai.com", "api.anthropic.com", "api.githubcopilot.com", "openrouter.ai"].contains(&host)).unwrap_or(false) {
            return Err(format!("missing credentials for {}; set its API key environment variable, options.apiKey, or connect it with OpenCode", self.provider_id));
        }
        if self.provider_id == "github-copilot" {
            headers.entry("openai-intent".into()).or_insert_with(|| "conversation-edits".into());
            headers.entry("x-github-api-version".into()).or_insert_with(|| "2026-08-01".into());
            headers.entry("x-interaction-type".into()).or_insert_with(|| "conversation-agent".into());
            headers.entry("x-initiator".into()).or_insert_with(|| if req.messages.last().map(|m| m["role"] == "user").unwrap_or(false) { "user".into() } else { "agent".into() });
            if let Some(session) = &req.session_id { headers.entry("x-interaction-id".into()).or_insert_with(|| session.clone()); }
            if protocol == Protocol::Anthropic { headers.entry("anthropic-beta".into()).or_insert_with(|| "interleaved-thinking-2025-05-14".into()); }
            if req.messages.iter().any(|m| m["content"].as_array().map(|parts| parts.iter().any(|p| ["image_url", "image", "input_image"].contains(&p["type"].as_str().unwrap_or("")))).unwrap_or(false)) { headers.insert("copilot-vision-request".into(), "true".into()); }
        }
        if protocol == Protocol::Anthropic {
            let path = parsed.path().trim_end_matches('/');
            if path.is_empty() { base_url.push_str("/v1"); }
        }
        let api_model = model.map(|m| m.extra.get("api").and_then(|v| v.get("id")).and_then(Value::as_str).filter(|v| !v.is_empty()).unwrap_or(if m.id.is_empty() { &req.model } else { &m.id }).to_string()).unwrap_or_else(|| req.model.clone());
        let max_output = model.and_then(|m| m.limit.as_ref()).and_then(|v| v["output"].as_u64());
        let header_timeout = if options["headerTimeout"] == false { None } else { Some(Duration::from_millis(options["headerTimeout"].as_u64().filter(|n| *n > 0).unwrap_or(60_000))) };
        let total_timeout = options["timeout"].as_u64().filter(|n| *n > 0).map(Duration::from_millis);
        let chunk_timeout = Duration::from_millis(options["chunkTimeout"].as_u64().filter(|n| *n > 0).unwrap_or(120_000));
        let tools_supported = model.and_then(|m| m.extra.get("tool_call").and_then(Value::as_bool).or_else(|| m.extra.get("capabilities").and_then(|v| v.get("tools")).and_then(Value::as_bool))).unwrap_or(true);
        Ok((protocol, Settings { provider_id: self.provider_id.clone(), base_url, api_model, token, headers, bearer, codex, options, max_output, tools_supported, header_timeout, total_timeout, chunk_timeout }))
    }
}

fn protocol(provider: &str, npm: &str, endpoint: Option<&str>, model: &str, codex: bool) -> Result<Protocol, String> {
    if let Some(endpoint) = endpoint {
        return match endpoint {
            "responses" | "/responses" => Ok(Protocol::Responses),
            "chat" | "chat/completions" | "/chat/completions" => Ok(Protocol::Chat),
            "messages" | "/messages" | "anthropic" => Ok(Protocol::Anthropic),
            other => Err(format!("unsupported provider endpoint {other}; supported endpoints are chat, responses, and messages")),
        };
    }
    if codex { return Ok(Protocol::Responses); }
    if npm.contains("anthropic") { return Ok(Protocol::Anthropic); }
    if npm.contains("openai-compatible-responses") { return Ok(Protocol::Responses); }
    if npm.contains("openai-compatible") { return Ok(Protocol::Chat); }
    if ["@openrouter/ai-sdk-provider", "@ai-sdk/openrouter", "@ai-sdk/groq", "@ai-sdk/xai", "@ai-sdk/deepseek", "@ai-sdk/mistral", "@ai-sdk/togetherai"].contains(&npm) { return Ok(Protocol::Chat); }
    if provider == "github-copilot" || npm.contains("github-copilot") {
        if model.starts_with("claude-") { return Ok(Protocol::Anthropic); }
        let major = model.strip_prefix("gpt-").and_then(|v| v.split(['.', '-']).next()).and_then(|v| v.parse::<u32>().ok());
        return Ok(if major.map(|v| v >= 5).unwrap_or(false) && !model.starts_with("gpt-5-mini") || model.starts_with("grok-") || model.starts_with("mai-code-") { Protocol::Responses } else { Protocol::Chat });
    }
    if !npm.is_empty() && !["@ai-sdk/openai", "@opencode/ai/providers/openai"].contains(&npm) {
        return Err(format!("provider SDK {npm} requires a protocol not implemented by ocmini. Configure an OpenAI-compatible, Responses, or Anthropic endpoint explicitly."));
    }
    Ok(if provider == "anthropic" { Protocol::Anthropic } else if provider == "openai" || provider == "openai-codex" || !npm.is_empty() { Protocol::Responses } else { Protocol::Chat })
}

fn default_base_url(provider: &str, _protocol: Protocol) -> Option<&'static str> {
    Some(match provider {
        "openai" | "openai-codex" => "https://api.openai.com/v1",
        "anthropic" => "https://api.anthropic.com/v1",
        "github-copilot" => "https://api.githubcopilot.com",
        "openrouter" => "https://openrouter.ai/api/v1",
        "groq" => "https://api.groq.com/openai/v1",
        "xai" => "https://api.x.ai/v1",
        "deepseek" => "https://api.deepseek.com/v1",
        "mistral" => "https://api.mistral.ai/v1",
        "togetherai" => "https://api.together.xyz/v1",
        "ollama" => "http://127.0.0.1:11434/v1",
        "vllm" => "http://127.0.0.1:8000/v1",
        "lmstudio" => "http://127.0.0.1:1234/v1",
        _ => return None,
    })
}

impl Settings {
    pub async fn send(&self, endpoint: &str, body: &Value) -> Result<reqwest::Response, ProviderError> {
        use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
        let mut headers = HeaderMap::new();
        headers.insert("accept", HeaderValue::from_static("text/event-stream, application/json"));
        headers.insert("user-agent", HeaderValue::from_static("ocmini"));
        if !self.token.is_empty() {
            let (name, value) = if endpoint == "messages" && !self.bearer { ("x-api-key", self.token.clone()) } else { ("authorization", format!("Bearer {}", self.token)) };
            headers.insert(HeaderName::from_bytes(name.as_bytes()).map_err(|e| ProviderError::fatal(e.to_string()))?, HeaderValue::from_str(&value).map_err(|_| ProviderError::fatal("provider credential contains invalid header characters"))?);
        }
        if endpoint == "messages" { headers.insert("anthropic-version", HeaderValue::from_static("2023-06-01")); }
        for (name, value) in &self.headers {
            headers.insert(HeaderName::from_bytes(name.as_bytes()).map_err(|e| ProviderError::fatal(format!("invalid provider header {name}: {e}")))?, HeaderValue::from_str(value).map_err(|_| ProviderError::fatal(format!("invalid value for provider header {name}")))?);
        }
        let suffix = format!("/{endpoint}");
        let url = if self.base_url.ends_with(&suffix) { self.base_url.clone() } else { format!("{}{suffix}", self.base_url.trim_end_matches('/')) };
        let mut request = client().post(url).headers(headers).json(body);
        if let Some(timeout) = self.total_timeout { request = request.timeout(timeout); }
        let response = if let Some(timeout) = self.header_timeout {
            tokio::time::timeout(timeout, request.send()).await
                .map_err(|_| ProviderError { message: format!("{} did not return response headers within {} seconds", self.provider_id, timeout.as_secs()), retryable: true, retry_after: None, overflow: false })?
                .map_err(|e| ProviderError { message: format!("{} request failed: {}", self.provider_id, e.without_url()), retryable: true, retry_after: None, overflow: false })?
        } else {
            request.send().await.map_err(|e| ProviderError { message: format!("{} request failed: {}", self.provider_id, e.without_url()), retryable: true, retry_after: None, overflow: false })?
        };
        if !response.status().is_success() {
            let status = response.status();
            let retry_after = response.headers().get("retry-after").and_then(|value| value.to_str().ok())
                .and_then(|value| value.trim().parse::<u64>().ok())
                .map(|seconds| Duration::from_secs(seconds.min(120)));
            let value = read_response(response, self.chunk_timeout, 64 * 1024).await.unwrap_or_default();
            let message = serde_json::from_slice::<Value>(&value).ok().map(|v| error_message(&v)).unwrap_or_else(|| String::from_utf8_lossy(&value).chars().take(2000).collect());
            let message = if self.token.is_empty() { message } else { message.replace(&self.token, "[redacted]") };
            let retryable = status_retryable(status);
            let overflow = matches!(status.as_u16(), 400 | 413 | 422) && context_overflow(&message);
            return Err(ProviderError { message: format!("{} HTTP {status}: {message}", self.provider_id), retryable, retry_after: if retryable { retry_after } else { None }, overflow });
        }
        Ok(response)
    }

    pub fn overlay_body(&self, body: &mut Value) {
        for key in ["body", "extraBody"] {
            if let Some(overlay) = self.options.get(key).filter(|v| v.is_object()) { deep_merge(body, overlay.clone()); }
        }
    }
}

pub(super) fn error_message(value: &Value) -> String {
    value["error"]["message"].as_str().or_else(|| value["message"].as_str()).or_else(|| value["error"].as_str()).unwrap_or("provider returned an error").chars().take(2000).collect()
}

pub(super) fn reasoning_effort<'a>(settings: &'a Settings, req: &'a ChatRequest) -> Option<&'a str> {
    settings.options["reasoningEffort"].as_str().or_else(|| req.variant.as_deref().filter(|variant| ["none", "minimal", "low", "medium", "high", "xhigh", "max", "ultra"].contains(variant)))
}

pub(super) async fn read_response(mut response: reqwest::Response, timeout: Duration, limit: usize) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    loop {
        let chunk = tokio::time::timeout(timeout, response.chunk()).await.map_err(|_| "provider response body timed out")?.map_err(|e| e.to_string())?;
        let Some(chunk) = chunk else { return Ok(bytes); };
        if bytes.len() + chunk.len() > limit { return Err(format!("provider response exceeds {limit} bytes")); }
        bytes.extend_from_slice(&chunk);
    }
}

pub(super) fn json_response(response: &reqwest::Response) -> bool {
    response.headers().get("content-type").and_then(|v| v.to_str().ok()).map(|v| v.contains("application/json") && !v.contains("event-stream")).unwrap_or(false)
}

pub(super) fn usage_openai(value: &Value, responses: bool) -> Tokens {
    let input = value[if responses { "input_tokens" } else { "prompt_tokens" }].as_i64().unwrap_or(0).max(0);
    let output = value[if responses { "output_tokens" } else { "completion_tokens" }].as_i64().unwrap_or(0).max(0);
    let cache = value[if responses { "input_tokens_details" } else { "prompt_tokens_details" }]["cached_tokens"].as_i64().unwrap_or(0).max(0).min(input);
    let reasoning = value[if responses { "output_tokens_details" } else { "completion_tokens_details" }]["reasoning_tokens"].as_i64().unwrap_or(0).max(0).min(output);
    Tokens { input: input - cache, output: output - reasoning, reasoning, cache: crate::types::Cache { read: cache, write: 0 } }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn custom_variants_apply_options_without_sending_the_variant_name_as_effort() {
        let cfg: ProviderCfg = serde_json::from_value(json!({"options":{"baseURL":"http://localhost/v1","apiKey":"test"},"models":{"model":{"variants":{"focused":{"reasoningEffort":"high","headers":{"x-mode":"focused"}}}}}})).unwrap();
        let adapter = Adapter { provider_id: "custom".into(), cfg, configured: true, auth: AuthStore::from_json("{}").unwrap() };
        let mut req = ChatRequest { model: "model".into(), system: String::new(), messages: vec![], tools: vec![], variant: Some("focused".into()), session_id: None };
        let (_, settings) = adapter.prepare(&req).await.unwrap();
        assert_eq!(reasoning_effort(&settings, &req), Some("high"));
        assert_eq!(settings.headers.get("x-mode").map(String::as_str), Some("focused"));
        req.variant = Some("missing".into());
        assert!(adapter.prepare(&req).await.is_err());
    }
    #[test]
    fn selects_protocol_and_rejects_unimplemented_provider_sdks() {
        assert_eq!(protocol("custom", "@ai-sdk/anthropic", None, "m", false).unwrap(), Protocol::Anthropic);
        assert_eq!(protocol("openai", "", None, "m", true).unwrap(), Protocol::Responses);
        assert_eq!(protocol("github-copilot", "", None, "claude-sonnet-4-6", false).unwrap(), Protocol::Anthropic);
        assert_eq!(protocol("github-copilot", "", None, "gpt-5.5", false).unwrap(), Protocol::Responses);
        assert!(protocol("google", "@ai-sdk/google", None, "m", false).is_err());
    }
    #[test]
    fn usage_buckets_are_disjoint() {
        let usage = usage_openai(&json!({"prompt_tokens":100,"completion_tokens":50,"prompt_tokens_details":{"cached_tokens":40},"completion_tokens_details":{"reasoning_tokens":20}}), false);
        assert_eq!((usage.input, usage.output, usage.reasoning, usage.cache.read), (60, 30, 20, 40));
    }
    #[test]
    fn overflow_detection_and_limit_parsing() {
        assert!(context_overflow("pllm5 HTTP 400: This model\'s maximum context length is 262144 tokens. However, you requested 0 output tokens and your prompt contains at least 262145 input tokens"));
        assert!(context_overflow("prompt is too long: 213000 tokens > 200000 maximum"));
        assert!(!context_overflow("provider stream failed: error decoding response body"));
        assert_eq!(parse_context_limit("maximum context length is 262144 tokens. your prompt contains 262145"), Some(262144));
        assert_eq!(parse_context_limit("prompt is too long: 213000 tokens > 200000 maximum"), Some(200000));
        assert_eq!(parse_context_limit("no numbers here"), None);
    }
}
