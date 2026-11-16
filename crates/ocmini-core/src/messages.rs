//! Transcript mutations own their corresponding notifications. Admission,
//! pruning and recovery announce only after their larger transactions commit.
use crate::engine::App;
use crate::events as event;
use crate::types::{MessageInfo, MessageWithParts, OperationError};
use event::{Message, Part};
use serde_json::{json, Value};

pub fn save(app: &App, message: &MessageWithParts) -> Result<(), OperationError> {
    app.store.add_message(message)?;
    announce(app, message);
    Ok(())
}

pub fn update_parts(app: &App, message: &MessageWithParts) -> Result<(), OperationError> {
    app.store.add_message(message)?;
    for part in &message.parts {
        announce_part(app, part);
    }
    Ok(())
}

pub fn update_info(app: &App, info: &MessageInfo) -> Result<(), OperationError> {
    app.store.add_message(&MessageWithParts {
        info: info.clone(),
        parts: vec![],
    })?;
    announce_info(app, info);
    Ok(())
}

pub fn update_part(app: &App, part: &Value) -> Result<(), OperationError> {
    let sid = part["sessionID"]
        .as_str()
        .ok_or_else(|| OperationError::invalid("Message part requires sessionID"))?;
    app.store.update_part(sid, part)?;
    announce_part(app, part);
    Ok(())
}

pub fn announce(app: &App, message: &MessageWithParts) {
    announce_info(app, &message.info);
    for part in &message.parts {
        announce_part(app, part);
    }
}

fn announce_info(app: &App, info: &MessageInfo) {
    let context = app.session_context(&info.session_id, json!({"messageID":info.id}));
    app.hooks.emit(
        Message::Updated {
            session_id: &info.session_id,
            info,
        },
        &context,
    );
}

pub fn announce_part(app: &App, part: &Value) {
    let sid = part["sessionID"].as_str().unwrap_or_default();
    let context = app.session_context(
        sid,
        json!({"messageID":part["messageID"],"partID":part["id"]}),
    );
    app.hooks.emit(
        Part::Updated {
            session_id: sid,
            part,
        },
        &context,
    );
}
