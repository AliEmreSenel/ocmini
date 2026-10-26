//! State interpretation shared by the two terminal frontends. Painting and
//! output formatting remain with their concrete clients.
use crate::events as event;
use crate::types::{BusEvent, MessageWithParts, PromptRequest};
use std::collections::{HashMap, VecDeque};

/// Enter the saved scope and apply explicit selections over the last user turn.
pub fn activate(
    app: &crate::engine::App,
    session: &crate::types::Session,
    request: &mut PromptRequest,
) -> Result<(crate::engine::App, Vec<MessageWithParts>), crate::types::OperationError> {
    crate::sessions::reject_correlated_reentry(&session.id)?;
    let app = app.for_directory(&session.directory)?;
    let history = app.store.messages(&session.id)?;
    restore_selection(&history, request);
    Ok((app, history))
}

pub fn restore_selection(history: &[MessageWithParts], request: &mut PromptRequest) {
    if let Some(previous) = history
        .iter()
        .rev()
        .find(|message| message.info.role == "user")
    {
        request.model = request.model.take().or_else(|| previous.info.model.clone());
        request.agent = request.agent.take().or_else(|| previous.info.agent.clone());
        request.variant = request
            .variant
            .take()
            .or_else(|| previous.info.variant.clone());
    }
}

pub fn event_session(event: &BusEvent) -> Option<&str> {
    let p = &event.properties;
    p["sessionID"]
        .as_str()
        .or(p["part"]["sessionID"].as_str())
        .or(p["info"]["sessionID"].as_str())
}

pub fn accepts(store: &crate::store::Store, root: &str, event: &BusEvent) -> bool {
    let Some(session) = event_session(event) else {
        return true;
    };
    if session == root {
        return true;
    }
    if !matches!(event.kind, event::EventKind::Permission(_))
        && !matches!(event.kind, event::EventKind::Question(_))
    {
        return false;
    }
    let mut current = Some(session.to_owned());
    let mut seen = std::collections::HashSet::new();
    while let Some(id) = current {
        if id == root {
            return true;
        }
        if !seen.insert(id.clone()) {
            break;
        }
        current = store.scope(&id).and_then(|session| session.parent_id);
    }
    false
}

pub fn pending_events(app: &crate::engine::App, root: &str) -> Vec<BusEvent> {
    let permission = app.perms.list().into_iter().map(|request| {
        BusEvent::new(
            event::Permission::Asked { request: &request },
            request.context.clone(),
        )
    });
    let questions = app.questions.list().into_iter().map(|request| {
        BusEvent::new(
            event::Question::Asked { request: &request },
            request.context.clone(),
        )
    });
    permission
        .chain(questions)
        .filter(|event| accepts(&app.store, root, event))
        .collect()
}

/// Requests remain queued until their authoritative reply/rejection event.
/// Terminal views may keep drafts, but never replace another pending request.
#[derive(Default)]
pub struct Interactions(VecDeque<BusEvent>);
impl Interactions {
    pub fn current(&self) -> Option<&BusEvent> {
        self.0.front()
    }
    pub fn clear(&mut self) {
        self.0.clear();
    }
    pub fn apply(&mut self, event: &BusEvent) -> bool {
        let previous = self.current().map(|e| e.properties["id"].clone());
        match event.kind {
            event::EventKind::Permission(event::PermissionKind::Asked)
            | event::EventKind::Question(event::QuestionKind::Asked) => {
                if event.properties["id"].is_string()
                    && !self
                        .0
                        .iter()
                        .any(|e| e.properties["id"] == event.properties["id"])
                {
                    self.0.push_back(event.clone());
                }
            }
            event::EventKind::Permission(event::PermissionKind::Replied)
            | event::EventKind::Question(event::QuestionKind::Replied)
            | event::EventKind::Question(event::QuestionKind::Rejected) => {
                self.0
                    .retain(|e| e.properties["id"] != event.properties["requestID"]);
            }
            _ => {}
        }
        previous != self.current().map(|e| e.properties["id"].clone())
    }
}

#[derive(Default)]
pub struct TextState {
    roles: HashMap<String, String>,
    parts: HashMap<String, (TextKind, String)>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextKind {
    Text,
    Reasoning,
}

pub struct TextUpdate {
    pub kind: TextKind,
    pub text: String,
    pub new: bool,
    pub reset: bool,
}
impl TextState {
    pub fn remember_role(&mut self, id: &str, role: &str) {
        self.roles.insert(id.into(), role.into());
    }
    pub fn part(&mut self, part: &serde_json::Value) -> Option<TextUpdate> {
        if self
            .roles
            .get(part["messageID"].as_str().unwrap_or_default())
            .is_some_and(|role| role != "assistant")
        {
            return None;
        }
        let kind = match part["type"].as_str()? {
            "text" => TextKind::Text,
            "reasoning" => TextKind::Reasoning,
            _ => return None,
        };
        let id = part["id"].as_str()?.to_owned();
        let full = part["text"].as_str().unwrap_or_default();
        let previous = self.parts.insert(id, (kind, full.into()));
        let reset = previous
            .as_ref()
            .is_some_and(|(_, text)| !full.starts_with(text));
        let text = previous
            .as_ref()
            .and_then(|(_, text)| full.strip_prefix(text))
            .unwrap_or(full)
            .into();
        Some(TextUpdate {
            kind,
            text,
            new: previous.is_none(),
            reset,
        })
    }

    pub fn apply(&mut self, event: &BusEvent) -> Option<TextUpdate> {
        let p = &event.properties;
        match event.kind {
            event::EventKind::Message(event::MessageKind::Updated) => {
                self.remember_role(p["info"]["id"].as_str()?, p["info"]["role"].as_str()?);
                None
            }
            event::EventKind::Part(event::PartKind::Updated) => self.part(&p["part"]),
            event::EventKind::Part(event::PartKind::Delta) => {
                let (kind, text) = self.parts.get_mut(p["partID"].as_str()?)?;
                let delta = p["delta"].as_str().unwrap_or_default();
                text.push_str(delta);
                Some(TextUpdate {
                    kind: *kind,
                    text: delta.into(),
                    new: false,
                    reset: false,
                })
            }
            _ => None,
        }
    }
}

pub fn parse_answer(line: &str, labels: &[String]) -> Vec<String> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }
    let numbers: Option<Vec<usize>> = trimmed
        .split_ascii_whitespace()
        .map(|token| {
            token
                .parse::<usize>()
                .ok()
                .filter(|number| (1..=labels.len()).contains(number))
        })
        .collect();
    match numbers {
        Some(numbers) => numbers
            .iter()
            .map(|number| labels[number - 1].clone())
            .collect(),
        None => vec![trimmed.to_string()],
    }
}

#[cfg(unix)]
pub fn shutdown_signal() -> std::io::Result<impl std::future::Future<Output = ()>> {
    use tokio::signal::unix::{signal, SignalKind};
    let mut hangup = signal(SignalKind::hangup())?;
    let mut interrupt = signal(SignalKind::interrupt())?;
    let mut quit = signal(SignalKind::quit())?;
    let mut terminate = signal(SignalKind::terminate())?;
    Ok(async move {
        tokio::select! {
            _ = hangup.recv() => {},
            _ = interrupt.recv() => {},
            _ = quit.recv() => {},
            _ = terminate.recv() => {},
        }
    })
}

#[cfg(not(unix))]
pub fn shutdown_signal() -> std::io::Result<impl std::future::Future<Output = ()>> {
    Ok(async {
        let _ = tokio::signal::ctrl_c().await;
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn event(kind: &str, properties: serde_json::Value) -> BusEvent {
        BusEvent {
            id: "event".into(),
            kind: serde_json::from_value(serde_json::json!(kind)).expect("known event"),
            properties,
        }
    }
    #[test]
    fn text_reduction_handles_roles_deltas_full_updates_and_resets() {
        let mut state = TextState::default();
        state.apply(&event(
            "message.updated",
            json!({"info":{"id":"user","role":"user"}}),
        ));
        assert!(state
            .apply(&event(
                "message.part.updated",
                json!({"part":{"id":"u","messageID":"user","type":"text","text":"prompt"}})
            ))
            .is_none());
        let initial = event(
            "message.part.updated",
            json!({"part":{"id":"p","messageID":"assistant","type":"text","text":""}}),
        );
        assert!(state.apply(&initial).unwrap().new);
        assert_eq!(
            state
                .apply(&event(
                    "message.part.delta",
                    json!({"partID":"p","delta":"partial"})
                ))
                .unwrap()
                .text,
            "partial"
        );
        assert!(state
            .apply(&event(
                "message.part.updated",
                json!({"part":{"id":"p","type":"text","text":"partial"}})
            ))
            .unwrap()
            .text
            .is_empty());
        assert!(state.apply(&initial).unwrap().reset);
        assert_eq!(
            state
                .apply(&event(
                    "message.part.updated",
                    json!({"part":{"id":"p","type":"text","text":"accepted"}})
                ))
                .unwrap()
                .text,
            "accepted"
        );
    }
    #[test]
    fn interactions_queue_descendants_and_settle_only_the_matching_request() {
        let mut pending = Interactions::default();
        let permission = event("permission.asked", json!({"id":"p","sessionID":"root"}));
        let question = event(
            "question.asked",
            json!({"id":"q","sessionID":"child","questions":[]}),
        );
        assert!(pending.apply(&permission));
        assert!(!pending.apply(&question));
        assert!(!pending.apply(&permission));
        assert!(!pending.apply(&event("question.replied", json!({"requestID":"unrelated"}))));
        assert_eq!(pending.current().unwrap().properties["id"], "p");
        assert!(pending.apply(&event("permission.replied", json!({"requestID":"p"}))));
        assert_eq!(pending.current().unwrap().properties["id"], "q");
        assert!(pending.apply(&event("question.rejected", json!({"requestID":"q"}))));
        assert!(pending.current().is_none());
    }

    #[test]
    fn history_seeds_live_text_without_repeating_saved_content_or_headings() {
        let mut state = TextState::default();
        state.remember_role("user", "user");
        assert!(state
            .part(&json!({"id":"u","messageID":"user","type":"text","text":"prompt"}))
            .is_none());
        state.remember_role("assistant", "assistant");
        let part = json!({"id":"a","messageID":"assistant","type":"reasoning","text":"saved"});
        assert!(state.part(&part).unwrap().new);
        let update = state
            .apply(&event(
                "message.part.delta",
                json!({"partID":"a","delta":" live"}),
            ))
            .unwrap();
        assert_eq!(update.kind, TextKind::Reasoning);
        assert_eq!(update.text, " live");
        assert!(!update.new);
        let final_part =
            json!({"id":"a","messageID":"assistant","type":"reasoning","text":"saved live"});
        assert!(state.part(&final_part).unwrap().text.is_empty());
    }
    #[tokio::test]
    async fn interaction_recovery_filters_unrelated_sessions_and_clears_settled_requests() {
        let store =
            crate::store::Store::open(std::path::Path::new(":memory:"), "client-test").unwrap();
        let root = store.create_session_owned("root", "/tmp");
        let mut child = crate::types::Session::new("child", "/tmp");
        child.parent_id = Some(root.id.clone());
        store.create_session(&child).unwrap();
        let other = store.create_session_owned("other", "/tmp");
        let questions = crate::engine::question::QuestionState::default();
        let (request, receiver) = questions.ask_for_tool(
            &child.id,
            json!({"question":"Continue?","multiple":false}),
            None,
            json!({"sessionID":child.id}),
        );
        let mut recovered = Interactions::default();
        for request in questions.list() {
            let asked = event("question.asked", serde_json::to_value(request).unwrap());
            if accepts(&store, &root.id, &asked) {
                recovered.apply(&asked);
            }
        }
        assert_eq!(recovered.current().unwrap().properties["id"], request.id);
        assert!(!accepts(&store, &other.id, recovered.current().unwrap()));
        assert!(!accepts(
            &store,
            &root.id,
            &event("message.updated", json!({"info":{"sessionID":child.id}}))
        ));
        assert!(!questions.reply(&request.id, vec![vec!["a".into(), "b".into()]]));
        assert_eq!(questions.list().len(), 1);
        assert!(questions.reject(&request.id));
        assert!(receiver.await.unwrap().is_none());
        recovered.clear();
        for request in questions.list() {
            recovered.apply(&event(
                "question.asked",
                serde_json::to_value(request).unwrap(),
            ));
        }
        assert!(recovered.current().is_none());
    }
}
