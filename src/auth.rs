use crate::config::data_home;
use serde_json::Value;
use std::collections::HashMap;

#[derive(Clone, Debug)]
pub enum AuthEntry {
    Api { key: String },
    Oauth { access: String, refresh: String, expires: i64, enterprise_url: Option<String> },
    WellKnown { key: String, token: String },
}

pub struct AuthStore {
    entries: HashMap<String, AuthEntry>,
}

impl AuthStore {
    pub fn load() -> Self {
        let raw = if let Ok(c) = std::env::var("OPENCODE_AUTH_CONTENT") {
            c
        } else {
            std::fs::read_to_string(format!("{}/opencode/auth.json", data_home())).unwrap_or_default()
        };
        let v: Value = serde_json::from_str(&raw).unwrap_or(Value::Null);
        let mut entries = HashMap::new();
        if let Some(obj) = v.as_object() {
            for (k, e) in obj {
                let t = e.get("type").and_then(|t| t.as_str()).unwrap_or("");
                let entry = match t {
                    "api" => e.get("key").and_then(|k| k.as_str()).map(|key| AuthEntry::Api { key: key.to_string() }),
                    "oauth" => Some(AuthEntry::Oauth {
                        access: e.get("access").and_then(|x| x.as_str()).unwrap_or_default().to_string(),
                        refresh: e.get("refresh").and_then(|x| x.as_str()).unwrap_or_default().to_string(),
                        expires: e.get("expires").and_then(|x| x.as_i64()).unwrap_or(0),
                        enterprise_url: e.get("enterpriseUrl").and_then(|x| x.as_str()).map(String::from),
                    }),
                    "wellknown" => Some(AuthEntry::WellKnown {
                        key: e.get("key").and_then(|x| x.as_str()).unwrap_or_default().to_string(),
                        token: e.get("token").and_then(|x| x.as_str()).unwrap_or_default().to_string(),
                    }),
                    _ => None,
                };
                if let Some(en) = entry {
                    entries.insert(k.clone(), en);
                }
            }
        }
        Self { entries }
    }

    pub fn get(&self, provider: &str) -> Option<&AuthEntry> {
        self.entries.get(provider)
    }

    pub fn connected(&self) -> Vec<String> {
        let mut v: Vec<String> = self.entries.keys().cloned().collect();
        v.sort();
        v
    }
}
