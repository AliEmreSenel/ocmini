//! Backend-independent history policies and transformations.
use crate::types::{now_ms, MessageInfo, MessageWithParts, OperationError, Session, Time, Tokens};
use serde_json::{json, Value};
use std::collections::HashMap;

/// Prepared by core while the backend holds the source snapshot; the backend
/// commits the session, remapped history and copied todos as one operation.
pub struct PreparedFork {
    pub session: Session,
    pub messages: Vec<MessageWithParts>,
}
pub(super) fn prepare_fork(source: &Session, history: Vec<MessageWithParts>) -> PreparedFork {
    let mut s = source.clone();
    s.id = crate::types::hex_id("ses_");
    s.slug = crate::types::slug();
    s.title = format!("{} (fork)", source.title);
    s.time = Time {
        created: now_ms(),
        updated: Some(now_ms()),
        ..Time::default()
    };
    s.parent_id = None;
    s.cost = 0.0;
    s.tokens = Tokens::default();
    s.extra.remove("revert");
    s.extra.remove("share");
    let mut ids = HashMap::new();
    let mut copied = Vec::new();
    for mut m in history {
        let new_id = crate::types::hex_id("msg_");
        ids.insert(m.info.id.clone(), new_id.clone());
        m.info.id = new_id.clone();
        if let Some(control) = m.info.extra.get_mut("ocminiV2") {
            control["id"] = json!(new_id);
        }
        m.info.session_id = s.id.clone();
        m.info.parent_id = m
            .info
            .parent_id
            .as_ref()
            .and_then(|id| ids.get(id).cloned());
        if let Some(tokens) = &m.info.tokens {
            s.tokens.input += tokens.input;
            s.tokens.output += tokens.output;
            s.tokens.reasoning += tokens.reasoning;
            s.tokens.cache.read += tokens.cache.read;
            s.tokens.cache.write += tokens.cache.write;
        }
        s.cost += m.info.cost.unwrap_or(0.0);
        for part in &mut m.parts {
            part["id"] = json!(crate::types::hex_id("prt_"));
            part["messageID"] = json!(new_id);
            part["sessionID"] = json!(s.id);
            if let Some(pending) = part["compactionPending"].as_str() {
                part["compactionPending"] = json!(ids.get(pending));
            }
            // A copied task cannot point at a live child owned by the source thread.
            if part["type"] == "tool" && part["tool"] == "task" {
                if let Some(meta) = part["state"]["metadata"].as_object_mut() {
                    meta.remove("sessionId");
                    meta.remove("sessionID");
                }
            }
        }
        copied.push(m);
    }
    PreparedFork {
        session: s,
        messages: copied,
    }
}

pub(super) fn interrupted_message(raw: &str, now: i64) -> Result<MessageInfo, OperationError> {
    let mut info: MessageInfo = serde_json::from_str(raw)
        .map_err(|e| OperationError::from(format!("Session history: {e}")))?;
    info.time.completed = Some(now.max(info.time.created));
    info.error = Some(
        json!({"name":"MessageAbortedError","data":{"message":"Server stopped before this response completed; send a new prompt to continue."}}),
    );
    Ok(info)
}
pub(super) fn interrupted_part(raw: &str, now: i64) -> Result<Value, OperationError> {
    let mut part: Value = serde_json::from_str(raw)
        .map_err(|e| OperationError::from(format!("Session history: {e}")))?;
    part["state"]["status"] = json!("error");
    part["state"]["error"] =
        json!("Execution interrupted by server restart; the operation was not replayed.");
    part["state"]["time"]["end"] = json!(now);
    Ok(part)
}

pub(super) fn transition_message(sid: &str, mut value: Value) -> (Value, MessageWithParts) {
    let id = crate::types::hex_id("msg_");
    value["id"] = json!(id);
    value["time"] = json!({"created":now_ms()});
    let info = MessageInfo {
        id,
        session_id: sid.into(),
        role: "control".into(),
        time: crate::types::MessageTime {
            created: now_ms(),
            completed: Some(now_ms()),
        },
        extra: serde_json::Map::from_iter([("ocminiV2".into(), value.clone())]),
        ..Default::default()
    };
    (
        value,
        MessageWithParts {
            info,
            parts: vec![],
        },
    )
}
