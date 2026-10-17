use crate::config::{data_home, Config, ProviderCfg};
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum AuthEntry {
    #[serde(rename = "api")]
    Api { key: String },
    #[serde(rename = "oauth")]
    Oauth {
        #[serde(default)]
        access: String,
        #[serde(default)]
        refresh: String,
        #[serde(default)]
        expires: i64,
        #[serde(default, rename = "enterpriseUrl")]
        enterprise_url: Option<String>,
        #[serde(default, rename = "accountId", alias = "accountID")]
        account_id: Option<String>,
        #[serde(default)]
        metadata: Value,
    },
    #[serde(rename = "wellknown")]
    WellKnown { key: String, token: String },
}

impl AuthEntry {
    pub fn usable(&self) -> bool {
        match self {
            Self::Api { key } => !key.is_empty(),
            Self::Oauth {
                access, refresh, ..
            } => !access.is_empty() || !refresh.is_empty(),
            Self::WellKnown { .. } => false,
        }
    }

    fn source_token(&self) -> &str {
        match self {
            Self::Api { key } => key,
            Self::Oauth {
                refresh, access, ..
            } => {
                if refresh.is_empty() {
                    access
                } else {
                    refresh
                }
            }
            Self::WellKnown { token, .. } => token,
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct CachedAuth {
    source: String,
    credential: AuthEntry,
}

#[derive(Clone)]
pub struct AuthStore {
    entries: Arc<HashMap<String, AuthEntry>>,
    refreshed: Arc<Mutex<HashMap<String, CachedAuth>>>,
    cache_path: Option<PathBuf>,
    error: Option<String>,
}

impl AuthStore {
    pub fn load() -> Self {
        let path = PathBuf::from(data_home()).join("opencode/auth.json");
        let raw = match std::env::var("OPENCODE_AUTH_CONTENT") {
            Ok(value) => Ok(value),
            Err(std::env::VarError::NotPresent) => match std::fs::read_to_string(&path) {
                Ok(value) => Ok(value),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok("{}".into()),
                Err(error) => Err(format!(
                    "cannot read OpenCode credentials {}: {error}",
                    path.display()
                )),
            },
            Err(error) => Err(format!("cannot read OPENCODE_AUTH_CONTENT: {error}")),
        };
        let mut store = raw
            .and_then(|raw| Self::from_json(&raw))
            .unwrap_or_else(|error| Self {
                entries: Arc::new(HashMap::new()),
                refreshed: Arc::new(Mutex::new(HashMap::new())),
                cache_path: None,
                error: Some(error),
            });
        let cache_path = PathBuf::from(data_home()).join("ocmini/auth.json");
        if let Ok(text) = std::fs::read_to_string(&cache_path) {
            if let Ok(cache) = serde_json::from_str::<HashMap<String, CachedAuth>>(&text) {
                let cache = cache
                    .into_iter()
                    .filter(|(id, cached)| {
                        store
                            .entries
                            .get(id)
                            .map(|entry| entry.source_token() == cached.source)
                            .unwrap_or(false)
                    })
                    .collect();
                store.refreshed = Arc::new(Mutex::new(cache));
            }
        }
        store.cache_path = Some(cache_path);
        store
    }

    pub fn from_json(raw: &str) -> Result<Self, String> {
        let value: Value = serde_json::from_str(raw)
            .map_err(|e| format!("invalid OpenCode auth.json/OPENCODE_AUTH_CONTENT: {e}"))?;
        let object = value
            .as_object()
            .ok_or("OpenCode auth must be a JSON object")?;
        let mut entries = HashMap::new();
        for (id, value) in object {
            let entry: AuthEntry = serde_json::from_value(value.clone())
                .map_err(|e| format!("invalid credential for provider {id}: {e}"))?;
            entries.insert(id.trim_end_matches('/').to_string(), entry);
        }
        Ok(Self {
            entries: Arc::new(entries),
            refreshed: Arc::new(Mutex::new(HashMap::new())),
            cache_path: None,
            error: None,
        })
    }

    pub fn connected(&self) -> Vec<String> {
        let mut ids: Vec<String> = self
            .entries
            .iter()
            .filter(|(_, entry)| entry.usable())
            .map(|(id, _)| id.clone())
            .collect();
        ids.sort();
        ids
    }

    pub fn connected_for(&self, config: &Config) -> Vec<String> {
        let mut ids = self.connected();
        for id in [
            "openai",
            "anthropic",
            "openrouter",
            "groq",
            "xai",
            "deepseek",
            "mistral",
            "togetherai",
        ] {
            if !configured_token(id, config.provider(id)).is_empty() {
                ids.push(id.into());
            }
        }
        for (id, provider) in &config.provider {
            if !configured_token(id, Some(provider)).is_empty()
                || !provider.options.base_url.is_empty()
                || provider
                    .extra
                    .get("api")
                    .and_then(Value::as_str)
                    .is_some_and(|url| !url.is_empty())
                || provider
                    .headers
                    .iter()
                    .chain(provider.options.headers.iter())
                    .any(|(name, value)| {
                        !value.is_empty()
                            && ["authorization", "x-api-key"]
                                .contains(&name.to_lowercase().as_str())
                    })
                || ["ollama", "vllm", "lmstudio"].contains(&id.as_str())
            {
                ids.push(id.clone());
            }
        }
        ids.retain(|id| config.provider_enabled(id));
        ids.sort();
        ids.dedup();
        ids
    }

    pub async fn credential(&self, provider: &str) -> Result<Option<AuthEntry>, String> {
        if let Some(error) = &self.error {
            return Err(error.clone());
        }
        let mut refreshed = self.refreshed.lock().await;
        let entry = refreshed
            .get(provider)
            .map(|cached| cached.credential.clone())
            .or_else(|| self.entries.get(provider).cloned());
        let Some(AuthEntry::Oauth {
            access,
            refresh,
            expires,
            enterprise_url,
            account_id,
            metadata,
        }) = entry.clone()
        else {
            return Ok(entry);
        };
        if provider == "github-copilot" {
            return Ok(entry);
        }
        let expires = if expires > 0 && expires < 10_000_000_000 {
            expires.saturating_mul(1000)
        } else {
            expires
        };
        if !access.is_empty() && (expires <= 0 || expires > crate::types::now_ms() + 60_000) {
            return Ok(entry);
        }
        if !["openai", "openai-codex"].contains(&provider) {
            return Err(format!("OAuth credential for {provider} is expired or missing an access token. Reconnect this provider with OpenCode or configure an API key; this provider's OAuth refresh protocol is not supported."));
        }
        if refresh.is_empty() {
            return Err(format!("OAuth credential for {provider} has expired and has no refresh token. Reconnect this provider with OpenCode."));
        }
        let response = crate::provider::client()
            .post("https://auth.openai.com/oauth/token")
            .timeout(std::time::Duration::from_secs(30))
            .header("User-Agent", "ocmini")
            .form(&[
                ("grant_type", "refresh_token"),
                ("refresh_token", refresh.as_str()),
                ("client_id", "app_EMoamEEZ73f0CkXaXp7hrann"),
            ])
            .send()
            .await
            .map_err(|e| format!("OpenAI OAuth refresh failed: {e}"))?;
        let status = response.status();
        if !status.is_success() {
            return Err(format!("OpenAI OAuth refresh returned HTTP {status}. Reconnect OpenAI with OpenCode; the stored refresh token may have expired or been revoked."));
        }
        let value: Value = response
            .json()
            .await
            .map_err(|e| format!("invalid OpenAI token response: {e}"))?;
        let next_access = value["access_token"]
            .as_str()
            .filter(|value| !value.is_empty())
            .ok_or("OpenAI OAuth refresh response has no access_token")?
            .to_string();
        let next = AuthEntry::Oauth {
            account_id: value["id_token"]
                .as_str()
                .and_then(account_from_jwt)
                .or_else(|| account_from_jwt(&next_access))
                .or(account_id),
            access: next_access,
            refresh: value["refresh_token"]
                .as_str()
                .unwrap_or(&refresh)
                .to_string(),
            expires: crate::types::now_ms()
                + value["expires_in"]
                    .as_i64()
                    .unwrap_or(3600)
                    .max(1)
                    .saturating_mul(1000),
            enterprise_url,
            metadata,
        };
        let source = self
            .entries
            .get(provider)
            .map(AuthEntry::source_token)
            .unwrap_or(&refresh)
            .to_string();
        refreshed.insert(
            provider.to_string(),
            CachedAuth {
                source,
                credential: next.clone(),
            },
        );
        if let Some(path) = &self.cache_path {
            save_cache(path, &refreshed)
                .map_err(|e| format!("OAuth refreshed but cannot save the rotated token: {e}"))?;
        }
        Ok(Some(next))
    }
}

pub fn configured_token(provider: &str, cfg: Option<&ProviderCfg>) -> String {
    if let Some(cfg) = cfg {
        if !cfg.options.api_key.is_empty() {
            return cfg.options.api_key.clone();
        }
        for key in ["authToken", "accessToken"] {
            if let Some(token) = cfg
                .options
                .extra
                .get(key)
                .and_then(Value::as_str)
                .filter(|token| !token.is_empty())
            {
                return token.to_string();
            }
        }
        for key in &cfg.env {
            if let Ok(token) = std::env::var(key) {
                if !token.is_empty() {
                    return token;
                }
            }
        }
    }
    let names: &[&str] = match provider {
        "openai" | "openai-codex" => &["OPENAI_API_KEY"],
        "anthropic" => &["ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN"],
        "openrouter" => &["OPENROUTER_API_KEY"],
        "groq" => &["GROQ_API_KEY"],
        "xai" => &["XAI_API_KEY"],
        "deepseek" => &["DEEPSEEK_API_KEY"],
        "mistral" => &["MISTRAL_API_KEY"],
        "togetherai" => &["TOGETHER_API_KEY"],
        "vllm" => &["VLLM_API_KEY"],
        "ollama" => &["OLLAMA_API_KEY"],
        _ => &[],
    };
    names
        .iter()
        .find_map(|name| std::env::var(name).ok().filter(|token| !token.is_empty()))
        .unwrap_or_default()
}

pub fn account_from_jwt(token: &str) -> Option<String> {
    let payload = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    let value: Value = serde_json::from_slice(&bytes).ok()?;
    value["https://api.openai.com/auth"]["chatgpt_account_id"]
        .as_str()
        .or_else(|| value["chatgpt_account_id"].as_str())
        .or_else(|| value["organizations"][0]["id"].as_str())
        .map(String::from)
}

fn save_cache(path: &std::path::Path, cache: &HashMap<String, CachedAuth>) -> Result<(), String> {
    use std::io::Write;
    let parent = path.parent().ok_or("invalid auth cache path")?;
    std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    // NamedTempFile defaults to mode 0600 on Unix and removes failed writes.
    let mut file = tempfile::NamedTempFile::new_in(parent).map_err(|e| e.to_string())?;
    file.write_all(&serde_json::to_vec(cache).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    file.as_file().sync_all().map_err(|e| e.to_string())?;
    file.persist(path).map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn cache_replacement_is_private_and_leaves_no_temporary_files() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("auth.json");
        std::fs::write(&path, "previous").unwrap();
        save_cache(&path, &HashMap::new()).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{}");
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(path.metadata().unwrap().permissions().mode() & 0o777, 0o600);
        }
    }
    #[test]
    fn auth_parses_metadata_and_never_treats_wellknown_as_api_key() {
        let store = AuthStore::from_json(&json!({"openai":{"type":"oauth","access":"token","refresh":"refresh","expires":0,"accountId":"account"},"x":{"type":"wellknown","key":"ENV_NAME","token":"url"},"empty":{"type":"api","key":""}}).to_string()).unwrap();
        assert_eq!(store.connected(), ["openai"]);
        assert!(
            matches!(store.entries.get("openai"), Some(AuthEntry::Oauth { account_id: Some(id), .. }) if id == "account")
        );
    }
    #[test]
    fn jwt_account_is_read_without_authenticating_the_token() {
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(r#"{"https://api.openai.com/auth":{"chatgpt_account_id":"acct"}}"#);
        assert_eq!(
            account_from_jwt(&format!("header.{payload}.signature")),
            Some("acct".into())
        );
    }
}
