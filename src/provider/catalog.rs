use super::{Adapter, ChatRequest, Protocol};
use crate::auth::AuthStore;
use crate::config::{deep_merge, Config, ModelInfo, ProviderCfg};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
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

/// Upstream's ceiling for a generated reasoning budget.
const OUTPUT_TOKEN_MAX: u64 = 32_000;

/// OpenCode 1.19.3: `provider.<id>.models.<id>.reasoning_options` declares the
/// reasoning controls a model accepts, builds its variants from them and
/// replaces the catalog-derived ones; configured `variants` still merge on top.
/// Options no supported protocol maps leave the catalog variants in place, as
/// upstream does.
pub(crate) fn normalize_provider(id: &str, source: &ProviderCfg) -> ProviderCfg {
    let mut provider = source.clone();
    let npm_default = provider.npm.clone();
    let provider_options = serde_json::to_value(&provider.options).unwrap_or_else(|_| json!({}));
    for model in provider.models.values_mut() {
        let Some(options) = model
            .extra
            .get("reasoning_options")
            .and_then(Value::as_array)
            .cloned()
        else {
            continue;
        };
        let effective = super::merge_model_options(provider_options.clone(), Some(model));
        let protocol = effective
            .and_then(|options| {
                super::model_protocol(id, &npm_default, &options, Some(model), &model.id, false)
            })
            .unwrap_or(Protocol::Chat);
        let Some(generated) = reasoning_variants(&options, protocol, model) else {
            continue;
        };
        let mut variants = Value::Object(generated);
        let configured = model
            .extra
            .get("variants")
            .cloned()
            .filter(|value| value.is_object());
        if let Some(configured) = configured {
            deep_merge(&mut variants, configured);
        }
        model.extra.insert("variants".into(), variants);
    }
    provider
}

#[cfg(test)]
fn apply_reasoning_options(config: &mut Config) {
    for (id, provider) in &mut config.provider {
        *provider = normalize_provider(id, provider);
    }
}

/// Variants from a model's `reasoning_options`, mirroring upstream's
/// `reasoningVariants` for the protocols ocmini speaks. `None` means no
/// supported mapping exists, so the catalog variants stay in place.
fn reasoning_variants(
    options: &[Value],
    protocol: Protocol,
    model: &ModelInfo,
) -> Option<serde_json::Map<String, Value>> {
    let mut variants = serde_json::Map::new();
    // An empty option list means the model takes no reasoning variants at all.
    if options.is_empty() {
        return Some(variants);
    }
    // A declared effort list wins over the other option shapes.
    if let Some(efforts) = options
        .iter()
        .find(|option| option["type"] == "effort")
        .and_then(|option| option["values"].as_array())
    {
        for value in efforts {
            // A null entry is the "none" variant, as upstream.
            let effort = match value {
                Value::Null => "none",
                Value::String(effort) => effort.as_str(),
                _ => continue,
            };
            variants.insert(effort.into(), effort_variant(protocol, effort));
        }
        return Some(variants);
    }
    let budget = options
        .iter()
        .find(|option| option["type"] == "budget_tokens")?;
    // Only the Anthropic Messages protocol maps a token budget among the
    // protocols ocmini speaks, and a bare toggle is provider-SDK specific.
    if protocol != Protocol::Anthropic {
        return None;
    }
    let output = model
        .limit
        .as_ref()
        .and_then(|limit| limit["output"].as_u64())
        .unwrap_or(OUTPUT_TOKEN_MAX);
    let maximum = budget["max"]
        .as_u64()
        .map(|max| max.saturating_sub(1))
        .unwrap_or(OUTPUT_TOKEN_MAX - 1)
        .min(output.saturating_sub(1))
        .min(OUTPUT_TOKEN_MAX - 1);
    if maximum == 0 {
        return None;
    }
    let high = budget["min"]
        .as_u64()
        .unwrap_or(0)
        .max(maximum.div_ceil(2))
        .min(maximum);
    for (id, tokens) in [("high", high), ("max", maximum)] {
        variants.insert(
            id.into(),
            json!({"thinking":{"type":"enabled","budgetTokens":tokens}}),
        );
    }
    Some(variants)
}

fn effort_variant(protocol: Protocol, effort: &str) -> Value {
    if protocol == Protocol::Anthropic {
        json!({"thinking":{"type":"adaptive"},"output_config":{"effort":effort}})
    } else {
        json!({"reasoningEffort":effort})
    }
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
    let request = ChatRequest::default();
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
    let mut models = HashMap::new();
    let mut after: Option<String> = None;
    let mut seen = HashSet::new();
    loop {
        let mut request = settings.request(
            reqwest::Method::GET,
            &format!("{}/models", base.trim_end_matches('/')),
            protocol,
        );
        if let Some(after) = &after {
            request = request.query(&[("after_id", after.as_str()), ("limit", "100")]);
        }
        let response = settings
            .send_request(request)
            .await
            .map_err(|error| error.message)?;
        let bytes =
            crate::http::read_body(response, Some(Duration::from_secs(6)), 8 * 1024 * 1024).await?;
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
        if !seen.insert(next.clone()) {
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
                    let protocol = if supports["adaptive_thinking"] == true {
                        Protocol::Anthropic
                    } else {
                        Protocol::Chat
                    };
                    variants.insert(effort.into(), effort_variant(protocol, effort));
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

    fn config(value: Value) -> Config {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn reasoning_options_replace_catalog_variants_and_merge_configured_ones() {
        let mut cfg = config(json!({"provider":{"local":{
            "npm":"@ai-sdk/openai-compatible",
            "options":{"baseURL":"http://127.0.0.1:1/v1","apiKey":"key"},
            "models":{"m":{
                "reasoning_options":[{"type":"effort","values":["low",null]}],
                "variants":{"high":{"headers":{"x-mode":"high"}}},
            }}
        }}}));
        apply_reasoning_options(&mut cfg);
        let variants = &cfg.provider["local"].models["m"].extra["variants"];
        assert_eq!(variants["low"]["reasoningEffort"], "low");
        assert_eq!(variants["none"]["reasoningEffort"], "none");
        assert_eq!(variants["high"]["headers"]["x-mode"], "high");
        assert_eq!(variants.as_object().unwrap().len(), 3);
    }

    #[test]
    fn reasoning_options_follow_the_model_protocol() {
        let mut cfg = config(json!({
            "provider": {
                "anthropic": {
                    "npm": "@ai-sdk/anthropic",
                    "models": {
                        "claude": {
                            "reasoning_options": [{"type": "effort", "values": ["low"]}]
                        }
                    }
                },
                "local": {
                    "npm": "@ai-sdk/openai-compatible",
                    "models": {
                        "cleared": {
                            "reasoning_options": [],
                            "variants": {"high": {"reasoningEffort": "high"}}
                        },
                        "kept": {
                            "reasoning_options": [{"type": "toggle"}],
                            "variants": {"high": {"reasoningEffort": "high"}}
                        }
                    }
                }
            }
        }));
        apply_reasoning_options(&mut cfg);
        let claude = &cfg.provider["anthropic"].models["claude"].extra["variants"];
        assert_eq!(claude["low"]["thinking"]["type"], "adaptive");
        assert_eq!(claude["low"]["output_config"]["effort"], "low");
        // An empty option list removes the discovered variants; the model's own
        // configured variants still merge on top, exactly as upstream merges them.
        assert_eq!(
            cfg.provider["local"].models["cleared"].extra["variants"],
            json!({"high":{"reasoningEffort":"high"}})
        );
        // Options no supported protocol maps keep the discovered variants.
        assert_eq!(
            cfg.provider["local"].models["kept"].extra["variants"],
            json!({"high":{"reasoningEffort":"high"}})
        );
    }

    #[test]
    fn empty_reasoning_options_leaves_a_model_no_variants() {
        let mut cfg = config(json!({
            "provider": {
                "local": {
                    "npm": "@ai-sdk/openai-compatible",
                    "models": {"cleared": {"reasoning_options": []}}
                }
            }
        }));
        apply_reasoning_options(&mut cfg);
        assert_eq!(
            cfg.provider["local"].models["cleared"].extra["variants"],
            json!({})
        );
    }

    #[test]
    fn budget_token_options_build_anthropic_variants() {
        let mut cfg = config(json!({
            "provider": {
                "anthropic": {
                    "npm": "@ai-sdk/anthropic",
                    "models": {
                        "claude": {
                            "limit": {"context": 200000, "output": 16000},
                            "reasoning_options": [{"type": "budget_tokens", "min": 4000, "max": 12000}]
                        }
                    }
                }
            }
        }));
        apply_reasoning_options(&mut cfg);
        let variants = &cfg.provider["anthropic"].models["claude"].extra["variants"];
        // The budget stays under the model's output ceiling: 12000-1, and the
        // high variant is the midpoint floor clamped up to the configured min.
        assert_eq!(variants["high"]["thinking"]["budgetTokens"], 6000);
        assert_eq!(variants["max"]["thinking"]["budgetTokens"], 11999);
    }

    #[tokio::test]
    async fn discovery_reaches_all_pages_and_rejects_nonconsecutive_cursor_cycles() {
        use axum::{
            extract::{Query, State},
            routing::get,
            Json, Router,
        };
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };
        async fn endpoint(
            State(calls): State<Arc<AtomicUsize>>,
            Query(query): Query<HashMap<String, String>>,
        ) -> Json<Value> {
            let page = query
                .get("after_id")
                .and_then(|id| id.parse::<usize>().ok())
                .unwrap_or(0)
                + 1;
            calls.fetch_add(1, Ordering::Relaxed);
            Json(
                json!({"data":[{"id":format!("model-{page}")}],"has_more":page<12,"last_id":page.to_string()}),
            )
        }
        let calls = Arc::new(AtomicUsize::new(0));
        let router = Router::new()
            .route("/models", get(endpoint))
            .route("/cycle/models", get(|Query(query): Query<HashMap<String,String>>| async move {
                let cursor = match query.get("after_id").map(String::as_str) { Some("a") => "b", _ => "a" };
                Json(json!({"data":[{"id":format!("model-{cursor}")}],"has_more":true,"last_id":cursor}))
            }))
            .with_state(calls.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let auth = AuthStore::from_json("{}").unwrap();
        for (base, cycle) in [(url.clone(), false), (format!("{url}/cycle"), true)] {
            let cfg: Config = serde_json::from_value(json!({"provider":{"fixture":{"npm":"@ai-sdk/openai-compatible","options":{"baseURL":base,"apiKey":"fixture"}}}})).unwrap();
            let result = discover("fixture", &cfg, &auth).await;
            if cycle {
                assert!(result.unwrap_err().contains("repeated pagination cursor"));
            } else {
                assert_eq!(result.unwrap().len(), 12);
            }
        }
        assert_eq!(calls.load(Ordering::Relaxed), 12);
        server.abort();
    }
}
