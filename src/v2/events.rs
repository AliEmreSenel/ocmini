use super::wire;
use crate::{store::Store, types::*};
use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, OnceLock, Weak,
    },
};
use tokio::sync::broadcast;

pub struct EncodedEvent {
    pub kind: String,
    pub json: String,
}
impl EncodedEvent {
    fn encode(mut value: Value) -> Arc<Self> {
        let json = value.to_string();
        let Value::String(kind) = value["type"].take() else {
            unreachable!("event type")
        };
        Arc::new(Self { kind, json })
    }
}
pub type Frame = Result<Arc<EncodedEvent>, Arc<str>>;
#[derive(Default)]
struct State {
    active: HashSet<String>,
    failures: HashMap<String, Value>,
    parts: HashMap<String, (usize, String, String)>,
    steps: HashSet<String>,
    ordinals: HashMap<String, usize>,
    compactions: HashMap<String, &'static str>,
}
#[derive(Default)]
struct Sequence {
    next: i64,
    end: i64,
}
#[derive(Default)]
struct SessionEvents {
    state: Mutex<State>,
    sequence: Mutex<Sequence>,
    active: AtomicBool,
}
pub struct Events {
    store: Weak<Store>,
    // Only executions in progress retain indexes. Idle API operations release theirs.
    sessions: Mutex<HashMap<String, Arc<SessionEvents>>>,
    tx: OnceLock<broadcast::Sender<Frame>>,
}
impl Events {
    pub fn new(store: &Arc<Store>) -> Result<Arc<Self>, String> {
        Ok(Arc::new(Self {
            store: Arc::downgrade(store),
            sessions: Mutex::new(HashMap::new()),
            tx: OnceLock::new(),
        }))
    }
    pub fn subscribe(&self) -> broadcast::Receiver<Frame> {
        self.tx
            .get_or_init(|| broadcast::channel(16384).0)
            .subscribe()
    }
    fn listening(&self) -> bool {
        self.tx.get().is_some_and(|tx| tx.receiver_count() > 0)
    }
    fn publish(&self, frame: Frame) {
        if let Some(tx) = self.tx.get() {
            let _ = tx.send(frame);
        }
    }
    fn session(&self, sid: &str) -> Arc<SessionEvents> {
        self.sessions
            .lock()
            .unwrap()
            .entry(sid.into())
            .or_default()
            .clone()
    }
    fn release(&self, sid: &str, session: &Arc<SessionEvents>) {
        let mut sessions = self.sessions.lock().unwrap();
        if !session.active.load(Ordering::Acquire) && Arc::strong_count(session) == 2 {
            sessions.remove(sid);
        }
    }
    pub fn enabled(&self, sid: &str) -> bool {
        if self.sessions.lock().unwrap().contains_key(sid) {
            return true;
        }
        self.store
            .upgrade()
            .is_some_and(|store| store.v2_enabled(sid).unwrap_or(false))
    }
    pub fn enable(&self, sid: &str) -> Result<(), OperationError> {
        let store = self.store.upgrade().ok_or("Runtime stopped")?;
        if !store.v2_enabled(sid)? {
            store.v2_update(sid, &json!({}))?;
        }
        Ok(())
    }
    pub fn prepare_delete(&self, session: &Session) -> Result<Option<Frame>, OperationError> {
        if !self.listening() || !self.enabled(&session.id) {
            return Ok(None);
        }
        let store = self.store.upgrade().ok_or("Runtime stopped")?;
        let (_, seq) = store.v2_sequence(&session.id, 0, 0, 1)?;
        Ok(Some(Ok(EncodedEvent::encode(
            json!({"id":hex_id("evt_"),"created":now_ms(),"type":"session.deleted","data":{"sessionID":session.id},"location":{"directory":session.directory},"durable":{"aggregateID":session.id,"seq":seq,"version":2}}),
        ))))
    }
    pub fn deleted(&self, sid: &str, frame: Frame) {
        self.sessions.lock().unwrap().remove(sid);
        self.publish(frame);
    }
    pub fn emit(&self, sid: &str, kind: &str, data: Value, durable: bool) {
        if !self.listening() {
            return;
        }
        let session = self.session(sid);
        // Ordering is per session. No database I/O holds the session-map lock.
        {
            let _guard = session.state.lock().unwrap();
            self.send(&session, sid, kind, data, durable);
        }
        self.release(sid, &session);
    }
    fn send(&self, session: &SessionEvents, sid: &str, kind: &str, data: Value, durable: bool) {
        if !self.listening() {
            return;
        }
        let Some(store) = self.store.upgrade() else {
            return;
        };
        let mut value = json!({"id":hex_id("evt_"),"created":now_ms(),"type":kind,"data":data});
        if let Some(directory) = store.directory_of(sid) {
            value["location"] = json!({"directory":directory});
        }
        if durable {
            let mut sequence = session.sequence.lock().unwrap();
            // This reserves IDs, not resources: exhausting a range obtains another;
            // it never limits events, output, queued work or concurrency. A sibling
            // writer invalidates the range through the persisted high-water mark.
            let reserve = if session.active.load(Ordering::Acquire) {
                4096
            } else {
                1
            };
            match store.v2_sequence(sid, sequence.next, sequence.end, reserve) {
                Ok((next, end)) => {
                    sequence.next = next + 1;
                    sequence.end = end;
                    value["durable"] = json!({"aggregateID":sid,"seq":next,"version":if matches!(kind,"session.tool.success"|"session.tool.failed"|"session.forked"){2}else{1}});
                }
                Err(error) => {
                    self.publish(Err(Arc::from(error.message)));
                    return;
                }
            }
        }
        self.publish(Ok(EncodedEvent::encode(value)));
    }
    pub fn observe(&self, event: &BusEvent) {
        let kind = event.kind.as_str();
        if !matches!(
            kind,
            "session.created"
                | "ocmini.prompt.accepted"
                | "ocmini.turn.started"
                | "session.status"
                | "session.error"
                | "session.idle"
                | "message.updated"
                | "message.part.updated"
                | "message.part.delta"
                | "permission.asked"
                | "permission.replied"
                | "question.asked"
                | "question.replied"
                | "question.rejected"
                | "ocmini.compaction.started"
                | "ocmini.compaction.failed"
        ) {
            return;
        }
        let listening = self.listening();
        if !listening
            && !matches!(
                kind,
                "session.created"
                    | "session.status"
                    | "session.error"
                    | "session.idle"
                    | "message.updated"
                    | "message.part.updated"
                    | "ocmini.compaction.started"
                    | "ocmini.compaction.failed"
            )
        {
            return;
        }
        let Some(store) = self.store.upgrade() else {
            return;
        };
        let p = &event.properties;
        let kind = event.kind.as_str();
        let sid = p["sessionID"]
            .as_str()
            .or_else(|| p["info"]["id"].as_str())
            .unwrap_or_default();
        if sid.is_empty() || kind == "session.updated" {
            return;
        }
        if kind == "session.created"
            && p["info"]["parentID"]
                .as_str()
                .is_some_and(|parent| self.enabled(parent))
        {
            if let Err(error) = self.enable(sid) {
                self.publish(Err(Arc::from(error.message)));
                return;
            }
        }
        if !listening && kind == "session.created" {
            return;
        }
        let existing = self.sessions.lock().unwrap().get(sid).cloned();
        let session = match existing {
            Some(session) => session,
            None => {
                // Transcript streaming belongs to an execution already registered
                // by busy. Do not query the database for every legacy/user delta.
                if matches!(
                    kind,
                    "message.part.delta" | "message.part.updated" | "message.updated"
                ) || !self.enabled(sid)
                {
                    return;
                }
                self.session(sid)
            }
        };
        {
            let mut state = session.state.lock().unwrap();
            if kind == "message.part.updated" {
                let mid = p["part"]["messageID"].as_str().unwrap_or_default();
                if !state.steps.contains(&format!("{sid}/{mid}"))
                    && !state.compactions.contains_key(mid)
                {
                    return;
                }
            }
            let result = self.translate(&store, &session, &mut state, sid, kind, p);
            session
                .active
                .store(state.active.contains(sid), Ordering::Release);
            if let Err(error) = result {
                self.publish(Err(Arc::from(error.message)));
            }
        }
        self.release(sid, &session);
    }
    fn translate(
        &self,
        store: &Store,
        session: &SessionEvents,
        state: &mut State,
        sid: &str,
        kind: &str,
        p: &Value,
    ) -> Result<(), OperationError> {
        match kind {
            "session.created" => {
                let info: &Value = &p["info"];
                let mut data = json!({"sessionID":sid,"projectID":info["projectID"],"location":{"directory":info["directory"]},"slug":info["slug"],"version":crate::OPENCODE_COMPAT_VERSION,"title":info["title"]});
                if let Some(parent) = info.get("parentID") {
                    data["parentID"] = parent.clone();
                }
                if let Some(permission) = info.get("permission") {
                    let rules: Vec<PermissionRule> = serde_json::from_value(permission.clone())
                        .map_err(|e| OperationError::from(e.to_string()))?;
                    data["permissions"] = wire::rules_out(&rules);
                }
                self.send(session, sid, "session.created", data, true);
            }
            "ocmini.prompt.accepted" => {
                if let Some(message) =
                    store.message(sid, p["messageID"].as_str().unwrap_or_default())?
                {
                    let input = wire::receipt(sid, &message)?;
                    self.send(session, sid,"session.inbox.enqueued",json!({"sessionID":sid,"inboxID":message.info.id,"item":{"type":input["type"],"delivery":input["delivery"],"payload":input["payload"]}}),true);
                }
            }
            "ocmini.turn.started" => {
                self.send(
                    session,
                    sid,
                    "session.inbox.delivered",
                    json!({"sessionID":sid,"inboxID":p["messageID"]}),
                    true,
                );
            }
            "session.status" => {
                if p["status"]["type"] == "busy" && state.active.insert(sid.into()) {
                    store.v2_update(sid, &json!({"busy":now_ms()}))?;
                    state.failures.remove(sid);
                    self.send(
                        session,
                        sid,
                        "session.execution.started",
                        json!({"sessionID":sid}),
                        true,
                    );
                } else if p["status"]["type"] == "retry" {
                    let amid = p["messageID"]
                        .as_str()
                        .map(str::to_owned)
                        .or_else(|| {
                            store
                                .messages_limited(sid, Some(1))
                                .ok()
                                .and_then(|m| m.last().map(|m| m.info.id.clone()))
                        })
                        .unwrap_or_default();
                    self.send(session, sid,"session.retry.scheduled",json!({"sessionID":sid,"assistantMessageID":amid,"attempt":p["status"]["attempt"],"at":p["status"]["next"],"error":wire::error(p["status"]["message"].as_str().unwrap_or("Retrying"))}),true);
                }
            }
            "session.error" => {
                state.failures.insert(sid.into(), p["error"].clone());
            }
            "session.idle" => {
                if state.active.remove(sid) {
                    let error = state.failures.remove(sid);
                    let outcome = if error
                        .as_ref()
                        .is_some_and(|e| e["name"] == "MessageAbortedError")
                    {
                        "interrupted"
                    } else if error.is_some() {
                        "failed"
                    } else {
                        "succeeded"
                    };
                    let now = now_ms();
                    store.v2_transition(
                        sid,
                        &json!({"outcome":outcome,"idle":now}),
                        json!({"type":"idle","outcome":outcome}),
                    )?;
                    let mut data = json!({"sessionID":sid});
                    if outcome == "interrupted" {
                        data["reason"] = json!("user");
                    }
                    if let Some(error) = error {
                        data["error"] = wire::error(
                            error["data"]["message"]
                                .as_str()
                                .unwrap_or("Execution failed"),
                        );
                    }
                    self.send(
                        session,
                        sid,
                        &format!("session.execution.{outcome}"),
                        data,
                        true,
                    );
                    // Only retain live part/step indexes; persisted history is authoritative.
                    state.parts.retain(|_, (_, _, owner)| owner != sid);
                    state
                        .steps
                        .retain(|key| !key.starts_with(&format!("{sid}/")));
                    state
                        .ordinals
                        .retain(|key, _| !key.starts_with(&format!("{sid}/")));
                }
            }
            "message.updated" => {
                let info: &Value = &p["info"];
                if info["role"] != "assistant" {
                    return Ok(());
                }
                let mid = info["id"].as_str().unwrap_or_default();
                let key = format!("{sid}/{mid}");
                if info["agent"] == "compaction" && info["summary"] != true {
                    if let Some(reason) = state
                        .compactions
                        .get(mid)
                        .copied()
                        .filter(|_| info["error"].is_object())
                    {
                        state.compactions.remove(mid);
                        self.send(session, sid,"session.compaction.failed",json!({"sessionID":sid,"reason":reason,"error":wire::error(info["error"]["data"]["message"].as_str().unwrap_or("Compaction failed"))}),true);
                    }
                    return Ok(());
                }
                if info["summary"] == true {
                    let reason = state.compactions.remove(mid).unwrap_or("auto");
                    if !self.listening() {
                        return Ok(());
                    }
                    let m = store
                        .message(sid, mid)?
                        .ok_or("Compaction message missing")?;
                    self.send(session, sid,"session.compaction.ended",json!({"sessionID":sid,"reason":reason,"text":text_parts(&m.parts).collect::<Vec<_>>().join("\n"),"model":{"providerID":info["providerID"],"id":info["modelID"]},"cost":info["cost"].as_f64().unwrap_or(0.0),"tokens":info["tokens"],"metadata":{}}),true);
                } else if state.steps.insert(key) {
                    let mut model = json!({"id":info["modelID"],"providerID":info["providerID"]});
                    if let Some(variant) = info.get("variant") {
                        model["variant"] = variant.clone();
                    }
                    self.send(session, sid,"session.step.started",json!({"sessionID":sid,"assistantMessageID":mid,"agent":info["agent"].as_str().unwrap_or("build"),"model":model,"started":info["time"]["created"]}),true);
                } else if info["time"]["completed"].is_number() {
                    let mut data = json!({"sessionID":sid,"assistantMessageID":mid,"finish":info["finish"].as_str().unwrap_or("stop"),"cost":info["cost"].as_f64().unwrap_or(0.0),"tokens":info["tokens"].as_object().map(|_|info["tokens"].clone()).unwrap_or_else(||json!(Tokens::default()))});
                    let failed = info.get("error").is_some_and(|e| !e.is_null());
                    if failed {
                        if info["tokens"].is_null() {
                            data.as_object_mut().unwrap().remove("tokens");
                        }
                        if info["cost"].is_null() {
                            data.as_object_mut().unwrap().remove("cost");
                        }
                        if info["finish"] != "content-filter" {
                            data.as_object_mut().unwrap().remove("finish");
                        }
                        data["error"] = wire::error(
                            info["error"]["data"]["message"]
                                .as_str()
                                .unwrap_or("Step failed"),
                        );
                    }
                    self.send(
                        session,
                        sid,
                        if failed {
                            "session.step.failed"
                        } else {
                            "session.step.ended"
                        },
                        data,
                        true,
                    );
                }
            }
            "message.part.updated" => {
                let part = &p["part"];
                let id = part["id"].as_str().unwrap_or_default();
                let mid = part["messageID"].as_str().unwrap_or_default();
                let name = part["type"].as_str().unwrap_or_default();
                if name == "step-finish" && !state.compactions.contains_key(mid) {
                    self.send(
                        session,
                        sid,
                        "session.step.streamed",
                        json!({"sessionID":sid,"assistantMessageID":mid}),
                        true,
                    );
                }
                if state.compactions.contains_key(mid)
                    || !matches!(name, "text" | "reasoning" | "tool")
                {
                    return Ok(());
                }
                let new = !state.parts.contains_key(id);
                if new {
                    let ordinal = state.ordinals.entry(format!("{sid}/{mid}")).or_default();
                    state
                        .parts
                        .insert(id.into(), (*ordinal, name.into(), sid.into()));
                    *ordinal += 1;
                }
                let ordinal = state.parts[id].0;
                if !self.listening() {
                    return Ok(());
                }
                if name == "tool" {
                    let call = part["callID"].clone();
                    let status = part["state"]["status"].as_str().unwrap_or_default();
                    let mut data = json!({"sessionID":sid,"assistantMessageID":mid,"id":call,"name":wire::tool_name(part["tool"].as_str().unwrap_or_default())});
                    if new {
                        self.send(
                            session,
                            sid,
                            "session.tool.input.started",
                            data.clone(),
                            true,
                        );
                        let mut ended = data.clone();
                        ended["text"] = json!(part["state"]["input"].to_string());
                        self.send(session, sid, "session.tool.input.ended", ended, true);
                    }
                    let converted = wire::content(part).ok_or("Invalid tool part")?;
                    data["input"] = part["state"]["input"].clone();
                    data["executed"] = json!(false);
                    match status {
                        "running" if part["state"].get("metadata").is_none() => {
                            self.send(session, sid, "session.tool.called", data, true)
                        }
                        "running" => {
                            data["metadata"] = converted["state"]["metadata"].clone();
                            self.send(session, sid, "session.tool.progress", data, false);
                        }
                        "completed" | "error" => {
                            data["metadata"] = converted["state"]["metadata"].clone();
                            if status == "completed" {
                                data["content"] = converted["state"]["content"].clone();
                            } else {
                                data["error"] = converted["state"]["error"].clone();
                            }
                            self.send(
                                session,
                                sid,
                                if status == "completed" {
                                    "session.tool.success"
                                } else {
                                    "session.tool.failed"
                                },
                                data,
                                true,
                            );
                        }
                        _ => {}
                    }
                } else {
                    let mut data =
                        json!({"sessionID":sid,"assistantMessageID":mid,"ordinal":ordinal});
                    if new {
                        self.send(
                            session,
                            sid,
                            &format!("session.{name}.started"),
                            data.clone(),
                            true,
                        );
                    }
                    if part["time"]["end"].is_number() {
                        data["text"] = part["text"].clone();
                        self.send(session, sid, &format!("session.{name}.ended"), data, true);
                    }
                }
            }
            "message.part.delta" => {
                if let Some((ordinal, name, _)) =
                    p["partID"].as_str().and_then(|id| state.parts.get(id))
                {
                    self.send(session, sid,&format!("session.{name}.delta"),json!({"sessionID":sid,"assistantMessageID":p["messageID"],"ordinal":ordinal,"delta":p["delta"]}),false);
                }
            }
            "permission.asked" => {
                let mut data = json!({"id":p["id"],"sessionID":sid,"action":wire::tool_name(p["permission"].as_str().unwrap_or_default()),"resources":p["patterns"],"save":p["always"],"metadata":p["metadata"]});
                if let Some(tool) = p.get("tool") {
                    data["source"] =
                        json!({"type":"tool","messageID":tool["messageID"],"id":tool["callID"]});
                }
                self.send(session, sid, "permission.asked", data, false);
            }
            "permission.replied" => self.send(
                session,
                sid,
                "permission.replied",
                json!({"sessionID":sid,"requestID":p["requestID"],"reply":p["reply"]}),
                false,
            ),
            "question.asked" => {
                let request: QuestionRequest = serde_json::from_value(p.clone())
                    .map_err(|e| OperationError::from(e.to_string()))?;
                self.send(
                    session,
                    sid,
                    "form.created",
                    json!({"form":wire::form(&request)}),
                    false,
                );
            }
            "question.replied" | "question.rejected" => {
                let id = p["requestID"].as_str().unwrap_or_default();
                let mut data =
                    json!({"id":format!("frm_{}",id.trim_start_matches("que_")),"sessionID":sid});
                if kind == "question.replied" {
                    let answers = p["answers"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .enumerate()
                        .map(|(n, a)| (n.to_string(), a.clone()))
                        .collect::<serde_json::Map<_, _>>();
                    data["answer"] = json!(answers);
                }
                self.send(
                    session,
                    sid,
                    if kind == "question.replied" {
                        "form.replied"
                    } else {
                        "form.cancelled"
                    },
                    data,
                    false,
                );
            }
            "ocmini.compaction.started" => {
                if let Some(mid) = p["messageID"].as_str() {
                    state.compactions.insert(
                        mid.into(),
                        if p["mode"] == "manual" {
                            "manual"
                        } else {
                            "auto"
                        },
                    );
                }
                self.send(
                    session,
                    sid,
                    "session.compaction.started",
                    json!({"sessionID":sid,"reason":if p["mode"]=="manual"{"manual"}else{"auto"}}),
                    true,
                );
            }
            "ocmini.compaction.failed" => {
                let Some(reason) = p["messageID"]
                    .as_str()
                    .and_then(|mid| state.compactions.remove(mid))
                else {
                    return Ok(());
                };
                self.send(session, sid,"session.compaction.failed",json!({"sessionID":sid,"reason":reason,"error":wire::error(p["error"].as_str().unwrap_or("Compaction failed"))}),true);
            }
            _ => {}
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn observe(events: &Events, kind: &str, properties: Value) {
        events.observe(
            &serde_json::from_value::<BusEvent>(
                json!({"id":hex_id("evt_"),"type":kind,"properties":properties}),
            )
            .unwrap(),
        );
    }
    #[test]
    fn saved_sessions_are_lazy_and_an_unobserved_execution_does_not_write_counters() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("state.db");
        let store = Arc::new(Store::open(&path, "events").unwrap());
        let mut sessions = Vec::new();
        for _ in 0..64 {
            let s = Session::new("Saved", "/tmp");
            store.create_session(&s).unwrap();
            store.v2_update(&s.id, &json!({})).unwrap();
            sessions.push(s);
        }
        let events = Events::new(&store).unwrap();
        assert!(events.sessions.lock().unwrap().is_empty());
        assert!(events.tx.get().is_none());
        for s in &sessions {
            assert!(events.enabled(&s.id));
        }
        assert!(
            events.sessions.lock().unwrap().is_empty(),
            "reading saved sessions must not retain all IDs"
        );
        let sid = &sessions[0].id;
        observe(
            &events,
            "session.status",
            json!({"sessionID":sid,"status":{"type":"busy"}}),
        );
        assert_eq!(events.sessions.lock().unwrap().len(), 1);
        events.emit(
            sid,
            "session.renamed",
            json!({"sessionID":sid,"title":"Unobserved"}),
            true,
        );
        observe(&events, "session.idle", json!({"sessionID":sid}));
        assert!(events.sessions.lock().unwrap().is_empty());
        assert_eq!(store.v2_state(sid).unwrap()["outcome"], "succeeded");
        let db = rusqlite::Connection::open(path).unwrap();
        let seq: i64 = db
            .query_row("SELECT seq FROM v2_state WHERE session_id=?1", [sid], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(
            seq, 0,
            "no event-log counters are needed when nobody observes the live feed"
        );
    }
    #[test]
    fn one_session_lock_does_not_hold_up_other_sessions() {
        let store = Arc::new(Store::open(std::path::Path::new(":memory:"), "events").unwrap());
        let a = Session::new("A", "/tmp");
        let b = Session::new("B", "/tmp");
        for s in [&a, &b] {
            store.create_session(s).unwrap();
            store.v2_update(&s.id, &json!({})).unwrap();
        }
        let events = Events::new(&store).unwrap();
        let _receiver = events.subscribe();
        let a_state = events.session(&a.id);
        let guard = a_state.state.lock().unwrap();
        let (sent, received) = std::sync::mpsc::channel();
        let a_thread = {
            let events = events.clone();
            std::thread::spawn(move || {
                events.emit(
                    &a.id,
                    "session.renamed",
                    json!({"sessionID":a.id,"title":"A"}),
                    true,
                )
            })
        };
        let b_thread = {
            let events = events.clone();
            std::thread::spawn(move || {
                events.emit(
                    &b.id,
                    "session.usage.updated",
                    json!({"sessionID":b.id,"cost":0,"tokens":Tokens::default()}),
                    false,
                );
                sent.send(()).unwrap();
            })
        };
        let completed = received.recv_timeout(std::time::Duration::from_secs(2));
        drop(guard);
        a_thread.join().unwrap();
        b_thread.join().unwrap();
        assert!(
            completed.is_ok(),
            "an independent session must make progress"
        );
    }
}
