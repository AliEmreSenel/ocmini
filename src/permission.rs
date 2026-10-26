use crate::events as event;
use crate::hooks::PermissionHook;
use crate::hooks::{Hook, HookName, Shape};
pub(crate) const PERMISSION_DECIDE: Hook = Hook::new(
    HookName::Permission(PermissionHook::Decide),
    &[("decision", Shape::Choice(&["allow", "ask", "deny"]))],
)
.validate(validate_decision);
pub(crate) const HOOKS: &[Hook] = &[PERMISSION_DECIDE];
use crate::types::{
    PermissionAction::{Allow, Ask},
    PermissionRequest, PermissionRule, Time,
};
#[cfg(test)]
use serde_json::json;
use serde_json::Value;
use std::sync::{Arc, Mutex};
use tokio::sync::oneshot;

type Pending = (PermissionRequest, oneshot::Sender<String>);

pub fn parse_rules(value: &Value) -> Result<Vec<PermissionRule>, String> {
    let rules: Vec<PermissionRule> = serde_json::from_value(value.clone())
        .map_err(|error| format!("Invalid permission rules: {error}"))?;
    if rules.iter().any(|rule| rule.permission.is_empty()) {
        return Err(
            "Invalid permission rules: a name and allow, ask, or deny action are required".into(),
        );
    }
    Ok(rules)
}

#[derive(Clone, Default)]
pub struct PermissionState {
    pending: Arc<Mutex<Vec<Pending>>>,
}

pub fn matches(pattern: &str, value: &str) -> bool {
    let (mut p, mut v, mut star, mut retry) = (0, 0, None, 0);
    while v < value.len() {
        let next = value[v..].chars().next().unwrap();
        match pattern[p..].chars().next() {
            Some('*') => {
                star = Some(p);
                p += 1;
                retry = v;
            }
            Some(c) if c == '?' || c == next => {
                p += c.len_utf8();
                v += next.len_utf8();
            }
            _ => {
                if let Some(s) = star {
                    p = s + 1;
                    retry += value[retry..].chars().next().unwrap().len_utf8();
                    v = retry;
                } else {
                    return false;
                }
            }
        }
    }
    pattern[p..].chars().all(|c| c == '*')
}

pub fn default_rules() -> Vec<PermissionRule> {
    [
        ("*", "*", Allow),
        ("external_directory", "*", Ask),
        ("doom_loop", "*", Ask),
        ("read", "*.env", Ask),
        ("read", "*.env.*", Ask),
    ]
    .into_iter()
    .map(|(permission, pattern, action)| PermissionRule {
        permission: permission.into(),
        pattern: pattern.into(),
        action,
    })
    .collect()
}

/// OpenCode uses the last matching rule: session overrides and 'always' replies
/// must be able to override an earlier broad default.
pub fn decide(rules: &[PermissionRule], name: &str, value: &str) -> crate::types::PermissionAction {
    rules
        .iter()
        .rev()
        .find(|r| matches(&r.permission, name) && matches(&r.pattern, value))
        .map_or(crate::types::PermissionAction::Allow, |r| r.action)
}

impl PermissionState {
    pub fn ask_for_tool(
        &self,
        context: Value,
        permission: &str,
        pattern: &str,
        input: &Value,
    ) -> (PermissionRequest, oneshot::Receiver<String>) {
        let (tx, rx) = oneshot::channel();
        let req = PermissionRequest {
            id: crate::types::hex_id("per_"),
            session_id: context["sessionID"].as_str().unwrap_or_default().into(),
            permission: permission.into(),
            patterns: vec![pattern.into()],
            always: vec![pattern.into()],
            metadata: serde_json::json!({"tool":context["tool"],"input":input}),
            time: Time {
                created: crate::types::now_ms(),
                ..Time::default()
            },
            tool: Some(
                serde_json::json!({"messageID":context["messageID"],"callID":context["callID"]}),
            ),
            context,
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
        let Some(reply) = event::PermissionReply::parse(response) else {
            return false;
        };
        let mut pending = self.pending.lock().unwrap();
        let Some(index) = pending.iter().position(|(r, _)| r.id == id) else {
            return false;
        };
        let (req, tx) = pending.remove(index);
        drop(pending);
        crate::engine::bus().emit(
            event::Permission::Replied {
                session_id: &req.session_id,
                request_id: &req.id,
                reply,
            },
            req.context,
        );
        let _ = tx.send(reply.as_str().into());
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

fn validate_decision(input: &Value, set: &serde_json::Map<String, Value>) -> Result<(), String> {
    if let Some(value) = set.get("decision") {
        let before: crate::types::PermissionAction =
            serde_json::from_value(input["decision"].clone()).map_err(|e| e.to_string())?;
        let after: crate::types::PermissionAction =
            serde_json::from_value(value.clone()).map_err(|e| e.to_string())?;
        if after < before {
            return Err("permission hooks can only tighten decisions".into());
        }
    }
    Ok(())
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
                action: crate::types::PermissionAction::Deny,
            },
            PermissionRule {
                permission: "read".into(),
                pattern: "/work/*".into(),
                action: crate::types::PermissionAction::Allow,
            },
        ];
        assert_eq!(
            decide(&rules, "read", "/work/a"),
            crate::types::PermissionAction::Allow
        );
        assert_eq!(
            decide(&rules, "edit", "/work/a"),
            crate::types::PermissionAction::Deny
        );
    }
    #[tokio::test]
    async fn invalid_reply_does_not_consume_request() {
        let state = PermissionState::default();
        let (request, rx) = state.ask_for_tool(
            json!({"sessionID":"s","tool":"bash"}),
            "bash",
            "echo hi",
            &json!({}),
        );
        assert!(!state.reply(&request.id, "yes"));
        assert_eq!(state.list().len(), 1);
        assert!(state.reply(&request.id, "once"));
        assert_eq!(rx.await.unwrap(), "once");
    }
}
