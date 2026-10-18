use crate::types::PermissionRule;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

#[derive(Deserialize, Serialize, Clone, Debug, Default)]
pub struct ModelInfo {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modalities: Option<Value>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Deserialize, Serialize, Clone, Debug, Default)]
pub struct ProviderCfg {
    #[serde(default)]
    pub npm: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub options: ProviderOptions,
    #[serde(default)]
    pub models: HashMap<String, ModelInfo>,
    #[serde(default)]
    pub env: Vec<String>,
    #[serde(default)]
    pub headers: HashMap<String, String>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Deserialize, Serialize, Clone, Debug, Default)]
pub struct ProviderOptions {
    #[serde(rename = "baseURL", default)]
    pub base_url: String,
    #[serde(rename = "apiKey", default)]
    pub api_key: String,
    #[serde(default)]
    pub headers: HashMap<String, String>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Deserialize, Serialize, Clone, Debug, Default)]
pub struct McpLocal {
    #[serde(default)]
    pub r#type: String,
    #[serde(default)]
    pub command: Vec<String>,
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub headers: HashMap<String, String>,
    #[serde(default)]
    pub environment: HashMap<String, String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub timeout: Option<u64>,
    #[serde(default)]
    pub oauth: Value,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Deserialize, Serialize, Clone, Debug, Default)]
pub struct CompactionCfg {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prune: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reserved: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preserve_recent_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tail_turns: Option<u64>,
}

impl CompactionCfg {
    pub fn automatic(&self) -> bool {
        self.auto.unwrap_or(true)
    }

    pub fn usable(&self, context: Option<u64>, input: Option<u64>, output: u64) -> Option<u64> {
        context.filter(|context| *context > 0).map(|context| {
            if let Some(input) = input.filter(|input| *input > 0) {
                input.saturating_sub(self.reserved.unwrap_or(output.min(20_000)))
            } else {
                context.saturating_sub(output)
            }
        })
    }

    pub fn recent_bytes(&self, usable: Option<u64>) -> usize {
        let tokens = self.preserve_recent_tokens.unwrap_or_else(|| {
            usable
                .map(|usable| (usable / 4).clamp(2_000, 15_000))
                .unwrap_or(15_000)
        });
        usize::try_from(tokens.saturating_mul(4)).unwrap_or(usize::MAX)
    }
}

#[derive(Deserialize, Serialize, Clone, Debug, Default)]
pub struct Config {
    #[serde(default)]
    pub provider: HashMap<String, ProviderCfg>,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub small_model: String,
    #[serde(default)]
    pub permission: Value,
    #[serde(default)]
    pub instructions: Vec<String>,
    #[serde(default)]
    pub mcp: HashMap<String, McpLocal>,
    #[serde(default)]
    pub agent: HashMap<String, Value>,
    #[serde(default)]
    pub command: HashMap<String, Value>,
    #[serde(default)]
    pub skills: Value,
    #[serde(default)]
    pub compaction: CompactionCfg,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent_depth: Option<u64>,
    #[serde(default)]
    pub disabled_providers: Vec<String>,
    #[serde(default)]
    pub enabled_providers: Option<Vec<String>>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

impl Config {
    pub fn load() -> Result<Config, String> {
        let directory = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        Self::load_for(&directory.to_string_lossy())
    }

    /// Configs are loaded from disk per request scope. With many concurrent
    /// sessions this is a hot disk path, so entries are cached briefly; T3
    /// config overlays take effect within the TTL.
    pub fn load_for(directory: &str) -> Result<Config, String> {
        static CACHE: std::sync::OnceLock<
            std::sync::Mutex<std::collections::HashMap<String, (std::time::Instant, Config)>>,
        > = std::sync::OnceLock::new();
        let cache = CACHE.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()));
        if let Some((created, config)) = cache.lock().unwrap().get(directory) {
            if created.elapsed() < std::time::Duration::from_secs(3) {
                return Ok(config.clone());
            }
        }
        let config = Self::load_uncached(directory)?;
        cache.lock().unwrap().insert(
            directory.to_string(),
            (std::time::Instant::now(), config.clone()),
        );
        Ok(config)
    }

    fn load_uncached(directory: &str) -> Result<Config, String> {
        let directory = absolute_path(
            Path::new(directory),
            &std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        );
        let global = PathBuf::from(config_home()).join("opencode");
        let mut value = json!({});
        let mut seen = HashSet::new();
        for name in ["opencode.json", "opencode.jsonc"] {
            merge_file(&mut value, &global.join(name), false, &mut seen)?;
        }
        load_markdown(&mut value, &global, "agent", "prompt")?;
        load_markdown(&mut value, &global, "command", "template")?;
        if let Ok(path) = std::env::var("OPENCODE_CONFIG") {
            if !path.trim().is_empty() {
                merge_file(
                    &mut value,
                    &absolute_path(Path::new(&path), &directory),
                    true,
                    &mut seen,
                )?;
            }
        }
        let mut ancestors: Vec<_> = directory.ancestors().collect();
        ancestors.reverse();
        if std::env::var("OPENCODE_DISABLE_PROJECT_CONFIG")
            .map(|v| v != "1" && v != "true")
            .unwrap_or(true)
        {
            for base in ancestors {
                for name in ["opencode.json", "opencode.jsonc"] {
                    merge_file(&mut value, &base.join(name), false, &mut seen)?;
                }
                let root = base.join(".opencode");
                if root.is_dir() {
                    for name in ["opencode.json", "opencode.jsonc"] {
                        merge_file(&mut value, &root.join(name), false, &mut seen)?;
                    }
                    load_markdown(&mut value, &root, "agent", "prompt")?;
                    load_markdown(&mut value, &root, "command", "template")?;
                }
            }
        }
        if let Ok(path) = std::env::var("OPENCODE_CONFIG_DIR") {
            if !path.trim().is_empty() {
                let root = absolute_path(Path::new(&path), &directory);
                for name in ["opencode.json", "opencode.jsonc"] {
                    merge_file(&mut value, &root.join(name), false, &mut seen)?;
                }
                load_markdown(&mut value, &root, "agent", "prompt")?;
                load_markdown(&mut value, &root, "command", "template")?;
            }
        }
        if let Ok(content) = std::env::var("OPENCODE_CONFIG_CONTENT") {
            if !content.trim().is_empty() {
                let overlay = parse_config(&content, "OPENCODE_CONFIG_CONTENT", &directory)?;
                deep_merge(&mut value, overlay);
            }
        }
        let mut cfg: Config = serde_json::from_value(value)
            .map_err(|e| format!("invalid OpenCode configuration: {e}"))?;
        validate_permission(&cfg.permission)?;
        for (pid, p) in cfg.provider.iter_mut() {
            if p.name.is_empty() {
                p.name = pid.clone();
            }
            for (mid, m) in p.models.iter_mut() {
                if m.id.is_empty() {
                    m.id = mid.clone();
                }
                if m.name.is_empty() {
                    m.name = mid.clone();
                }
            }
        }
        Ok(cfg)
    }

    pub fn provider_enabled(&self, id: &str) -> bool {
        !self.disabled_providers.iter().any(|item| item == id)
            && self
                .enabled_providers
                .as_ref()
                .map(|items| items.iter().any(|item| item == id))
                .unwrap_or(true)
    }

    pub fn catalog(&self) -> Vec<Value> {
        let mut out = Vec::new();
        for (pid, p) in &self.provider {
            if !self.provider_enabled(pid) {
                continue;
            }
            let whitelist = p.extra.get("whitelist").and_then(Value::as_array);
            let blacklist = p.extra.get("blacklist").and_then(Value::as_array);
            for (mid, m) in &p.models {
                if blacklist
                    .map(|list| list.iter().any(|v| v == mid))
                    .unwrap_or(false)
                    || whitelist
                        .map(|list| !list.iter().any(|v| v == mid))
                        .unwrap_or(false)
                    || m.extra.get("disabled").and_then(Value::as_bool) == Some(true)
                {
                    continue;
                }
                let mut model = serde_json::to_value(m).unwrap_or_else(|_| json!({}));
                model["id"] = json!(format!("{pid}/{mid}"));
                model["providerID"] = json!(pid);
                model["modelID"] = json!(mid);
                out.push(model);
            }
        }
        out.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
        out
    }

    pub fn provider(&self, id: &str) -> Option<&ProviderCfg> {
        self.provider.get(id)
    }

    pub fn permission_rules(&self) -> Vec<PermissionRule> {
        permission_rules(&self.permission)
    }

    pub fn skill_paths(&self) -> Vec<String> {
        let paths = if self.skills.is_array() {
            Some(&self.skills)
        } else {
            self.skills.get("paths")
        };
        paths
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default()
    }
}

pub fn permission_rules(value: &Value) -> Vec<PermissionRule> {
    let mut out = Vec::new();
    if let Some(action) = value.as_str() {
        out.push(PermissionRule {
            permission: "*".into(),
            pattern: "*".into(),
            action: action.into(),
        });
    }
    if let Some(rules) = value.as_object() {
        for (permission, patterns) in rules {
            if let Some(action) = patterns.as_str() {
                out.push(PermissionRule {
                    permission: permission.clone(),
                    pattern: "*".into(),
                    action: action.into(),
                });
            }
            if let Some(patterns) = patterns.as_object() {
                for (pattern, action) in patterns {
                    if let Some(action) = action.as_str() {
                        out.push(PermissionRule {
                            permission: permission.clone(),
                            pattern: pattern.clone(),
                            action: action.into(),
                        });
                    }
                }
            }
        }
    }
    out
}

fn validate_permission(value: &Value) -> Result<(), String> {
    if value.is_null() {
        return Ok(());
    }
    if let Some(action) = value.as_str() {
        return if ["allow", "ask", "deny"].contains(&action) {
            Ok(())
        } else {
            Err(format!("invalid permission action: {action}"))
        };
    }
    if let Some(map) = value.as_object() {
        for child in map.values() {
            validate_permission(child)?;
        }
        return Ok(());
    }
    Err("permission must be allow, ask, deny, or an object of permission rules".into())
}

fn merge_file(
    base: &mut Value,
    path: &Path,
    required: bool,
    seen: &mut HashSet<PathBuf>,
) -> Result<(), String> {
    let key = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    if !seen.insert(key) {
        return Ok(());
    }
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if !required && error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(format!("cannot read config {}: {error}", path.display())),
    };
    deep_merge(
        base,
        parse_config(
            &text,
            &path.display().to_string(),
            path.parent().unwrap_or(Path::new(".")),
        )?,
    );
    Ok(())
}

fn parse_config(text: &str, source: &str, base: &Path) -> Result<Value, String> {
    let mut value: Value = serde_json_lenient::from_str(text.trim_start_matches('\u{feff}'))
        .map_err(|e| format!("invalid config {source}: {e}"))?;
    if !value.is_object() {
        return Err(format!("config {source} must be a JSON object"));
    }
    interpolate(&mut value, base).map_err(|e| format!("config {source}: {e}"))?;
    if let Some(paths) = value.get_mut("instructions").and_then(Value::as_array_mut) {
        for path in paths {
            if let Some(text) = path.as_str() {
                if !text.starts_with("http://") && !text.starts_with("https://") {
                    *path = json!(absolute_path(Path::new(text), base).to_string_lossy());
                }
            }
        }
    }
    let paths = if value["skills"].is_array() {
        value.get_mut("skills")
    } else {
        value.get_mut("skills").and_then(|v| v.get_mut("paths"))
    };
    if let Some(paths) = paths.and_then(Value::as_array_mut) {
        for path in paths {
            if let Some(text) = path.as_str() {
                *path = json!(absolute_path(Path::new(text), base).to_string_lossy());
            }
        }
    }
    if let Some(servers) = value.get_mut("mcp").and_then(Value::as_object_mut) {
        for server in servers.values_mut() {
            if let Some(cwd) = server.get("cwd").and_then(Value::as_str) {
                server["cwd"] = json!(absolute_path(Path::new(cwd), base).to_string_lossy());
            }
        }
    }
    Ok(value)
}

pub fn deep_merge(base: &mut Value, overlay: Value) {
    match (base, overlay) {
        (Value::Object(base), Value::Object(overlay)) => {
            for (key, value) in overlay {
                if let Some(previous) = base.get_mut(&key) {
                    if key == "instructions" {
                        if let (Some(a), Some(b)) = (previous.as_array_mut(), value.as_array()) {
                            for item in b {
                                if !a.contains(item) {
                                    a.push(item.clone());
                                }
                            }
                            continue;
                        }
                    }
                    deep_merge(previous, value);
                } else {
                    base.insert(key, value);
                }
            }
        }
        (base, overlay) => *base = overlay,
    }
}

fn interpolate(value: &mut Value, base: &Path) -> Result<(), String> {
    match value {
        Value::String(text) => {
            let mut rest = text.as_str();
            let mut output = String::new();
            while let Some(start) = rest.find('{') {
                output.push_str(&rest[..start]);
                rest = &rest[start..];
                let Some(end) = rest.find('}') else {
                    break;
                };
                let token = &rest[1..end];
                if let Some(name) = token.strip_prefix("env:") {
                    output.push_str(&std::env::var(name).unwrap_or_default());
                } else if let Some(path) = token.strip_prefix("file:") {
                    let path = absolute_path(Path::new(path), base);
                    let content = std::fs::read_to_string(&path)
                        .map_err(|e| format!("cannot load {}: {e}", path.display()))?;
                    output.push_str(content.trim_end());
                } else {
                    output.push_str(&rest[..=end]);
                }
                rest = &rest[end + 1..];
            }
            output.push_str(rest);
            *text = output;
        }
        Value::Array(items) => {
            for item in items {
                interpolate(item, base)?;
            }
        }
        Value::Object(items) => {
            for item in items.values_mut() {
                interpolate(item, base)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn load_markdown(value: &mut Value, root: &Path, kind: &str, body_key: &str) -> Result<(), String> {
    for folder in [kind.to_string(), format!("{kind}s")] {
        let directory = root.join(folder);
        if !directory.is_dir() {
            continue;
        }
        for entry in walkdir::WalkDir::new(&directory)
            .max_depth(8)
            .into_iter()
            .filter_map(Result::ok)
        {
            if !entry.file_type().is_file()
                || entry.path().extension().and_then(|e| e.to_str()) != Some("md")
            {
                continue;
            }
            let content = std::fs::read_to_string(entry.path())
                .map_err(|e| format!("cannot read {}: {e}", entry.path().display()))?;
            let name = entry
                .path()
                .strip_prefix(&directory)
                .unwrap_or(entry.path())
                .with_extension("")
                .to_string_lossy()
                .replace('\\', "/");
            let (frontmatter, body) = markdown_frontmatter(&content)
                .map_err(|e| format!("invalid frontmatter in {}: {e}", entry.path().display()))?;
            let mut item = Value::Object(frontmatter);
            item[body_key] = json!(body.trim());
            if value.get(kind).is_none() {
                value[kind] = json!({});
            }
            if let Some(map) = value.get_mut(kind).and_then(Value::as_object_mut) {
                if let Some(current) = map.get_mut(&name) {
                    deep_merge(current, item);
                } else {
                    map.insert(name, item);
                }
            }
        }
    }
    Ok(())
}

fn markdown_frontmatter(content: &str) -> Result<(Map<String, Value>, &str), String> {
    let Some(rest) = content
        .strip_prefix("---\n")
        .or_else(|| content.strip_prefix("---\r\n"))
    else {
        return Ok((Map::new(), content));
    };
    let mut offset = 0;
    for line in rest.split_inclusive('\n') {
        if line.trim_end_matches(['\r', '\n']) == "---" {
            let frontmatter: Value =
                yaml_serde::from_str(&rest[..offset]).map_err(|e| e.to_string())?;
            let map = if frontmatter.is_null() {
                Map::new()
            } else {
                frontmatter
                    .as_object()
                    .cloned()
                    .ok_or("YAML frontmatter must be a mapping")?
            };
            return Ok((map, &rest[offset + line.len()..]));
        }
        offset += line.len();
    }
    Err("missing closing --- delimiter".into())
}

pub fn config_home() -> String {
    std::env::var("XDG_CONFIG_HOME").unwrap_or_else(|_| format!("{}/.config", home()))
}
fn home() -> String {
    std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_else(|_| ".".into())
}
pub fn data_home() -> String {
    std::env::var("XDG_DATA_HOME").unwrap_or_else(|_| format!("{}/.local/share", home()))
}
pub fn store_path() -> PathBuf {
    Path::new(&data_home()).join("ocmini").join("ocmini.db")
}

pub fn absolute_path(path: &Path, base: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    if text == "~" {
        return PathBuf::from(home());
    }
    if let Some(rest) = text.strip_prefix("~/").or_else(|| text.strip_prefix("~\\")) {
        return PathBuf::from(home()).join(rest);
    }
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn compaction_configuration_controls_headroom_and_retention() {
        let cfg: Config = serde_json::from_value(json!({"compaction": {
            "auto":false,"prune":true,"reserved":500,"tail_turns":1,"preserve_recent_tokens":1000
        },"subagent_depth":12}))
        .unwrap();
        assert!(!cfg.compaction.automatic());
        assert_eq!(cfg.compaction.prune, Some(true));
        assert_eq!(
            cfg.compaction.usable(Some(10_000), Some(9_000), 2_000),
            Some(8_500)
        );
        assert_eq!(
            cfg.compaction.usable(Some(10_000), None, 2_000),
            Some(8_000)
        );
        assert_eq!(cfg.compaction.recent_bytes(Some(8_500)), 4_000);
        assert_eq!(cfg.subagent_depth, Some(12));
        let defaults = Config::default();
        assert!(defaults.compaction.automatic());
        assert_eq!(defaults.compaction.prune, None);
        assert_eq!(defaults.compaction.recent_bytes(Some(128_000)), 60_000);
        assert_eq!(defaults.compaction.recent_bytes(Some(4_000)), 8_000);
        assert_eq!(defaults.compaction.usable(Some(0), None, 2_000), None);
        for config in [
            json!({"subagent_depth":-1}),
            json!({"compaction":{"reserved":-1}}),
            json!({"compaction":{"auto":"yes"}}),
        ] {
            assert!(serde_json::from_value::<Config>(config).is_err());
        }
    }
    #[test]
    fn jsonc_rejects_json5_and_malformed_input() {
        for text in [
            "{key: 1}",
            "{'key': 1}",
            "{\"a\":1 \"b\":2}",
            "{\"a\":0x10}",
            "{} {}",
            "// empty",
            "[]",
        ] {
            assert!(
                parse_config(text, "test", Path::new(".")).is_err(),
                "{text}"
            );
        }
        assert_eq!(
            parse_config(
                "\u{feff}{\"permission\": {\"*\": \"ask\", \"read\": \"allow\",},}",
                "test",
                Path::new(".")
            )
            .unwrap()["permission"]
                .as_object()
                .unwrap()
                .keys()
                .collect::<Vec<_>>(),
            ["*", "read"]
        );
    }
    #[test]
    fn jsonc_preserves_unicode_and_string_punctuation() {
        let value: Value = parse_config(
            "{\n// note\n\"text\":\"café 中文 ,} https://x/\\\"\", /* c */ \"array\":[1,2,],\n}",
            "test",
            Path::new("."),
        )
        .unwrap();
        assert_eq!(value["text"], "café 中文 ,} https://x/\"");
        assert_eq!(value["array"], json!([1, 2]));
        assert!(parse_config("{} /* unfinished", "test", Path::new(".")).is_err());
    }
    #[test]
    fn overlays_preserve_provider_models_and_every_config_section() {
        let mut base = json!({"provider":{"local":{"options":{"baseURL":"http://localhost/v1"},"models":{"a":{}}}},"permission":{"*":"ask","edit":"deny"},"instructions":["a"]});
        deep_merge(
            &mut base,
            json!({"provider":{"local":{"options":{"apiKey":"secret"},"models":{"b":{"variants":{"careful":{"reasoningEffort":"high"}}}}}},"permission":{"edit":"allow"},"mcp":{"x":{"enabled":false}},"instructions":["b","a"]}),
        );
        let cfg: Config = serde_json::from_value(base).unwrap();
        assert_eq!(cfg.provider["local"].models.len(), 2);
        assert_eq!(
            cfg.provider["local"].options.base_url,
            "http://localhost/v1"
        );
        assert_eq!(cfg.permission_rules().len(), 2);
        assert_eq!(cfg.instructions, ["a", "b"]);
        assert_eq!(cfg.mcp["x"].enabled, Some(false));
        assert!(
            serde_json::to_value(&cfg.provider["local"].models["b"]).unwrap()["variants"]
                .is_object()
        );
    }
    #[test]
    fn nested_permissions_and_catalog_filtering() {
        let cfg: Config = serde_json::from_value(json!({"permission":{"bash":{"*":"ask","git status":"allow"},"edit":"deny"},"provider":{"x":{"blacklist":["hidden"],"models":{"visible":{"name":"Display","variants":{"high":{}}},"hidden":{}}}}})).unwrap();
        assert_eq!(cfg.permission_rules().len(), 3);
        let models = cfg.catalog();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0]["id"], "x/visible");
        assert!(models[0]["variants"]["high"].is_object());
    }
}
