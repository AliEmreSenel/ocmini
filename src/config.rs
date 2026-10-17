use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

#[derive(Deserialize, Serialize, Clone, Debug, Default)]
pub struct ModelInfo {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub limit: Option<Value>,
    #[serde(default)]
    pub modalities: Option<Value>,
}

#[derive(Deserialize, Clone, Debug, Default)]
pub struct ProviderCfg {
    #[serde(rename = "npm", default)]
    pub npm: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub options: ProviderOptions,
    #[serde(default)]
    pub models: HashMap<String, ModelInfo>,
}

#[derive(Deserialize, Clone, Debug, Default)]
pub struct ProviderOptions {
    #[serde(rename = "baseURL", default)]
    pub base_url: String,
    #[serde(rename = "apiKey", default)]
    pub api_key: String,
}

#[derive(Deserialize, Clone, Debug, Default)]
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
}

#[derive(Deserialize, Clone, Debug, Default)]
pub struct Config {
    #[serde(default)]
    pub provider: HashMap<String, ProviderCfg>,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub permission: HashMap<String, String>,
    #[serde(default)]
    pub instructions: Vec<String>,
    #[serde(default)]
    pub mcp: HashMap<String, McpLocal>,
    #[serde(default)]
    pub agent: HashMap<String, Value>,
    #[serde(default)]
    pub command: HashMap<String, Value>,
}

impl Config {
    pub fn load() -> Config {
        let mut base = String::new();
        let cfg_home = std::env::var("XDG_CONFIG_HOME").unwrap_or_else(|_| format!("{}/.config", std::env::var("HOME").unwrap()));
        if let Ok(s) = std::fs::read_to_string(format!("{cfg_home}/opencode/opencode.json")) {
            base = s;
        }
        let mut cfg: Config = serde_json::from_str(&strip_jsonc(&base)).unwrap_or_default();
        if let Ok(overlay) = std::env::var("OPENCODE_CONFIG_CONTENT") {
            if !overlay.trim().is_empty() && overlay != "{}" {
                if let Ok(v) = serde_json::from_str::<Value>(&strip_jsonc(&overlay)) {
                    if let Some(p) = v.get("provider").and_then(|p| p.as_object()) {
                        for (k, pv) in p {
                            if let Ok(pc) = serde_json::from_value::<ProviderCfg>(pv.clone()) {
                                cfg.provider.insert(k.clone(), pc);
                            }
                        }
                    }
                    if let Some(m) = v.get("model").and_then(|m| m.as_str()) {
                        cfg.model = m.to_string();
                    }
                }
            }
        }
        cfg
    }

    pub fn catalog(&self) -> Vec<Value> {
        let mut out = Vec::new();
        for (pid, p) in &self.provider {
            for (mid, m) in &p.models {
                out.push(serde_json::json!({"id": format!("{pid}/{mid}"), "providerID": pid, "modelID": mid, "name": m.name}));
            }
        }
        out
    }

    pub fn provider(&self, id: &str) -> Option<&ProviderCfg> {
        self.provider.get(id)
    }
}

pub fn data_home() -> String {
    std::env::var("XDG_DATA_HOME").unwrap_or_else(|_| format!("{}/.local/share", std::env::var("HOME").unwrap()))
}

pub fn store_path() -> std::path::PathBuf {
    std::path::Path::new(&data_home()).join("ocmini").join("ocmini.db")
}

pub fn strip_jsonc(src: &str) -> String {
    let mut out = String::with_capacity(src.len());
    let b = src.as_bytes();
    let mut i = 0usize;
    let mut in_str = false;
    while i < b.len() {
        let c = b[i] as char;
        if in_str {
            out.push(c);
            if c == '\\' && i + 1 < b.len() {
                out.push(b[i + 1] as char);
                i += 2;
                continue;
            }
            if c == '"' {
                in_str = false;
            }
            i += 1;
            continue;
        }
        match c {
            '"' => {
                in_str = true;
                out.push(c);
                i += 1;
            }
            '/' if i + 1 < b.len() && b[i + 1] == b'/' => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            '/' if i + 1 < b.len() && b[i + 1] == b'*' => {
                i += 2;
                while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                    i += 1;
                }
                i += 2;
            }
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }
    let mut res = String::with_capacity(out.len());
    let bytes = out.as_bytes();
    let mut k = 0usize;
    while k < bytes.len() {
        if bytes[k] == b',' {
            let mut j = k + 1;
            while j < bytes.len() && (bytes[j] as char).is_whitespace() {
                j += 1;
            }
            if j < bytes.len() && (bytes[j] == b'}' || bytes[j] == b']') {
                k += 1;
                continue;
            }
        }
        res.push(bytes[k] as char);
        k += 1;
    }
    res
}
