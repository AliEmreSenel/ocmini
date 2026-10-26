//! Recovery prepares terminal facts and subscription routing before Store
//! enters its transaction. The transaction rechecks the observed ownership.
use crate::events as event;
use crate::store::{InterruptedTurn, RecoveredTurn, Store};
use crate::types::now_ms;
use serde_json::{json, Value};

pub fn recover(store: &Store) -> Result<(), String> {
    let plans = store
        .interrupted_turns()?
        .into_iter()
        .map(prepare)
        .collect::<Result<Vec<_>, _>>()?;
    let (events, failures) = store.recover_interrupted(&plans)?;
    for event in events {
        crate::engine::bus().publish(event);
    }
    for error in failures {
        crate::engine::bus().emit(
            event::Hook::Overflow {
                data: event::Overflow::Durable {
                    diagnostic: &(error.chars().take(512).collect::<String>()),
                },
            },
            json!({"delivery":"durable"}),
        );
    }
    Ok(())
}
fn prepare(source: InterruptedTurn) -> Result<RecoveredTurn, String> {
    let InterruptedTurn {
        message_id: mid,
        session_id: sid,
        state,
        reason,
        started,
        session,
        message,
        prepared,
        snapshot,
        request,
    } = &source;
    let now = now_ms();
    let mut deliveries = Vec::new();
    let mut events: Vec<crate::types::BusEvent> = if let Some(raw) = prepared {
        serde_json::from_str(raw).map_err(|e| e.to_string())?
    } else {
        let session: Value = serde_json::from_str(session).map_err(|e| e.to_string())?;
        let message: Value = message
            .as_deref()
            .map(serde_json::from_str)
            .transpose()
            .map_err(|e| e.to_string())?
            .unwrap_or(Value::Null);
        let reason = if state == "running" {
            "error"
        } else {
            reason.as_deref().unwrap_or("error")
        };
        let request: Value = request
            .as_deref()
            .map(serde_json::from_str)
            .transpose()
            .map_err(|e| e.to_string())?
            .unwrap_or(Value::Null);
        let model = message.get("model").or_else(|| request.get("model"));
        let agent = message.get("agent").or_else(|| request.get("agent"));
        let command = request["command"].as_str().or(message["command"].as_str());
        let kind = if command == Some("__ocmini_compact") {
            "compaction"
        } else {
            "primary"
        };
        vec![crate::types::BusEvent {
            id: format!("evt_{mid}_ocmini_turn_ended"),
            kind: event::EventKind::Turn(event::TurnKind::Ended),
            properties: json!({"version":1,"time":now,"directory":session["directory"],"sessionID":sid,"turnID":mid,"turnKind":kind,"agent":agent,"model":model,"parentID":session["parentID"],"reason":reason,"outcome":reason,"started":started,"responseID":null,"error":"Execution interrupted by server restart; no effects were replayed."}),
        }]
    };
    for event in &mut events {
        if event.kind == event::EventKind::Turn(event::TurnKind::Ended) {
            event.properties["finalizerError"] = json!("interrupted");
            event.properties["recovered"] = json!(true);
        }
        deliveries.extend(super::events::prepare_recovered(
            event,
            snapshot.as_deref(),
        )?);
    }

    Ok(RecoveredTurn {
        source,
        events: events.into_iter().map(std::sync::Arc::new).collect(),
        deliveries,
    })
}
