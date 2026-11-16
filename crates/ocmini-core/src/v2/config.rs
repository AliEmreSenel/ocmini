//! Normalize native v2 configuration into the runtime's existing representation.
use crate::config::deep_merge;
use serde_json::{json, Value};

pub fn normalize(value: &mut Value) -> Result<(), String> {
    for (native, legacy) in [
        ("providers", "provider"),
        ("agents", "agent"),
        ("commands", "command"),
        ("permissions", "permission"),
    ] {
        if let Some(native) = value.as_object_mut().and_then(|v| v.remove(native)) {
            let old = value
                .as_object_mut()
                .and_then(|v| v.remove(legacy))
                .unwrap_or_else(|| json!({}));
            let mut merged = old;
            deep_merge(&mut merged, native);
            value[legacy] = merged;
        }
    }
    normalize_selection(value)?;
    if value["permission"]
        .as_array()
        .and_then(|v| v.first())
        .is_some_and(|v| v.get("effect").is_some())
    {
        value["permission"] =
            json!(super::wire::rules(&value["permission"]).map_err(|e| e.message)?);
    }
    if let Some(agents) = value.get_mut("agent").and_then(Value::as_object_mut) {
        for agent in agents.values_mut() {
            normalize_selection(agent)?;
            if let Some(v) = agent.as_object_mut().and_then(|a| a.remove("permissions")) {
                agent["permission"] = json!(super::wire::rules(&v).map_err(|e| e.message)?);
            }
            for (native, legacy) in [("system", "prompt"), ("disabled", "disable")] {
                if let Some(v) = agent.as_object_mut().and_then(|a| a.remove(native)) {
                    agent[legacy] = v;
                }
            }
        }
    }
    if let Some(commands) = value.get_mut("command").and_then(Value::as_object_mut) {
        for command in commands.values_mut().filter(|v| v.is_object()) {
            normalize_selection(command)?;
        }
    }
    if let Some(providers) = value.get_mut("provider").and_then(Value::as_object_mut) {
        for provider in providers.values_mut() {
            if let Some(package) = provider.as_object_mut().and_then(|p| p.remove("package")) {
                let name = package
                    .as_str()
                    .ok_or("Provider package must be a string")?;
                provider["npm"] = json!(match name {
                    "@opencode/ai/providers/openai-compatible" => "@ai-sdk/openai-compatible",
                    "@opencode/ai/providers/openai" => "@ai-sdk/openai",
                    "@opencode/ai/providers/anthropic" => "@ai-sdk/anthropic",
                    other => other,
                });
            }
            if let Some(settings) = provider.as_object_mut().and_then(|p| p.remove("settings")) {
                let mut options = provider["options"].take();
                deep_merge(&mut options, settings);
                provider["options"] = options;
            }
            if let Some(models) = provider.get_mut("models").and_then(Value::as_object_mut) {
                for model in models.values_mut() {
                    if let Some(settings) = model.as_object_mut().and_then(|m| m.remove("settings"))
                    {
                        let mut options = model["options"].take();
                        deep_merge(&mut options, settings);
                        model["options"] = options;
                    }
                    if let Some(variants) = model["variants"].as_array() {
                        let mut normalized = serde_json::Map::new();
                        for variant in variants {
                            let id = variant["id"]
                                .as_str()
                                .ok_or("Variant id must be a string")?;
                            let mut body =
                                variant["settings"].as_object().cloned().unwrap_or_default();
                            if let Some(options) = variant["body"].as_object() {
                                body.extend(options.clone());
                            }
                            normalized.insert(id.into(), json!(body));
                        }
                        model["variants"] = json!(normalized);
                    }
                    if let Some(reasoning) = model["capabilities"].get("reasoning") {
                        if reasoning["supported"] == false {
                            model["reasoning_options"] = json!([]);
                        } else if let Some(effort) = reasoning.get("effort") {
                            model["reasoning_options"] = json!([{"type":"effort","values":effort}]);
                        }
                    }
                }
            }
        }
    }
    if let Some(servers) = value.get_mut("mcp").and_then(Value::as_object_mut) {
        for server in servers.values_mut() {
            if let Some(disabled) = server.as_object_mut().and_then(|s| s.remove("disabled")) {
                server["enabled"] =
                    json!(!disabled.as_bool().ok_or("MCP disabled must be boolean")?);
            }
            if let Some(timeout) = server["timeout"].as_object() {
                if let Some(execution) = timeout.get("execution") {
                    server["timeout"] = execution.clone();
                } else {
                    server.as_object_mut().unwrap().remove("timeout");
                }
            }
        }
    }
    Ok(())
}
fn normalize_selection(value: &mut Value) -> Result<(), String> {
    if value["model"].is_object() {
        if let Some(variant) = value["model"].get("variant").cloned() {
            value["variant"] = variant;
        }
        value["model"] = json!(selection(&value["model"])?);
    } else if let Some((model, variant)) = value["model"].as_str().and_then(|s| s.split_once('#')) {
        let (model, variant) = (model.to_owned(), variant.to_owned());
        if variant.is_empty() || variant.contains('#') {
            return Err("Invalid model variant".into());
        }
        value["model"] = json!(model);
        value["variant"] = json!(variant);
    }
    Ok(())
}
fn selection(value: &Value) -> Result<String, String> {
    let provider = value["providerID"]
        .as_str()
        .ok_or("Model selection needs providerID")?;
    let model = value["model"]
        .as_str()
        .or_else(|| value["id"].as_str())
        .ok_or("Model selection needs model")?;
    if provider.is_empty() || model.is_empty() {
        return Err("Model selection must not be empty".into());
    }
    Ok(format!("{provider}/{model}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_config_maps_to_existing_provider_and_permission_behavior() {
        let mut v = json!({"provider":{"local":{"options":{"baseURL":"old"},"models":{"keep":{"name":"Keep"}}}},"providers":{"local":{"package":"@opencode/ai/providers/openai-compatible","settings":{"baseURL":"new"},"models":{"m":{"variants":[{"id":"high","settings":{"reasoningEffort":"high"}}]}}}},"permissions":[{"action":"shell","resource":"*","effect":"ask"}],"model":{"providerID":"local","model":"m"}});
        normalize(&mut v).unwrap();
        assert_eq!(v["model"], "local/m");
        assert_eq!(v["provider"]["local"]["npm"], "@ai-sdk/openai-compatible");
        assert_eq!(v["provider"]["local"]["options"]["baseURL"], "new");
        assert_eq!(v["provider"]["local"]["models"]["keep"]["name"], "Keep");
        assert_eq!(
            v["provider"]["local"]["models"]["m"]["variants"]["high"]["reasoningEffort"],
            "high"
        );
        assert_eq!(v["permission"][0]["permission"], "bash");
        let mut selections = json!({"model":{"providerID":"local","model":"m","variant":"high"},"agents":{"plan":{"model":"local/m#low"}},"commands":{"check":{"template":"Check","model":{"providerID":"local","model":"m","variant":"medium"}}}});
        normalize(&mut selections).unwrap();
        assert_eq!(selections["variant"], "high");
        assert_eq!(selections["agent"]["plan"]["model"], "local/m");
        assert_eq!(selections["agent"]["plan"]["variant"], "low");
        assert_eq!(selections["command"]["check"]["model"], "local/m");
        assert_eq!(selections["command"]["check"]["variant"], "medium");
        let mut empty = json!({});
        normalize(&mut empty).unwrap();
        assert_eq!(empty, json!({}));
    }
    #[test]
    fn compaction_buffer_and_keep_use_native_settings_and_context_ceiling() {
        let cfg: crate::config::CompactionCfg = serde_json::from_value(
            json!({"buffer":"20%","keep":{"tokens":123},"preserve_recent_tokens":999}),
        )
        .unwrap();
        assert_eq!(cfg.usable(Some(10000), Some(20000), 2000), Some(8000));
        assert_eq!(cfg.recent_bytes(Some(8000)), 492);
        for buffer in [json!("0%"), json!("100%"), json!("NaN%"), json!(-1)] {
            assert!(serde_json::from_value::<crate::config::CompactionCfg>(
                json!({"buffer":buffer})
            )
            .is_err());
        }
    }
}
