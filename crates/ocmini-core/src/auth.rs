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

    fn fresh(&self) -> bool {
        match self {
            Self::Oauth {
                access, expires, ..
            } => {
                let expires = if *expires > 0 && *expires < 10_000_000_000 {
                    expires.saturating_mul(1000)
                } else {
                    *expires
                };
                !access.is_empty() && (expires <= 0 || expires > crate::types::now_ms() + 60_000)
            }
            _ => self.usable(),
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
    refresh_locks: Arc<std::sync::Mutex<HashMap<String, Arc<Mutex<()>>>>>,
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
                refresh_locks: Arc::new(Default::default()),
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
            refresh_locks: Arc::new(Default::default()),
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
        for id in config.provider.keys().map(String::as_str).chain([
            "openai",
            "anthropic",
            "openrouter",
            "groq",
            "xai",
            "deepseek",
            "mistral",
            "togetherai",
        ]) {
            if crate::provider::is_configured(id, config.provider(id)) {
                ids.push(id.into());
            }
        }
        ids.retain(|id| config.provider_enabled(id));
        ids.sort();
        ids.dedup();
        ids
    }

    pub fn is_oauth(&self, provider: &str) -> bool {
        matches!(self.entries.get(provider), Some(AuthEntry::Oauth { .. }))
    }

    pub async fn credential(&self, provider: &str) -> Result<Option<AuthEntry>, String> {
        self.credential_at(provider, "https://auth.openai.com/oauth/token")
            .await
    }

    async fn credential_at(
        &self,
        provider: &str,
        endpoint: &str,
    ) -> Result<Option<AuthEntry>, String> {
        if let Some(error) = &self.error {
            return Err(error.clone());
        }
        let lock = self
            .refresh_locks
            .lock()
            .unwrap()
            .entry(provider.into())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone();
        let _refresh = lock.lock().await;
        let entry = self
            .refreshed
            .lock()
            .await
            .get(provider)
            .map(|cached| cached.credential.clone())
            .or_else(|| self.entries.get(provider).cloned());
        if provider == "github-copilot" || entry.as_ref().is_some_and(AuthEntry::fresh) {
            return Ok(entry);
        }
        let Some(AuthEntry::Oauth {
            refresh,
            enterprise_url,
            account_id,
            metadata,
            ..
        }) = entry.clone()
        else {
            return Ok(entry);
        };
        if !["openai", "openai-codex"].contains(&provider) {
            return Err(format!("OAuth credential for {provider} is expired or missing an access token. Reconnect this provider with OpenCode or configure an API key; this provider's OAuth refresh protocol is not supported."));
        }
        if refresh.is_empty() {
            return Err(format!("OAuth credential for {provider} has expired and has no refresh token. Reconnect this provider with OpenCode."));
        }
        // Serialize only this rotating credential across processes, then reload it.
        let source = self
            .entries
            .get(provider)
            .map(AuthEntry::source_token)
            .unwrap_or(&refresh)
            .to_owned();
        let _claim = if let Some(path) = &self.cache_path {
            let lock = path.with_file_name(format!(
                "auth-{:016x}.lock",
                crate::types::stable_hash(provider)
            ));
            let claim = cache_lock(&lock).await?;
            if let Some(cached) = crate::blocking::run(|| read_cache(path).remove(provider))
                .filter(|cached| cached.source == source && cached.credential.fresh())
            {
                let credential = cached.credential.clone();
                self.refreshed.lock().await.insert(provider.into(), cached);
                return Ok(Some(credential));
            }
            Some(claim)
        } else {
            None
        };
        let response = crate::provider::client()
            .post(endpoint)
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
        let cached = CachedAuth {
            source,
            credential: next.clone(),
        };
        self.refreshed
            .lock()
            .await
            .insert(provider.into(), cached.clone());
        if let Some(path) = &self.cache_path {
            let _write = cache_lock(&path.with_extension("write.lock")).await?;
            crate::blocking::run(|| save_cache(path, &HashMap::from([(provider.into(), cached)])))
                .map_err(|e| format!("OAuth refreshed but cannot save the rotated token: {e}"))?;
        }
        Ok(Some(next))
    }
}

pub fn configured_credential(
    provider: &str,
    cfg: Option<&ProviderCfg>,
    options: &Value,
) -> (String, bool) {
    for key in ["apiKey", "authToken", "accessToken"] {
        if let Some(token) = options[key].as_str().filter(|token| !token.is_empty()) {
            return (token.into(), key != "apiKey");
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
    for name in cfg
        .into_iter()
        .flat_map(|cfg| cfg.env.iter().map(String::as_str))
        .chain(names.iter().copied())
    {
        if let Ok(token) = std::env::var(name) {
            if !token.is_empty() {
                return (
                    token,
                    provider == "anthropic" && name == "ANTHROPIC_AUTH_TOKEN",
                );
            }
        }
    }
    (String::new(), false)
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

fn read_cache(path: &std::path::Path) -> HashMap<String, CachedAuth> {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

/// Advisory file ownership is scoped to the returned File and released on cancellation/drop.
async fn cache_lock(path: &std::path::Path) -> Result<std::fs::File, String> {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let result = crate::blocking::run(|| -> std::io::Result<std::fs::File> {
            std::fs::create_dir_all(
                path.parent()
                    .ok_or_else(|| std::io::Error::other("invalid auth lock path"))?,
            )?;
            let mut options = std::fs::OpenOptions::new();
            options.read(true).write(true).create(true).truncate(false);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            #[cfg(windows)]
            {
                use std::os::windows::fs::OpenOptionsExt;
                options.share_mode(0);
            }
            let file = options.open(path)?;
            #[cfg(unix)]
            {
                use std::os::fd::AsRawFd;
                if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            Ok(file)
        });
        match result {
            Ok(file) => return Ok(file),
            Err(error)
                if (error.kind() == std::io::ErrorKind::WouldBlock
                    || cfg!(windows) && matches!(error.raw_os_error(), Some(32 | 33)))
                    && tokio::time::Instant::now() < deadline =>
            {
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
            Err(error) => return Err(format!("Cannot acquire auth cache ownership: {error}")),
        }
    }
}

fn save_cache(path: &std::path::Path, cache: &HashMap<String, CachedAuth>) -> Result<(), String> {
    use std::io::Write;
    let parent = path.parent().ok_or("invalid auth cache path")?;
    std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    // NamedTempFile defaults to mode 0600 on Unix and removes failed writes.
    let mut file = tempfile::NamedTempFile::new_in(parent).map_err(|e| e.to_string())?;
    let mut merged = read_cache(path);
    merged.extend(
        cache
            .iter()
            .map(|(id, cached)| (id.clone(), cached.clone())),
    );
    file.write_all(&serde_json::to_vec(&merged).map_err(|e| e.to_string())?)
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
    #[tokio::test]
    async fn cache_claims_release_when_a_waiter_is_cancelled_and_writes_merge() {
        let root = tempfile::tempdir().unwrap();
        let lock = root.path().join("cache.lock");
        let claim = cache_lock(&lock).await.unwrap();
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(40), cache_lock(&lock))
                .await
                .is_err()
        );
        drop(claim);
        let _claim = cache_lock(&lock).await.unwrap();
        let path = root.path().join("auth.json");
        for id in ["one", "two"] {
            save_cache(
                &path,
                &HashMap::from([(
                    id.into(),
                    CachedAuth {
                        source: id.into(),
                        credential: AuthEntry::Api {
                            key: "fixture".into(),
                        },
                    },
                )]),
            )
            .unwrap();
        }
        assert_eq!(read_cache(&path).len(), 2);
    }
    #[tokio::test]
    async fn one_refresh_does_not_block_an_unrelated_api_credential() {
        let store = AuthStore::from_json(r#"{"openai":{"type":"oauth","access":"expired","refresh":"fixture","expires":1},"other":{"type":"api","key":"fixture"}}"#).unwrap();
        let lock = Arc::new(Mutex::new(()));
        store
            .refresh_locks
            .lock()
            .unwrap()
            .insert("openai".into(), lock.clone());
        let _refresh = lock.lock().await;
        let entry = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            store.credential("other"),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(matches!(entry, Some(AuthEntry::Api { .. })));
    }
    #[test]
    fn inventory_and_execution_see_model_credentials_and_the_same_precedence() {
        let config: Config = serde_json::from_value(
            json!({"provider":{"custom":{"models":{"m":{"options":{"apiKey":"fixture"}}}}}}),
        )
        .unwrap();
        let store = AuthStore::from_json("{}").unwrap();
        assert!(store.connected_for(&config).contains(&"custom".into()));
        assert_eq!(
            configured_credential(
                "custom",
                None,
                &json!({"apiKey":"api","authToken":"bearer"})
            ),
            ("api".into(), false)
        );
        assert_eq!(
            configured_credential("custom", None, &json!({"accessToken":"bearer"})),
            ("bearer".into(), true)
        );
    }

    #[tokio::test]
    async fn rotating_refresh_is_shared_by_waiters_and_independent_cache_readers() {
        use axum::{routing::post, Json, Router};
        use std::sync::atomic::{AtomicUsize, Ordering};
        let calls = Arc::new(AtomicUsize::new(0));
        let count = calls.clone();
        let router = Router::new().route("/token", post(move |body: String| {
            let count = count.clone();
            async move {
                count.fetch_add(1, Ordering::Relaxed);
                assert!(body.contains("refresh_token=old-refresh"));
                tokio::time::sleep(std::time::Duration::from_millis(30)).await;
                Json(json!({"access_token":"new-access","refresh_token":"rotated-refresh","expires_in":3600}))
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/token", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let root = tempfile::tempdir().unwrap();
        let cache = root.path().join("new-directory/auth.json");
        let raw = r#"{"openai":{"type":"oauth","access":"expired-access","refresh":"old-refresh","expires":1},"other":{"type":"api","key":"fixture"}}"#;
        let mut first = AuthStore::from_json(raw).unwrap();
        first.cache_path = Some(cache.clone());
        // A second store models a process that loaded auth before rotation.
        let mut second = AuthStore::from_json(raw).unwrap();
        second.cache_path = Some(cache.clone());
        let (a, b, c, api) = tokio::join!(
            first.credential_at("openai", &endpoint),
            first.credential_at("openai", &endpoint),
            second.credential_at("openai", &endpoint),
            first.credential("other"),
        );
        for credential in [a, b, c] {
            assert!(
                matches!(credential.unwrap(), Some(AuthEntry::Oauth { access, refresh, .. }) if access == "new-access" && refresh == "rotated-refresh")
            );
        }
        assert!(matches!(api.unwrap(), Some(AuthEntry::Api { key }) if key == "fixture"));
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        assert!(read_cache(&cache)["openai"].credential.fresh());
        server.abort();
    }
}
