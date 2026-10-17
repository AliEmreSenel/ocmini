pub mod anthropic;
pub mod openai_compat;

use crate::auth::AuthStore;
use crate::config::ProviderCfg;
use crate::types::ProviderEvent;
use async_trait::async_trait;

pub struct ChatRequest {
    pub model: String,
    pub system: String,
    pub messages: Vec<serde_json::Value>,
    pub tools: Vec<serde_json::Value>,
    pub variant: Option<String>,
}

#[async_trait]
pub trait ProviderAdapter: Send + Sync {
    async fn stream(
        &self,
        req: ChatRequest,
    ) -> Result<Box<dyn futures::Stream<Item = ProviderEvent> + Send + Unpin>, String>;
}

pub fn build(
    provider_id: &str,
    cfg: Option<&ProviderCfg>,
    auth: &AuthStore,
) -> Box<dyn ProviderAdapter> {
    let npm = cfg.map(|c| c.npm.as_str()).unwrap_or("");
    let anthropic = provider_id == "anthropic"
        || cfg.map(|c| c.options.base_url.contains("anthropic")).unwrap_or(false);
    if anthropic {
        return Box::new(anthropic::AnthropicAdapter::new(provider_id, cfg, auth));
    }
    let _ = npm;
    Box::new(openai_compat::OpenAiCompatAdapter::new(provider_id, cfg, auth))
}
