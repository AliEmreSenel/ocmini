use crate::types::{PermissionRequest, PermissionRule, Time};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use tokio::sync::oneshot;

type Pending = (PermissionRequest, oneshot::Sender<String>);

#[derive(Clone, Default)]
pub struct PermissionState {
    pending: Arc<Mutex<Vec<Pending>>>,
}

pub fn matches(pattern: &str, value: &str) -> bool {
    let mut regex = String::from("(?s)^");
    for c in pattern.chars() {
        match c {
            '*' => regex.push_str(".*"),
            '?' => regex.push('.'),
            c => regex.push_str(&regex::escape(&c.to_string())),
        }
    }
    regex.push('$');
    regex::Regex::new(&regex).is_ok_and(|re| re.is_match(value))
}

/// OpenCode uses the last matching rule: session overrides and 'always' replies
/// must be able to override an earlier broad default.
pub fn decide(rules: &[PermissionRule], name: &str, value: &str) -> &'static str {
    for rule in rules.iter().rev() {
        if matches(&rule.permission, name) && matches(&rule.pattern, value) {
            return match rule.action.as_str() {
                "allow" => "allow",
                "ask" => "ask",
                _ => "deny",
            };
        }
    }
    "allow"
}

impl PermissionState {
    pub fn ask_for_tool(
        &self,
        sid: &str,
        permission: &str,
        patterns: Vec<String>,
        metadata: Value,
        always: Vec<String>,
        tool: Option<Value>,
    ) -> (PermissionRequest, oneshot::Receiver<String>) {
        let (tx, rx) = oneshot::channel();
        let req = PermissionRequest {
            id: crate::types::hex_id("per_"),
            session_id: sid.into(),
            permission: permission.into(),
            patterns,
            metadata,
            always,
            time: Time {
                created: crate::types::now_ms(),
                ..Time::default()
            },
            tool,
        };
        self.pending.lock().unwrap().push((req.clone(), tx));
        (req, rx)
    }

    pub fn reject_all(&self, sid: &str) {
        let ids: Vec<String> = self
            .list()
            .into_iter()
            .filter(|r| r.session_id == sid)
            .map(|r| r.id)
            .collect();
        for id in ids {
            self.reply(&id, "reject");
        }
    }

    pub fn reply(&self, id: &str, response: &str) -> bool {
        if !["once", "always", "reject"].contains(&response) {
            return false;
        }
        let mut pending = self.pending.lock().unwrap();
        let Some(index) = pending.iter().position(|(r, _)| r.id == id) else {
            return false;
        };
        let (req, tx) = pending.remove(index);
        drop(pending);
        crate::engine::bus().emit(
            "permission.replied",
            json!({"sessionID":req.session_id,"requestID":req.id,"reply":response}),
        );
        let _ = tx.send(response.into());
        true
    }

    pub fn list(&self) -> Vec<PermissionRequest> {
        self.pending
            .lock()
            .unwrap()
            .iter()
            .map(|(r, _)| r.clone())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn wildcard_matching_and_override_order() {
        assert!(matches("*.env*", "/project/local.env.prod"));
        assert!(matches("git * --?", "git log --x"));
        assert!(!matches("*.env", "foo.env.rs"));
        let rules = vec![
            PermissionRule {
                permission: "*".into(),
                pattern: "*".into(),
                action: "deny".into(),
            },
            PermissionRule {
                permission: "read".into(),
                pattern: "/work/*".into(),
                action: "allow".into(),
            },
        ];
        assert_eq!(decide(&rules, "read", "/work/a"), "allow");
        assert_eq!(decide(&rules, "edit", "/work/a"), "deny");
    }
    #[tokio::test]
    async fn invalid_reply_does_not_consume_request() {
        let state = PermissionState::default();
        let (request, rx) =
            state.ask_for_tool("s", "bash", vec!["echo hi".into()], json!({}), vec![], None);
        assert!(!state.reply(&request.id, "yes"));
        assert_eq!(state.list().len(), 1);
        assert!(state.reply(&request.id, "once"));
        assert_eq!(rx.await.unwrap(), "once");
    }
}
