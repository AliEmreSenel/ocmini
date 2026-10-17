use super::{Adapter, ChatRequest, Protocol};
use crate::auth::AuthStore;
use crate::config::{Config, ModelInfo};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::hash::{Hash, Hasher};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

type Cache = HashMap<u64, (Instant, HashMap<String, ModelInfo>)>;
static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();

pub async fn discover_models(config: &Config, auth: &AuthStore) -> (Config, Vec<String>) {
    let mut result = config.clone();
    let providers = auth.connected_for(config);
    let tasks = providers
        .into_iter()
        .filter(|id| {
            config
                .provider(id)
                .map(|p| p.models.is_empty())
                .unwrap_or(true)
        })
        .map(|id| async move {
            let models = discover(&id, config, auth).await;
            (id, models)
        });
    let mut errors = Vec::new();
    for (id, models) in futures::future::join_all(tasks).await {
        match models {
            Ok(models) => {
                let provider = result.provider.entry(id.clone()).or_default();
                if provider.name.is_empty() { provider.name = id; }
                provider.models = models;
            }
            Err(error) => errors.push(format!("Model discovery for {id}: {error}. Configure provider.{id}.models to supply an explicit catalog.")),
        }
    }
    // A model explicitly selected in config is a declared capability, even if its provider cannot enumerate models.
    for selected in [&config.model, &config.small_model] {
        if let Some((provider, model)) = selected.split_once('/') {
            if !provider.is_empty() && !model.is_empty() && config.provider_enabled(provider) {
                result
                    .provider
                    .entry(provider.into())
                    .or_default()
                    .models
                    .entry(model.into())
                    .or_insert_with(|| ModelInfo {
                        id: model.into(),
                        name: model.into(),
                        ..Default::default()
                    });
            }
        }
    }
    (result, errors)
}

async fn discover(
    id: &str,
    config: &Config,
    auth: &AuthStore,
) -> Result<HashMap<String, ModelInfo>, String> {
    let adapter = Adapter {
        provider_id: id.into(),
        cfg: config.provider(id).cloned().unwrap_or_default(),
        configured: config.provider(id).is_some(),
        auth: auth.clone(),
    };
    let request = ChatRequest {
        model: String::new(),
        system: String::new(),
        messages: Vec::new(),
        tools: Vec::new(),
        variant: None,
        session_id: None,
    };
    let (protocol, settings) = adapter.prepare(&request).await?;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    id.hash(&mut hasher);
    settings.base_url.hash(&mut hasher);
    settings.token.hash(&mut hasher);
    settings.headers.hash(&mut hasher);
    let key = hasher.finish();
    if let Some((created, models)) = CACHE
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap()
        .get(&key)
    {
        if created.elapsed() < Duration::from_secs(300) {
            return Ok(models.clone());
        }
    }
    let mut base = settings.base_url.as_str();
    for suffix in ["/chat/completions", "/responses", "/messages"] {
        if let Some(stripped) = base.strip_suffix(suffix) {
            base = stripped;
            break;
        }
    }
    let client = super::client();
    let mut models = HashMap::new();
    let mut after: Option<String> = None;
    for _ in 0..10 {
        let mut request = client
            .get(format!("{}/models", base.trim_end_matches('/')))
            .header("User-Agent", "ocmini");
        if !settings.token.is_empty() {
            request = if protocol == Protocol::Anthropic && !settings.bearer {
                request.header("x-api-key", &settings.token)
            } else {
                request.bearer_auth(&settings.token)
            };
        }
        if protocol == Protocol::Anthropic {
            request = request.header("anthropic-version", "2023-06-01");
        }
        for (name, value) in &settings.headers {
            request = request.header(name.as_str(), value.as_str());
        }
        if let Some(after) = &after {
            request = request.query(&[("after_id", after.as_str()), ("limit", "100")]);
        }
        let response = request.send().await.map_err(|e| e.to_string())?;
        if !response.status().is_success() {
            return Err(format!("GET /models returned HTTP {}", response.status()));
        }
        let bytes = super::read_response(response, Duration::from_secs(6), 8 * 1024 * 1024).await?;
        let data: Value = serde_json::from_slice(&bytes)
            .map_err(|e| format!("invalid model catalog JSON: {e}"))?;
        models.extend(parse_models(id, &data)?);
        if data["has_more"] != true {
            break;
        }
        let next = data["last_id"]
            .as_str()
            .filter(|value| !value.is_empty())
            .ok_or("paginated model catalog has no last_id")?
            .to_string();
        if after.as_ref() == Some(&next) {
            return Err("model catalog returned a repeated pagination cursor".into());
        }
        after = Some(next);
    }
    if models.is_empty() {
        return Err("provider returned no supported language models".into());
    }
    let mut cache = CACHE
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap();
    cache.retain(|_, (created, _)| created.elapsed() < Duration::from_secs(300));
    if cache.len() >= 64 {
        cache.clear();
    }
    cache.insert(key, (Instant::now(), models.clone()));
    Ok(models)
}

fn parse_models(provider: &str, data: &Value) -> Result<HashMap<String, ModelInfo>, String> {
    let items = data["data"]
        .as_array()
        .or_else(|| data["models"].as_array())
        .ok_or("model catalog must contain a data or models array")?;
    let mut models = HashMap::new();
    for item in items {
        let Some(id) = item["id"]
            .as_str()
            .or_else(|| item["slug"].as_str())
            .or_else(|| item["model"].as_str())
            .filter(|id| !id.is_empty())
        else {
            continue;
        };
        if item["policy"]["state"] == "disabled"
            || item["model_picker_enabled"] == false
            || item["disabled"] == true
        {
            continue;
        }
        if provider == "openai" && item["object"] == "model" {
            let language = id.starts_with("gpt-")
                || id.starts_with("chatgpt-")
                || id.starts_with("o1")
                || id.starts_with("o3")
                || id.starts_with("o4");
            if !language
                || [
                    "realtime",
                    "audio",
                    "transcribe",
                    "image",
                    "tts",
                    "instruct",
                    "search",
                ]
                .iter()
                .any(|part| id.contains(part))
            {
                continue;
            }
        }
        let name = item["display_name"]
            .as_str()
            .or_else(|| item["name"].as_str())
            .unwrap_or(id);
        let limits = &item["capabilities"]["limits"];
        let mut limit = item["limit"].as_object().cloned().unwrap_or_default();
        for (key, candidates) in [
            (
                "context",
                vec![
                    &limits["max_context_window_tokens"],
                    &limits["max_prompt_tokens"],
                    &item["context_window"],
                    &item["context_length"],
                ],
            ),
            (
                "output",
                vec![&limits["max_output_tokens"], &item["max_output_tokens"]],
            ),
            ("input", vec![&limits["max_prompt_tokens"]]),
        ] {
            if !limit.contains_key(key) {
                if let Some(value) = candidates.iter().find_map(|v| v.as_u64()) {
                    limit.insert(key.into(), json!(value));
                }
            }
        }
        let supports = &item["capabilities"]["supports"];
        let mut extra = BTreeMap::new();
        for key in ["cost", "family", "status"] {
            if let Some(value) = item.get(key) {
                extra.insert(key.into(), value.clone());
            }
        }
        if let Some(tools) = supports["tool_calls"].as_bool() {
            extra.insert("tool_call".into(), json!(tools));
        }
        let endpoints = item["supported_endpoints"].as_array();
        if let Some(endpoints) = endpoints {
            let endpoint = if endpoints.iter().any(|e| e == "/v1/messages") {
                Some("messages")
            } else if endpoints.iter().any(|e| e == "/responses") {
                Some("responses")
            } else if endpoints.iter().any(|e| e == "/chat/completions") {
                Some("chat")
            } else {
                None
            };
            if let Some(endpoint) = endpoint {
                extra.insert("options".into(), json!({"endpoint":endpoint}));
            }
        }
        let efforts = supports["reasoning_effort"]
            .as_array()
            .or_else(|| item["supported_reasoning_levels"].as_array());
        if let Some(efforts) = efforts {
            let mut variants = serde_json::Map::new();
            for effort in efforts {
                if let Some(effort) = effort.as_str().or_else(|| effort["effort"].as_str()) {
                    let variant = if supports["adaptive_thinking"] == true {
                        json!({"thinking":{"type":"adaptive"},"output_config":{"effort":effort}})
                    } else {
                        json!({"reasoningEffort":effort})
                    };
                    variants.insert(effort.into(), variant);
                }
            }
            if !variants.is_empty() {
                extra.insert("variants".into(), Value::Object(variants));
                extra.insert("reasoning".into(), json!(true));
            }
        }
        let modalities = item.get("modalities").cloned().or_else(|| item["input_modalities"].as_array().map(|input| json!({"input":input,"output":["text"]}))).or_else(|| supports["vision"].as_bool().map(|vision| json!({"input":if vision { vec!["text","image"] } else { vec!["text"] },"output":["text"]})));
        models.insert(
            id.into(),
            ModelInfo {
                id: id.into(),
                name: name.into(),
                limit: if limit.is_empty() {
                    None
                } else {
                    Some(Value::Object(limit))
                },
                modalities,
                extra,
            },
        );
    }
    Ok(models)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn copilot_catalog_keeps_real_limits_routes_and_variants() {
        let models = parse_models("github-copilot", &json!({"data":[{"id":"claude","name":"Claude","supported_endpoints":["/v1/messages"],"capabilities":{"limits":{"max_prompt_tokens":100000,"max_output_tokens":8000},"supports":{"tool_calls":true,"vision":true,"adaptive_thinking":true,"reasoning_effort":["low","high"]}}}]})).unwrap();
        assert_eq!(models["claude"].limit.as_ref().unwrap()["context"], 100000);
        assert_eq!(models["claude"].extra["options"]["endpoint"], "messages");
        assert_eq!(
            models["claude"].extra["variants"]["high"]["thinking"]["type"],
            "adaptive"
        );
    }
    #[test]
    fn generic_catalog_does_not_invent_limits() {
        let models = parse_models("custom", &json!({"data":[{"id":"local-model"}]})).unwrap();
        assert!(models["local-model"].limit.is_none());
    }
}
