//! Resource descriptions shared by HTTP and terminal clients.
use crate::auth::AuthStore;
use crate::config::Config;
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;

pub fn provider_inventory(config: &Config, auth: &AuthStore) -> Value {
    let mut catalog: BTreeMap<String, Map<String, Value>> = BTreeMap::new();
    for mut model in config.catalog() {
        let Some(provider) = model
            .get("providerID")
            .and_then(Value::as_str)
            .map(str::to_owned)
        else {
            continue;
        };
        let Some(id) = model
            .get("modelID")
            .and_then(Value::as_str)
            .map(str::to_owned)
        else {
            continue;
        };
        model["id"] = json!(id);
        if let Some(fields) = model.as_object_mut() {
            fields.remove("modelID");
        }
        catalog.entry(provider).or_default().insert(id, model);
    }
    let all: Vec<Value> = catalog.into_iter().map(|(id, models)| {
        let provider = config.provider.get(&id);
        let name = provider.map(|provider| provider.name.as_str()).filter(|name| !name.is_empty()).unwrap_or(&id);
        let options = provider.map(|provider| json!({"baseURL": provider.options.base_url})).unwrap_or_else(|| json!({}));
        json!({"id": id, "name": name, "env": provider.map(|provider| provider.env.clone()).unwrap_or_default(), "source": "config", "options": options, "models": models})
    }).collect();
    let mut defaults = Map::new();
    for provider in &all {
        if let (Some(id), Some(models)) = (provider["id"].as_str(), provider["models"].as_object())
        {
            let prefix = format!("{id}/");
            let configured = config
                .model
                .strip_prefix(prefix.as_str())
                .filter(|model| models.contains_key(*model));
            if let Some(model) = configured.or_else(|| models.keys().next().map(String::as_str)) {
                defaults.insert(id.to_owned(), json!(model));
            }
        }
    }
    json!({"all": all, "connected": auth.connected_for(config), "default": defaults})
}

const BUILTIN_AGENTS: [(&str, &str, &str); 4] = [
    (
        "build",
        "Coding agent with the configured tool permissions.",
        "primary",
    ),
    (
        "plan",
        "Analyze and plan changes without editing the workspace.",
        "primary",
    ),
    (
        "general",
        "Delegate multi-step research and implementation tasks.",
        "subagent",
    ),
    (
        "explore",
        "Read and search a codebase without editing it.",
        "subagent",
    ),
];

fn build_agent_json(config: &Config, name: &str, builtin: Option<(&str, &str)>) -> Option<Value> {
    let definition = config.agent.get(name);
    if definition
        .and_then(|d| d.get("disable"))
        .and_then(Value::as_bool)
        == Some(true)
    {
        return None;
    }
    if builtin.is_none() && definition.is_none() {
        return None;
    }
    let rules = config.agent_permission_rules(name);
    let mut agent = match builtin {
        Some((description, mode)) => json!({
            "name": name,
            "description": description,
            "mode": mode,
            "native": true,
            "hidden": false,
            "options": {},
            "permission": rules
        }),
        None => json!({
            "name": name,
            "mode": "all",
            "native": false,
            "hidden": false,
            "options": {},
            "permission": rules
        }),
    };
    if let Some(properties) = definition.and_then(Value::as_object) {
        for (key, value) in properties {
            if key != "permission" && key != "disable" {
                agent[key] = value.clone();
            }
        }
    }
    agent["name"] = json!(name);
    Some(agent)
}

pub fn agent_json(config: &Config, name: &str) -> Option<Value> {
    let builtin = BUILTIN_AGENTS
        .iter()
        .find(|(n, _, _)| *n == name)
        .map(|(_, desc, mode)| (*desc, *mode));
    build_agent_json(config, name, builtin)
}

pub fn agents_json(config: &Config) -> Value {
    let mut definitions = BTreeMap::new();
    for (name, description, mode) in BUILTIN_AGENTS {
        if let Some(agent) = build_agent_json(config, name, Some((description, mode))) {
            definitions.insert(name.to_owned(), agent);
        }
    }
    for name in config.agent.keys() {
        if definitions.contains_key(name)
            || BUILTIN_AGENTS.iter().any(|(n, _, _)| *n == name.as_str())
        {
            continue;
        }
        if let Some(agent) = build_agent_json(config, name, None) {
            definitions.insert(name.clone(), agent);
        }
    }
    json!(definitions.into_values().collect::<Vec<_>>())
}

pub fn skills_json_for(directory: &str, config: &Config) -> Value {
    json!(crate::tools::skill_list_with_paths(
        Some(directory),
        &config.skill_paths()
    ))
}

pub fn commands_json(config: &Config) -> Value {
    static PLACEHOLDER: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let placeholder = PLACEHOLDER
        .get_or_init(|| regex::Regex::new(r"\$ARGUMENTS|\$\d+").expect("constant regex"));
    let commands: BTreeMap<_, _> = crate::commands::Registry::new(config)
        .configured()
        .map(|(name, entry)| {
            let mut entry = if entry.is_string() {
                json!({"template": entry})
            } else {
                entry.clone()
            };
            entry["name"] = json!(name);
            if entry.get("source").is_none() {
                entry["source"] = json!("command");
            }
            let template = entry
                .get("template")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let mut hints = Vec::new();
            for matched in placeholder.find_iter(template) {
                let hint = matched.as_str().to_owned();
                if !hints.contains(&hint) {
                    hints.push(hint);
                }
            }
            entry["hints"] = json!(hints);
            (name, entry)
        })
        .collect();
    json!(commands.into_values().collect::<Vec<_>>())
}
