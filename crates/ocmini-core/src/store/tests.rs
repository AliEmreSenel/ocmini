use super::{backend::RecordingBackend, sqlite::SqliteBackend, Store};
use crate::types::{MessageInfo, MessageTime, MessageWithParts, PromptRequest, Session};
use serde_json::json;
use std::{
    path::Path,
    sync::{Arc, Mutex},
};

fn message(session: &Session, id: &str, role: &str) -> MessageWithParts {
    MessageWithParts {
        info: MessageInfo {
            id: id.into(),
            session_id: session.id.clone(),
            role: role.into(),
            time: MessageTime {
                created: 123,
                completed: None,
            },
            ..Default::default()
        },
        parts: vec![
            json!({"id":format!("part_{id}"),"messageID":id,"sessionID":session.id,"type":"text","text":id}),
        ],
    }
}

#[test]
fn compiled_backend_owns_admission_paging_forks_and_recovery() {
    let root = crate::test_tempdir().unwrap();
    let path = root.path().join("history.db");
    let calls = Arc::new(Mutex::new(Vec::new()));
    let store = Store::from_backend(RecordingBackend {
        inner: SqliteBackend::open(&path, "before-restart").unwrap(),
        calls: calls.clone(),
    });
    let session = Session::new("injected", "/work");
    store.create_session(&session).unwrap();
    store.v2_update(&session.id, &json!({"test":true})).unwrap();
    let user = message(&session, "msg_user", "user");
    let request = PromptRequest {
        message_id: Some(user.info.id.clone()),
        ..Default::default()
    };
    assert!(store.admit(&request, &user).unwrap());
    assert!(!store.admit(&request, &user).unwrap());
    // Pending admissions stay outside v2 history until execution starts.
    assert!(store
        .v2_page(&session.id, None, false, 1)
        .unwrap()
        .is_empty());
    assert_eq!(store.v2_inbox(&session.id).unwrap().len(), 1);
    assert_eq!(
        store.next_prompt(&session.id).unwrap().unwrap().message_id,
        request.message_id
    );
    let mut assistant = message(&session, "msg_assistant", "assistant");
    assistant.info.parent_id = Some(user.info.id.clone());
    store.add_message(&assistant).unwrap();
    let first = store.v2_page(&session.id, None, false, 1).unwrap();
    assert_eq!(first[0].1.info.id, user.info.id);
    let next = store
        .v2_page(&session.id, Some(first[0].0), false, 1)
        .unwrap();
    assert_eq!(next[0].1.info.id, assistant.info.id);
    let fork = store.try_fork(&session, Some(&assistant.info.id)).unwrap();
    let forked = store.messages(&fork.id).unwrap();
    assert_eq!(forked.len(), 1);
    assert_ne!(forked[0].info.id, user.info.id);
    assert_eq!(forked[0].parts[0]["sessionID"], fork.id);
    assert_eq!(
        store
            .context_messages(&session.id, &user.info.id)
            .unwrap()
            .len(),
        2
    );
    drop(store);

    let recovered = Store::from_backend(RecordingBackend {
        inner: SqliteBackend::open(&path, "after-restart").unwrap(),
        calls: calls.clone(),
    });
    crate::hooks::recover(recovered.backend()).unwrap();
    assert!(recovered
        .message(&session.id, &assistant.info.id)
        .unwrap()
        .unwrap()
        .info
        .time
        .completed
        .is_some());
    assert!(!recovered.is_pending_message(&user.info.id).unwrap());
    assert!(recovered.v2_state(&session.id).unwrap()["test"]
        .as_bool()
        .unwrap());
    let calls = calls.lock().unwrap();
    for operation in [
        "admit",
        "v2_page",
        "v2_inbox",
        "next_prompt",
        "add_message",
        "fork_history",
        "context_messages",
        "interrupted_turns",
        "apply_recovery",
    ] {
        assert!(
            calls.contains(&operation),
            "{operation} bypassed the injected backend"
        );
    }
}

#[test]
fn session_callbacks_and_filters_use_the_compiled_backend() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let store = Store::from_backend(RecordingBackend {
        inner: SqliteBackend::open(Path::new(":memory:"), "callbacks").unwrap(),
        calls: calls.clone(),
    });
    let session = Session::new("before", "/work");
    store.create_session(&session).unwrap();
    let title = String::from("after");
    let updated = store
        .mutate_session(&session.id, move |session| {
            session.title = title;
            Ok(true)
        })
        .unwrap()
        .unwrap();
    assert_eq!(updated.title, "after");
    let mut visited = 0;
    let selected = store
        .list_sessions_filtered(None, false, None, |session| {
            visited += 1;
            session.title == "after"
        })
        .unwrap();
    assert_eq!(visited, 1);
    assert_eq!(selected[0].id, updated.id);
    assert!(calls.lock().unwrap().contains(&"mutate_session"));
    assert!(calls.lock().unwrap().contains(&"list_sessions_filtered"));
}
