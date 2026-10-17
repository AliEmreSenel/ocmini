use crate::types::{PermissionRequest, PermissionRule};
use std::sync::{Arc, Mutex};
use tokio::sync::oneshot;

#[derive(Clone, Default)]
pub struct PermissionState {
    pending: Arc<Mutex<Vec<(PermissionRequest, oneshot::Sender<String>)>>>,
}

pub fn matches(pattern: &str, value: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    if let Some(prefix) = pattern.strip_suffix('*') {
        return value.starts_with(prefix);
    }
    pattern == value
}

pub fn decide(rules: &[PermissionRule], name: &str, value: &str) -> &'static str {
    let mut action = "allow";
    for r in rules {
        if matches(&r.permission, name) && matches(&r.pattern, value) {
            action = match r.action.as_str() {
                "deny" => return "deny",
                "ask" => "ask",
                _ => "allow",
            };
        }
    }
    action
}

pub fn default_rules(cfg_perm: &std::collections::HashMap<String, String>) -> Vec<PermissionRule> {
    let mut rules = Vec::new();
    for (name, action) in cfg_perm {
        rules.push(PermissionRule { permission: name.clone(), pattern: "*".into(), action: action.clone() });
    }
    rules
}

impl PermissionState {
    pub fn ask(
        &self,
        session_id: &str,
        permission: &str,
        patterns: Vec<String>,
        metadata: serde_json::Value,
        always: Vec<String>,
    ) -> (PermissionRequest, oneshot::Receiver<String>) {
        let (tx, rx) = oneshot::channel();
        let req = PermissionRequest {
            id: crate::types::hex_id("per_"),
            session_id: session_id.to_string(),
            permission: permission.to_string(),
            patterns,
            metadata,
            always,
            time: crate::types::Time {
                created: crate::types::now_ms(),
                updated: None,
            },
        };
        self.pending.lock().unwrap().push((req.clone(), tx));
        (req, rx)
    }

    pub fn reject_all(&self, session_id: &str) {
        let mut p = self.pending.lock().unwrap();
        let mut drained = Vec::new();
        let mut keep = Vec::new();
        while let Some(item) = p.pop() {
            if item.0.session_id == session_id {
                drained.push((item.0.id.clone(), item.1));
            } else {
                keep.push(item);
            }
        }
        p.extend(keep);
        drop(p);
        for (id, tx) in drained {
            let _ = tx.send("reject".to_string());
            crate::engine::GLOBAL_BUS.emit(
                "permission.replied",
                serde_json::json!({"sessionID": session_id, "requestID": id, "reply": "reject"}),
            );
        }
    }

    pub fn reply(&self, id: &str, response: &str) -> bool {
        let mut p = self.pending.lock().unwrap();
        if let Some(i) = p.iter().position(|(r, _)| r.id == id) {
            let (req, tx) = p.remove(i);
            let _ = tx.send(response.to_string());
            crate::engine::GLOBAL_BUS.emit(
                "permission.replied",
                serde_json::json!({"sessionID": req.session_id, "requestID": req.id, "reply": response}),
            );
            return true;
        }
        false
    }

    pub fn list(&self) -> Vec<PermissionRequest> {
        self.pending.lock().unwrap().iter().map(|(r, _)| r.clone()).collect()
    }
}
