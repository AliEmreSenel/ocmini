use super::wire;
use crate::{store::Store, types::*};
use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex, OnceLock, Weak},
};
use tokio::sync::broadcast;

pub type Frame = Result<Arc<str>, Arc<str>>;
#[derive(Default)]
struct State {
    enabled: HashSet<String>,
    active: HashSet<String>,
    failures: HashMap<String, Value>,
    parts: HashMap<String, (usize, String, String)>,
    steps: HashSet<String>,
    ordinals: HashMap<String, usize>,
    compactions: HashMap<String, &'static str>,
}
pub struct Events {
    store: Weak<Store>,
    state: Mutex<State>,
    tx: OnceLock<broadcast::Sender<Frame>>,
}
impl Events {
    pub fn new(store: &Arc<Store>) -> Result<Arc<Self>, String> {
        Ok(Arc::new(Self {
            store: Arc::downgrade(store),
            state: Mutex::new(State {
                enabled: store.v2_sessions()?.into_iter().collect(),
                ..Default::default()
            }),
            tx: OnceLock::new(),
        }))
    }
    pub fn subscribe(&self) -> broadcast::Receiver<Frame> {
        self.tx
            .get_or_init(|| broadcast::channel(16384).0)
            .subscribe()
    }
    fn publish(&self, frame: Frame) {
        if let Some(tx) = self.tx.get() {
            let _ = tx.send(frame);
        }
    }
    pub fn enabled(&self, sid: &str) -> bool {
        self.state.lock().unwrap().enabled.contains(sid)
    }
    pub fn enable(&self, sid: &str) -> Result<(), OperationError> {
        let mut state = self.state.lock().unwrap();
        if !state.enabled.contains(sid) {
            self.store
                .upgrade()
                .ok_or("Runtime stopped")?
                .v2_update(sid, &json!({}))?;
            state.enabled.insert(sid.into());
        }
        Ok(())
    }
    pub fn prepare_delete(&self, session: &Session) -> Result<Option<Frame>, OperationError> {
        let state = self.state.lock().unwrap();
        if !state.enabled.contains(&session.id) {
            return Ok(None);
        }
        let store = self.store.upgrade().ok_or("Runtime stopped")?;
        let seq = store.v2_sequence(&session.id)?;
        Ok(Some(Ok(Arc::from(json!({"id":hex_id("evt_"),"created":now_ms(),"type":"session.deleted","data":{"sessionID":session.id},"location":{"directory":session.directory},"durable":{"aggregateID":session.id,"seq":seq,"version":1}}).to_string()))))
    }
    pub fn deleted(&self, sid: &str, frame: Frame) {
        let mut state = self.state.lock().unwrap();
        state.enabled.remove(sid);
        state.active.remove(sid);
        state.failures.remove(sid);
        self.publish(frame);
    }
    pub fn emit(&self, sid: &str, kind: &str, data: Value, durable: bool) {
        let _guard = self.state.lock().unwrap();
        self.send(sid, kind, data, durable);
    }
    fn send(&self, sid: &str, kind: &str, data: Value, durable: bool) {
        let Some(store) = self.store.upgrade() else {
            return;
        };
        let mut value = json!({"id":hex_id("evt_"),"created":now_ms(),"type":kind,"data":data});
        if let Some(directory) = store.directory_of(sid) {
            value["location"] = json!({"directory":directory});
        }
        if durable {
            match store.v2_sequence(sid) {
                Ok(seq) => {
                    value["durable"] = json!({"aggregateID":sid,"seq":seq,"version":if matches!(kind,"session.tool.success"|"session.tool.failed"|"session.forked"){2}else{1}})
                }
                Err(error) => {
                    self.publish(Err(Arc::from(error.message)));
                    return;
                }
            }
        }
        if self.tx.get().is_some() {
            self.publish(Ok(Arc::from(value.to_string())));
        }
    }
    pub fn observe(&self, event: &BusEvent) {
        let Some(store) = self.store.upgrade() else {
            return;
        };
        let p = &event.properties;
        let kind = event.kind.as_str();
        let sid = p["sessionID"]
            .as_str()
            .or_else(|| p["info"]["id"].as_str())
            .unwrap_or_default();
        if sid.is_empty() {
            return;
        }
        let mut state = self.state.lock().unwrap();
        if kind == "session.created"
            && p["info"]["parentID"]
                .as_str()
                .is_some_and(|parent| state.enabled.contains(parent))
        {
            match store.v2_update(sid, &json!({})) {
                Ok(_) => {
                    state.enabled.insert(sid.into());
                }
                Err(e) => {
                    self.publish(Err(Arc::from(e.message)));
                    return;
                }
            }
        }
        if !state.enabled.contains(sid) {
            return;
        }
        let result = self.translate(&store, &mut state, sid, kind, p);
        if let Err(error) = result {
            self.publish(Err(Arc::from(error.message)));
        }
    }
    fn translate(
        &self,
        store: &Store,
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
                self.send(sid, "session.created", data, true);
            }
            "session.updated" => self.send(
                sid,
                "session.renamed",
                json!({"sessionID":sid,"title":p["info"]["title"]}),
                true,
            ),
            "ocmini.prompt.accepted" => {
                if let Some(message) =
                    store.message(sid, p["messageID"].as_str().unwrap_or_default())?
                {
                    let input = wire::receipt(sid, &message)?;
                    self.send(sid,"session.inbox.enqueued",json!({"sessionID":sid,"inboxID":message.info.id,"item":{"type":input["type"],"delivery":input["delivery"],"payload":input["payload"]}}),true);
                }
            }
            "ocmini.turn.started" => {
                self.send(
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
                    self.send(sid,"session.retry.scheduled",json!({"sessionID":sid,"assistantMessageID":amid,"attempt":p["status"]["attempt"],"at":p["status"]["next"],"error":wire::error(p["status"]["message"].as_str().unwrap_or("Retrying"))}),true);
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
                    store.v2_update(sid, &json!({"outcome":outcome,"idle":now}))?;
                    store.v2_control(sid, json!({"type":"idle","outcome":outcome}))?;
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
                    self.send(sid, &format!("session.execution.{outcome}"), data, true);
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
                        self.send(sid,"session.compaction.failed",json!({"sessionID":sid,"reason":reason,"error":wire::error(info["error"]["data"]["message"].as_str().unwrap_or("Compaction failed"))}),true);
                    }
                    return Ok(());
                }
                if info["summary"] == true {
                    let reason = state.compactions.remove(mid).unwrap_or("auto");
                    let m = store
                        .message(sid, mid)?
                        .ok_or("Compaction message missing")?;
                    self.send(sid,"session.compaction.ended",json!({"sessionID":sid,"reason":reason,"text":text_parts(&m.parts).collect::<Vec<_>>().join("\n"),"model":{"providerID":info["providerID"],"id":info["modelID"]},"cost":info["cost"].as_f64().unwrap_or(0.0),"tokens":info["tokens"],"metadata":{}}),true);
                } else if state.steps.insert(key) {
                    let mut model = json!({"id":info["modelID"],"providerID":info["providerID"]});
                    if let Some(variant) = info.get("variant") {
                        model["variant"] = variant.clone();
                    }
                    self.send(sid,"session.step.started",json!({"sessionID":sid,"assistantMessageID":mid,"agent":info["agent"].as_str().unwrap_or("build"),"model":model,"started":info["time"]["created"]}),true);
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
                if name == "tool" {
                    let call = part["callID"].clone();
                    let status = part["state"]["status"].as_str().unwrap_or_default();
                    let mut data = json!({"sessionID":sid,"assistantMessageID":mid,"id":call,"name":wire::tool_name(part["tool"].as_str().unwrap_or_default())});
                    if new {
                        self.send(sid, "session.tool.input.started", data.clone(), true);
                        let mut ended = data.clone();
                        ended["text"] = json!(part["state"]["input"].to_string());
                        self.send(sid, "session.tool.input.ended", ended, true);
                    }
                    let converted = wire::content(part).ok_or("Invalid tool part")?;
                    data["input"] = part["state"]["input"].clone();
                    data["executed"] = json!(false);
                    match status {
                        "running" if part["state"].get("metadata").is_none() => {
                            self.send(sid, "session.tool.called", data, true)
                        }
                        "running" => {
                            data["metadata"] = converted["state"]["metadata"].clone();
                            self.send(sid, "session.tool.progress", data, false);
                        }
                        "completed" | "error" => {
                            data["metadata"] = converted["state"]["metadata"].clone();
                            if status == "completed" {
                                data["content"] = converted["state"]["content"].clone();
                            } else {
                                data["error"] = converted["state"]["error"].clone();
                            }
                            self.send(
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
                        self.send(sid, &format!("session.{name}.started"), data.clone(), true);
                    }
                    if part["time"]["end"].is_number() {
                        data["text"] = part["text"].clone();
                        self.send(sid, &format!("session.{name}.ended"), data, true);
                    }
                }
            }
            "message.part.delta" => {
                if let Some((ordinal, name, _)) =
                    p["partID"].as_str().and_then(|id| state.parts.get(id))
                {
                    self.send(sid,&format!("session.{name}.delta"),json!({"sessionID":sid,"assistantMessageID":p["messageID"],"ordinal":ordinal,"delta":p["delta"]}),false);
                }
            }
            "permission.asked" => {
                let mut data = json!({"id":p["id"],"sessionID":sid,"action":wire::tool_name(p["permission"].as_str().unwrap_or_default()),"resources":p["patterns"],"save":p["always"],"metadata":p["metadata"]});
                if let Some(tool) = p.get("tool") {
                    data["source"] =
                        json!({"type":"tool","messageID":tool["messageID"],"id":tool["callID"]});
                }
                self.send(sid, "permission.asked", data, false);
            }
            "permission.replied" => self.send(
                sid,
                "permission.replied",
                json!({"sessionID":sid,"requestID":p["requestID"],"reply":p["reply"]}),
                false,
            ),
            "question.asked" => {
                let request: QuestionRequest = serde_json::from_value(p.clone())
                    .map_err(|e| OperationError::from(e.to_string()))?;
                self.send(
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
                self.send(sid,"session.compaction.failed",json!({"sessionID":sid,"reason":reason,"error":wire::error(p["error"].as_str().unwrap_or("Compaction failed"))}),true);
            }
            _ => {}
        }
        Ok(())
    }
}
