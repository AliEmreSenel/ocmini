use crate::types::{FailureKind, OperationError};
pub mod anthropic;
mod catalog;
mod completion;
mod content;
pub use completion::{request as request_completion, request_text};
pub(crate) mod hooks;
pub mod openai_compat;
mod options;
mod replay;
pub mod responses;
pub mod sse;
mod stream;
pub use catalog::discover_models;
pub(crate) use catalog::normalize_provider;
pub use hooks::{complete_text, prepare_hooks, TEXT_TRANSFORMED};
pub use replay::reconcile_text;

use crate::auth::{configured_credential, AuthEntry, AuthStore};
use crate::config::{deep_merge, ProviderCfg};
use crate::types::{ProviderEvent, Tokens};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::time::Duration;

#[derive(Default, Clone)]
pub struct ChatRequest {
    pub model: String,
    pub system: String,
    pub messages: Vec<Value>,
    pub tools: Vec<Value>,
    pub variant: Option<String>,
    pub session_id: Option<String>,
    /// Request-local generation options. Null is the empty/default value.
    pub options: Value,
    /// Request-local non-authentication headers; credentials stay in Settings.
    pub headers: BTreeMap<String, String>,
}

pub type EventStream = Box<dyn futures::Stream<Item = ProviderEvent> + Send + Unpin>;

/// Provider failures carry their retry policy: flaky transports (connection
/// resets, mid-body deaths, 5xx/429, overloaded events) must be retried with
/// the provider's own Retry-After hint where available.
#[derive(Debug, Clone)]
pub struct ProviderError {
    pub kind: FailureKind,
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
        Self {
            kind: FailureKind::Error,
            message: message.into(),
            retryable: false,
            retry_after: None,
            overflow: false,
        }
    }
}

impl From<ProviderError> for OperationError {
    fn from(error: ProviderError) -> Self {
        Self::new(error.kind, error.message)
    }
}

impl From<OperationError> for ProviderError {
    fn from(error: OperationError) -> Self {
        Self {
            kind: error.kind,
            ..Self::fatal(error.message)
        }
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
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .redirect(reqwest::redirect::Policy::limited(10))
            .pool_idle_timeout(Duration::from_secs(90))
            .build()
            .expect("process HTTP client")
    })
}

pub fn status_retryable(status: reqwest::StatusCode) -> bool {
    matches!(
        status.as_u16(),
        408 | 409 | 425 | 429 | 500 | 502 | 503 | 504
    )
}

/// Mid-stream failures observed before any tool executed. Transport deaths,
/// stalled streams and provider overload events are transient; malformed
/// protocol data is not.
pub fn stream_retryable(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    [
        "overload",
        "rate limit",
        "rate_limit",
        "stream failed",
        "stream timed out",
        "stream ended before",
        "decoding response body",
        "connection",
        "connection reset",
        "broken pipe",
        "timed out",
        "tls",
        "dns",
        "transport",
        "body stream",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
}

/// Requests rejected for exceeding the model's context window. Retrying the
/// identical request can never succeed; the engine compacts and retries.
pub fn context_overflow(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    [
        "maximum context length",
        "context length exceeded",
        "prompt is too long",
        "reduce the length of the input prompt",
        "input tokens are too long",
        "exceeds the maximum context",
        "exceed the model's maximum",
        "context window exceeded",
        "token limit exceeded",
        "maximum context window",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
}

/// The window size a provider names in its overflow error ("maximum context
/// length is 262144 tokens", "213000 tokens > 200000 maximum"), so compaction
/// can target the real limit even for models absent from the catalog.
pub fn parse_context_limit(message: &str) -> Option<u64> {
    fn first_number(text: &str) -> Option<u64> {
        let start = text.find(|c: char| c.is_ascii_digit())?;
        text[start..]
            .chars()
            .take_while(char::is_ascii_digit)
            .collect::<String>()
            .parse()
            .ok()
    }
    let lower = message.to_ascii_lowercase();
    for marker in [
        "maximum context length is",
        "context length of",
        "maximum context window of",
        "tokens >",
    ] {
        if let Some(index) = lower.find(marker) {
            if let Some(limit) = first_number(&lower[index + marker.len()..]) {
                return Some(limit);
            }
        }
    }
    None
}

#[async_trait]
pub trait ProviderAdapter: Send + Sync {
    async fn stream(&self, req: &ChatRequest) -> Result<EventStream, ProviderError>;
    /// Protocol `prepare` will select for this request, without building
    /// settings. Context assembly needs it before the request exists.
    fn protocol_of(&self, req: &ChatRequest) -> Result<Protocol, OperationError>;
}

struct Adapter {
    provider_id: String,
    cfg: ProviderCfg,
    configured: bool,
    auth: AuthStore,
}

pub fn build(
    provider_id: &str,
    cfg: Option<&ProviderCfg>,
    auth: &AuthStore,
) -> Box<dyn ProviderAdapter> {
    Box::new(Adapter {
        provider_id: provider_id.into(),
        cfg: cfg
            .map(|cfg| normalize_provider(provider_id, cfg))
            .unwrap_or_default(),
        configured: cfg.is_some(),
        auth: auth.clone(),
    })
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Protocol {
    Chat,
    Responses,
    Anthropic,
}

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
    async fn stream(&self, req: &ChatRequest) -> Result<EventStream, ProviderError> {
        let (protocol, settings) = self.prepare(req).await.map_err(ProviderError::from)?;
        match protocol {
            Protocol::Chat => openai_compat::stream(settings, req).await,
            Protocol::Responses => responses::stream(settings, req).await,
            Protocol::Anthropic => anthropic::stream(settings, req).await,
        }
    }

    /// Protocol `prepare` will select for this request, without building
    /// settings. Context assembly needs it before the request exists.
    fn protocol_of(&self, req: &ChatRequest) -> Result<Protocol, OperationError> {
        let options = resolve_options(&self.cfg, req).map_err(OperationError::invalid)?;
        let (token, _) = configured_credential(&self.provider_id, Some(&self.cfg), &options);
        let codex = token.is_empty()
            && self.auth.is_oauth(&self.provider_id)
            && ["openai", "openai-codex"].contains(&self.provider_id.as_str());
        self.select_protocol(req, &options, codex)
            .map_err(OperationError::invalid)
    }
}

impl Adapter {
    fn select_protocol(
        &self,
        req: &ChatRequest,
        options: &Value,
        codex: bool,
    ) -> Result<Protocol, String> {
        let model = self.cfg.models.get(&req.model);
        model_protocol(
            &self.provider_id,
            &self.cfg.npm,
            options,
            model,
            &req.model,
            codex,
        )
    }

    async fn prepare(&self, req: &ChatRequest) -> Result<(Protocol, Settings), OperationError> {
        let model = self.cfg.models.get(&req.model);
        let options = resolve_options(&self.cfg, req).map_err(OperationError::invalid)?;
        let (mut token, mut bearer) =
            configured_credential(&self.provider_id, Some(&self.cfg), &options);
        let credential = if token.is_empty() {
            self.auth.credential(&self.provider_id).await?
        } else {
            None
        };
        let oauth = matches!(credential, Some(AuthEntry::Oauth { .. }));
        let codex = oauth && ["openai", "openai-codex"].contains(&self.provider_id.as_str());
        let mut headers = BTreeMap::new();
        for (key, value) in &self.cfg.headers {
            headers.insert(key.to_lowercase(), value.clone());
        }
        if let Some(map) = options["headers"].as_object() {
            for (key, value) in map {
                if let Some(value) = value.as_str() {
                    headers.insert(key.to_lowercase(), value.to_string());
                } else {
                    return Err(OperationError::invalid(format!(
                        "provider header {key} must be a string"
                    )));
                }
            }
        }
        let mut base_url = options["baseURL"]
            .as_str()
            .unwrap_or_default()
            .trim_end_matches('/')
            .to_string();
        if base_url.is_empty() {
            base_url = self
                .cfg
                .extra
                .get("api")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .trim_end_matches('/')
                .to_string();
        }
        if base_url.is_empty() {
            let variable = match self.provider_id.as_str() {
                "openai" => "OPENAI_BASE_URL",
                "anthropic" => "ANTHROPIC_BASE_URL",
                "vllm" => "VLLM_BASE_URL",
                _ => "",
            };
            if !variable.is_empty() {
                base_url = std::env::var(variable)
                    .unwrap_or_default()
                    .trim_end_matches('/')
                    .to_string();
            }
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
            Some(AuthEntry::WellKnown { .. }) => return Err(OperationError::invalid(format!("provider {} uses well-known discovery credentials; configure an explicit provider baseURL and API key instead of treating discovery data as an API token", self.provider_id))),
            None => {}
        }
        let protocol = self
            .select_protocol(req, &options, codex)
            .map_err(OperationError::invalid)?;
        options::validate_protocol_options(&req.options, protocol, codex)
            .map_err(OperationError::invalid)?;
        if base_url.is_empty() {
            base_url = default_base_url(&self.provider_id).ok_or_else(|| format!("provider {} has no supported default endpoint; configure provider.{}.options.baseURL", self.provider_id, self.provider_id))?.into();
        }
        if !self.configured && default_base_url(&self.provider_id).is_none() {
            return Err(OperationError::invalid(format!(
                "unknown provider {}; define it in OpenCode configuration",
                self.provider_id
            )));
        }
        let parsed =
            reqwest::Url::parse(&base_url).map_err(|e| format!("invalid provider baseURL: {e}"))?;
        if !["http", "https"].contains(&parsed.scheme()) {
            return Err(OperationError::invalid(
                "provider baseURL must use http or https",
            ));
        }
        if token.is_empty()
            && !headers.contains_key("authorization")
            && !headers.contains_key("x-api-key")
            && parsed
                .host_str()
                .map(|host| {
                    [
                        "api.openai.com",
                        "api.anthropic.com",
                        "api.githubcopilot.com",
                        "openrouter.ai",
                    ]
                    .contains(&host)
                })
                .unwrap_or(false)
        {
            return Err(OperationError::invalid(format!("missing credentials for {}; set its API key environment variable, options.apiKey, or connect it with OpenCode", self.provider_id)));
        }
        if self.provider_id == "github-copilot" {
            headers
                .entry("openai-intent".into())
                .or_insert_with(|| "conversation-edits".into());
            headers
                .entry("x-github-api-version".into())
                .or_insert_with(|| "2026-08-01".into());
            headers
                .entry("x-interaction-type".into())
                .or_insert_with(|| "conversation-agent".into());
            headers.entry("x-initiator".into()).or_insert_with(|| {
                if req
                    .messages
                    .last()
                    .map(|m| m["role"] == "user")
                    .unwrap_or(false)
                {
                    "user".into()
                } else {
                    "agent".into()
                }
            });
            if let Some(session) = &req.session_id {
                headers
                    .entry("x-interaction-id".into())
                    .or_insert_with(|| session.clone());
            }
            if protocol == Protocol::Anthropic {
                headers
                    .entry("anthropic-beta".into())
                    .or_insert_with(|| "interleaved-thinking-2025-05-14".into());
            }
            if req.messages.iter().any(|m| {
                m["content"]
                    .as_array()
                    .map(|parts| {
                        parts.iter().any(|p| {
                            ["image_url", "image", "input_image"]
                                .contains(&p["type"].as_str().unwrap_or(""))
                        })
                    })
                    .unwrap_or(false)
            }) {
                headers.insert("copilot-vision-request".into(), "true".into());
            }
        }
        if protocol == Protocol::Anthropic {
            let path = parsed.path().trim_end_matches('/');
            if path.is_empty() {
                base_url.push_str("/v1");
            }
        }
        let api_model = model
            .map(|m| {
                m.extra
                    .get("api")
                    .and_then(|v| v.get("id"))
                    .and_then(Value::as_str)
                    .filter(|v| !v.is_empty())
                    .unwrap_or(if m.id.is_empty() { &req.model } else { &m.id })
                    .to_string()
            })
            .unwrap_or_else(|| req.model.clone());
        let max_output = model
            .and_then(|m| m.limit.as_ref())
            .and_then(|v| v["output"].as_u64());
        options::validate_limits(&req.options, &options, protocol, max_output)
            .map_err(OperationError::invalid)?;
        for (name, value) in options::safe_headers(&req.headers).map_err(OperationError::invalid)? {
            headers.insert(name, value);
        }
        let header_timeout = if options["headerTimeout"] == false {
            None
        } else {
            Some(Duration::from_millis(
                options["headerTimeout"]
                    .as_u64()
                    .filter(|n| *n > 0)
                    .unwrap_or(60_000),
            ))
        };
        let total_timeout = options["timeout"]
            .as_u64()
            .filter(|n| *n > 0)
            .map(Duration::from_millis);
        let chunk_timeout = Duration::from_millis(
            options["chunkTimeout"]
                .as_u64()
                .filter(|n| *n > 0)
                .unwrap_or(120_000),
        );
        let tools_supported = model
            .and_then(|m| {
                m.extra
                    .get("tool_call")
                    .and_then(Value::as_bool)
                    .or_else(|| {
                        m.extra
                            .get("capabilities")
                            .and_then(|v| v.get("tools"))
                            .and_then(Value::as_bool)
                    })
            })
            .unwrap_or(true);
        Ok((
            protocol,
            Settings {
                provider_id: self.provider_id.clone(),
                base_url,
                api_model,
                token,
                headers,
                bearer,
                codex,
                options,
                max_output,
                tools_supported,
                header_timeout,
                total_timeout,
                chunk_timeout,
            },
        ))
    }
}

fn resolve_options(cfg: &ProviderCfg, req: &ChatRequest) -> Result<Value, String> {
    let model = cfg.models.get(&req.model);
    let mut options = serde_json::to_value(&cfg.options).map_err(|e| e.to_string())?;
    if let Some(api) = cfg.extra.get("api").and_then(Value::as_str) {
        options["baseURL"] = json!(api);
    }
    let mut options = merge_model_options(options, model)?;
    if let Some(variant) = &req.variant {
        match model.and_then(|model| model.extra.get("variants")?.get(variant)) {
            Some(value) => {
                if !value.is_object() {
                    return Err(format!(
                        "variant {variant} for {} must be an object",
                        req.model
                    ));
                }
                if value["disabled"] == true {
                    return Err(format!(
                        "variant {variant} is disabled for model {}",
                        req.model
                    ));
                }
                deep_merge(&mut options, value.clone());
            }
            None if ![
                "none", "minimal", "low", "medium", "high", "xhigh", "max", "ultra",
            ]
            .contains(&variant.as_str()) =>
            {
                return Err(format!("unknown model variant {variant} for {}", req.model));
            }
            None => {}
        }
    }
    options::validate_options(&req.options)?;
    if let Some(request_options) = req.options.as_object() {
        if request_options.contains_key("maxOutputTokens") {
            options.as_object_mut().unwrap().remove("maxTokens");
        }
        if request_options.contains_key("maxTokens") {
            options.as_object_mut().unwrap().remove("maxOutputTokens");
        }
        for (key, value) in request_options {
            options[key] = value.clone();
        }
    }
    for key in ["headers", "body", "extraBody"] {
        if options
            .get(key)
            .is_some_and(|value| !value.is_object() && !value.is_null())
        {
            return Err(format!("provider option {key} must be an object"));
        }
    }
    if let Some(transport) = options["transport"].as_str() {
        if !["http", "sse", "auto"].contains(&transport) {
            return Err(format!("provider transport {transport} is not supported; configure transport as http or sse"));
        }
    }
    Ok(options)
}

/// Inventory and execution resolve the same model-level credentials and endpoints.
pub fn is_configured(id: &str, provider: Option<&ProviderCfg>) -> bool {
    let empty = ProviderCfg::default();
    let provider = provider.unwrap_or(&empty);
    std::iter::once("")
        .chain(provider.models.keys().map(String::as_str))
        .any(|model| {
            let request = ChatRequest {
                model: model.into(),
                ..Default::default()
            };
            resolve_options(provider, &request).is_ok_and(|options| {
                let (token, _) = configured_credential(id, Some(provider), &options);
                !token.is_empty()
                    || options["baseURL"]
                        .as_str()
                        .is_some_and(|url| !url.is_empty())
                    || provider
                        .headers
                        .iter()
                        .map(|(k, v)| (k.as_str(), v.as_str()))
                        .chain(
                            options["headers"]
                                .as_object()
                                .into_iter()
                                .flat_map(|headers| {
                                    headers
                                        .iter()
                                        .filter_map(|(k, v)| v.as_str().map(|v| (k.as_str(), v)))
                                }),
                        )
                        .any(|(name, value)| {
                            !value.is_empty()
                                && ["authorization", "x-api-key"]
                                    .contains(&name.to_ascii_lowercase().as_str())
                        })
                    || ["ollama", "vllm", "lmstudio"].contains(&id)
            })
        })
}

fn merge_model_options(
    mut options: Value,
    model: Option<&crate::config::ModelInfo>,
) -> Result<Value, String> {
    if let Some(model) = model {
        for key in ["options", "settings", "request"] {
            if let Some(value) = model.extra.get(key) {
                if !value.is_object() {
                    return Err(format!("model {} {key} must be an object", model.id));
                }
                deep_merge(&mut options, value.clone());
            }
        }
        if let Some(headers) = model.extra.get("headers") {
            deep_merge(&mut options["headers"], headers.clone());
        }
        if let Some(body) = model.extra.get("body") {
            deep_merge(&mut options["body"], body.clone());
        }
        if let Some(api) = model
            .extra
            .get("provider")
            .and_then(|v| v.get("api"))
            .and_then(Value::as_str)
        {
            options["baseURL"] = json!(api);
        }
        if model.extra.get("disabled").and_then(Value::as_bool) == Some(true) {
            return Err(format!("model {} is disabled", model.id));
        }
    }
    Ok(options)
}

fn model_protocol(
    provider: &str,
    npm_default: &str,
    options: &Value,
    model: Option<&crate::config::ModelInfo>,
    model_id: &str,
    codex: bool,
) -> Result<Protocol, String> {
    let npm = model
        .and_then(|m| m.extra.get("provider"))
        .and_then(|v| v.get("npm"))
        .and_then(Value::as_str)
        .unwrap_or(npm_default);
    let explicit = options["endpoint"]
        .as_str()
        .or_else(|| options["api"].as_str());
    protocol(provider, npm, explicit, model_id, codex)
}

fn protocol(
    provider: &str,
    npm: &str,
    endpoint: Option<&str>,
    model: &str,
    codex: bool,
) -> Result<Protocol, String> {
    if let Some(endpoint) = endpoint {
        return match endpoint {
            "responses" | "/responses" => Ok(Protocol::Responses),
            "chat" | "chat/completions" | "/chat/completions" => Ok(Protocol::Chat),
            "messages" | "/messages" | "anthropic" => Ok(Protocol::Anthropic),
            other => Err(format!("unsupported provider endpoint {other}; supported endpoints are chat, responses, and messages")),
        };
    }
    if codex {
        return Ok(Protocol::Responses);
    }
    if npm.contains("anthropic") {
        return Ok(Protocol::Anthropic);
    }
    if npm.contains("openai-compatible-responses") {
        return Ok(Protocol::Responses);
    }
    if npm.contains("openai-compatible") {
        return Ok(Protocol::Chat);
    }
    if [
        "@openrouter/ai-sdk-provider",
        "@ai-sdk/openrouter",
        "@ai-sdk/groq",
        "@ai-sdk/xai",
        "@ai-sdk/deepseek",
        "@ai-sdk/mistral",
        "@ai-sdk/togetherai",
    ]
    .contains(&npm)
    {
        return Ok(Protocol::Chat);
    }
    if provider == "github-copilot" || npm.contains("github-copilot") {
        if model.starts_with("claude-") {
            return Ok(Protocol::Anthropic);
        }
        let major = model
            .strip_prefix("gpt-")
            .and_then(|v| v.split(['.', '-']).next())
            .and_then(|v| v.parse::<u32>().ok());
        return Ok(
            if major.map(|v| v >= 5).unwrap_or(false) && !model.starts_with("gpt-5-mini")
                || model.starts_with("grok-")
                || model.starts_with("mai-code-")
            {
                Protocol::Responses
            } else {
                Protocol::Chat
            },
        );
    }
    if !npm.is_empty() && !["@ai-sdk/openai", "@opencode/ai/providers/openai"].contains(&npm) {
        return Err(format!("provider SDK {npm} requires a protocol not implemented by ocmini. Configure an OpenAI-compatible, Responses, or Anthropic endpoint explicitly."));
    }
    Ok(if provider == "anthropic" {
        Protocol::Anthropic
    } else if provider == "openai" || provider == "openai-codex" || !npm.is_empty() {
        Protocol::Responses
    } else {
        Protocol::Chat
    })
}

fn default_base_url(provider: &str) -> Option<&'static str> {
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
    pub(super) fn request(
        &self,
        method: reqwest::Method,
        url: &str,
        protocol: Protocol,
    ) -> reqwest::RequestBuilder {
        let mut request = client()
            .request(method, url)
            .header("accept", "text/event-stream, application/json")
            .header("user-agent", "ocmini");
        if !self.token.is_empty() {
            request = if protocol == Protocol::Anthropic && !self.bearer {
                request.header("x-api-key", &self.token)
            } else {
                request.bearer_auth(&self.token)
            };
        }
        if protocol == Protocol::Anthropic {
            request = request.header("anthropic-version", "2023-06-01");
        }
        for (name, value) in &self.headers {
            request = request.header(name.as_str(), value.as_str());
        }
        if let Some(timeout) = self.total_timeout {
            request = request.timeout(timeout);
        }
        request
    }

    pub async fn send(
        &self,
        endpoint: &str,
        protocol: Protocol,
        body: &Value,
    ) -> Result<reqwest::Response, ProviderError> {
        let suffix = format!("/{endpoint}");
        let url = if self.base_url.ends_with(&suffix) {
            self.base_url.clone()
        } else {
            format!("{}{suffix}", self.base_url.trim_end_matches('/'))
        };
        self.send_request(
            self.request(reqwest::Method::POST, &url, protocol)
                .json(body),
        )
        .await
    }

    pub(super) async fn send_request(
        &self,
        request: reqwest::RequestBuilder,
    ) -> Result<reqwest::Response, ProviderError> {
        let request = request
            .build()
            .map_err(|error| ProviderError::fatal(error.without_url().to_string()))?;
        let send = client().execute(request);
        let response = match self.header_timeout {
            Some(timeout) => {
                tokio::time::timeout(timeout, send)
                    .await
                    .map_err(|_| ProviderError {
                        retryable: true,
                        ..ProviderError::fatal(format!(
                            "{} did not return response headers within {} seconds",
                            self.provider_id,
                            timeout.as_secs()
                        ))
                    })?
            }
            None => send.await,
        }
        .map_err(|error| ProviderError {
            retryable: true,
            ..ProviderError::fatal(format!(
                "{} request failed: {}",
                self.provider_id,
                error.without_url()
            ))
        })?;
        if !response.status().is_success() {
            let status = response.status();
            let retry_after = response
                .headers()
                .get("retry-after")
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.trim().parse::<u64>().ok())
                .map(|seconds| Duration::from_secs(seconds.min(120)));
            let value = crate::http::read_body(response, Some(self.chunk_timeout), 64 * 1024)
                .await
                .unwrap_or_default();
            let message = serde_json::from_slice::<Value>(&value)
                .ok()
                .map(|v| error_message(&v))
                .unwrap_or_else(|| String::from_utf8_lossy(&value).chars().take(2000).collect());
            let message = if self.token.is_empty() {
                message
            } else {
                message.replace(&self.token, "[redacted]")
            };
            let retryable = status_retryable(status);
            let overflow = matches!(status.as_u16(), 400 | 413 | 422) && context_overflow(&message);
            return Err(ProviderError {
                kind: FailureKind::Error,
                message: format!("{} HTTP {status}: {message}", self.provider_id),
                retryable,
                retry_after: if retryable { retry_after } else { None },
                overflow,
            });
        }
        Ok(response)
    }

    pub fn overlay_body(&self, body: &mut Value) {
        for key in ["body", "extraBody"] {
            if let Some(overlay) = self.options.get(key).filter(|v| v.is_object()) {
                deep_merge(body, overlay.clone());
            }
        }
    }
}

pub(super) fn append_text(value: &mut Value, delta: &str) {
    match value {
        Value::String(text) => text.push_str(delta),
        _ => *value = Value::String(delta.into()),
    }
}

pub(super) fn error_message(value: &Value) -> String {
    value["error"]["message"]
        .as_str()
        .or_else(|| value["message"].as_str())
        .or_else(|| value["error"].as_str())
        .unwrap_or("provider returned an error")
        .chars()
        .take(2000)
        .collect()
}

pub(super) fn reasoning_effort<'a>(
    settings: &'a Settings,
    req: &'a ChatRequest,
) -> Option<&'a str> {
    settings.options["reasoningEffort"].as_str().or_else(|| {
        req.variant.as_deref().filter(|variant| {
            [
                "none", "minimal", "low", "medium", "high", "xhigh", "max", "ultra",
            ]
            .contains(variant)
        })
    })
}

pub(super) fn json_response(response: &reqwest::Response) -> bool {
    response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.contains("application/json") && !v.contains("event-stream"))
        .unwrap_or(false)
}

pub(super) fn usage_openai(value: &Value, responses: bool) -> Tokens {
    let (input_key, output_key, input_details, output_details) = if responses {
        (
            "input_tokens",
            "output_tokens",
            "input_tokens_details",
            "output_tokens_details",
        )
    } else {
        (
            "prompt_tokens",
            "completion_tokens",
            "prompt_tokens_details",
            "completion_tokens_details",
        )
    };
    let count = |value: &Value| value.as_i64().unwrap_or(0).max(0);
    let input = count(&value[input_key]);
    let output = count(&value[output_key]);
    let cache = count(&value[input_details]["cached_tokens"]).min(input);
    let reasoning = count(&value[output_details]["reasoning_tokens"]).min(output);
    Tokens {
        input: input - cache,
        output: output - reasoning,
        reasoning,
        cache: crate::types::Cache {
            read: cache,
            write: 0,
        },
    }
}

#[cfg(test)]
pub(super) fn test_settings(options: Value) -> Settings {
    Settings {
        provider_id: "test".into(),
        base_url: "http://localhost/v1".into(),
        api_model: "wire-model".into(),
        token: "test".into(),
        headers: BTreeMap::new(),
        bearer: true,
        codex: false,
        options,
        max_output: Some(32_000),
        tools_supported: true,
        header_timeout: None,
        total_timeout: None,
        chunk_timeout: Duration::from_secs(1),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn request_options_and_safe_headers_override_presets_within_model_limits() {
        let cfg: ProviderCfg = serde_json::from_value(json!({"options":{"baseURL":"http://localhost/v1","apiKey":"configured-secret","maxTokens":500},"headers":{"x-mode":"preset"},"models":{"m":{"limit":{"output":1000}}}})).unwrap();
        let adapter = Adapter {
            provider_id: "custom".into(),
            cfg,
            configured: true,
            auth: AuthStore::from_json("{}").unwrap(),
        };
        let mut req = ChatRequest {
            model: "m".into(),
            options: json!({"maxOutputTokens":256,"temperature":0.25}),
            headers: BTreeMap::from([("X-Mode".into(), "request".into())]),
            ..Default::default()
        };
        let (_, settings) = adapter.prepare(&req).await.unwrap();
        assert_eq!(settings.token, "configured-secret");
        assert_eq!(settings.headers["x-mode"], "request");
        assert_eq!(settings.options["maxOutputTokens"], 256);
        assert!(settings.options.get("maxTokens").is_none());
        req.options = json!({"maxOutputTokens":1001});
        assert!(adapter
            .prepare(&req)
            .await
            .err()
            .unwrap()
            .contains("model limit"));
        req.options = json!({"model":"replacement"});
        assert!(adapter.prepare(&req).await.is_err());
        req.options = Value::Null;
        req.headers = BTreeMap::from([("Authorization".into(), "replacement-secret".into())]);
        let error = adapter.prepare(&req).await.err().unwrap();
        assert!(error.contains("protected header"));
        assert!(!error.contains("replacement-secret"));
        req.headers.clear();
        req.model = "unconfigured-model".into();
        req.variant = Some("made-up-variant".into());
        assert!(adapter
            .prepare(&req)
            .await
            .err()
            .unwrap()
            .contains("unknown model variant"));
    }
    #[tokio::test]
    async fn custom_variants_apply_options_without_sending_the_variant_name_as_effort() {
        let cfg: ProviderCfg = serde_json::from_value(json!({"options":{"baseURL":"http://localhost/v1","apiKey":"test"},"models":{"model":{"variants":{"focused":{"reasoningEffort":"high","headers":{"x-mode":"focused"}}}}}})).unwrap();
        let adapter = Adapter {
            provider_id: "custom".into(),
            cfg,
            configured: true,
            auth: AuthStore::from_json("{}").unwrap(),
        };
        let mut req = ChatRequest {
            model: "model".into(),
            system: String::new(),
            messages: vec![],
            tools: vec![],
            variant: Some("focused".into()),
            session_id: None,
            ..Default::default()
        };
        let (_, settings) = adapter.prepare(&req).await.unwrap();
        assert_eq!(reasoning_effort(&settings, &req), Some("high"));
        assert_eq!(
            settings.headers.get("x-mode").map(String::as_str),
            Some("focused")
        );
        req.variant = Some("missing".into());
        assert!(adapter.prepare(&req).await.is_err());
    }
    #[test]
    fn selects_protocol_and_rejects_unimplemented_provider_sdks() {
        assert_eq!(
            protocol("custom", "@ai-sdk/anthropic", None, "m", false).unwrap(),
            Protocol::Anthropic
        );
        assert_eq!(
            protocol("openai", "", None, "m", true).unwrap(),
            Protocol::Responses
        );
        assert_eq!(
            protocol("github-copilot", "", None, "claude-sonnet-4-6", false).unwrap(),
            Protocol::Anthropic
        );
        assert_eq!(
            protocol("github-copilot", "", None, "gpt-5.5", false).unwrap(),
            Protocol::Responses
        );
        assert!(protocol("google", "@ai-sdk/google", None, "m", false).is_err());
    }
    #[test]
    fn usage_buckets_are_disjoint() {
        let usage = usage_openai(
            &json!({"prompt_tokens":100,"completion_tokens":50,"prompt_tokens_details":{"cached_tokens":40},"completion_tokens_details":{"reasoning_tokens":20}}),
            false,
        );
        assert_eq!(
            (usage.input, usage.output, usage.reasoning, usage.cache.read),
            (60, 30, 20, 40)
        );
    }
    #[test]
    fn overflow_detection_and_limit_parsing() {
        assert!(context_overflow("pllm5 HTTP 400: This model\'s maximum context length is 262144 tokens. However, you requested 0 output tokens and your prompt contains at least 262145 input tokens"));
        assert!(context_overflow(
            "prompt is too long: 213000 tokens > 200000 maximum"
        ));
        assert!(!context_overflow(
            "provider stream failed: error decoding response body"
        ));
        assert_eq!(
            parse_context_limit(
                "maximum context length is 262144 tokens. your prompt contains 262145"
            ),
            Some(262144)
        );
        assert_eq!(
            parse_context_limit("prompt is too long: 213000 tokens > 200000 maximum"),
            Some(200000)
        );
        assert_eq!(parse_context_limit("no numbers here"), None);
    }
}
