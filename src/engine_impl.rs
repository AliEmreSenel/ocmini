use crate::config::{CompactionCfg, Config};
use crate::engine::{bus, App, EngineApi};
use crate::provider::{self, ChatRequest};
use crate::store::Store;
use crate::title;
use crate::tools::{self, ToolCtx};
use crate::types::*;
use base64::Engine as Base64Engine;
use futures::StreamExt;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::oneshot;

struct Running {
    app: App,
    cancel: Arc<AtomicBool>,
    waiters: HashMap<String, Vec<oneshot::Sender<Value>>>,
}

#[derive(Clone, Default)]
pub struct Engine {
    running: Arc<Mutex<HashMap<String, Running>>>,
}

impl Engine {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn abort_and_wait(&self, sid: &str) {
        self.abort(sid);
        while self.running.lock().unwrap().contains_key(sid) {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    fn wake(&self, app: App, sid: String) {
        let mut running = self.running.lock().unwrap();
        if running.contains_key(&sid) {
            return;
        }
        let cancel = match self.claim(&mut running, &app, &sid, None) {
            Ok(cancel) => cancel,
            Err(error) => {
                emit_error(&sid, &error, false);
                return;
            }
        };
        drop(running);
        let engine = self.clone();
        tokio::spawn(async move {
            engine.drain(app, sid, cancel).await;
        });
    }

    fn claim(
        &self,
        running: &mut HashMap<String, Running>,
        app: &App,
        sid: &str,
        waiter: Option<(String, oneshot::Sender<Value>)>,
    ) -> Result<Arc<AtomicBool>, String> {
        app.store.claim(sid)?;
        app.store.set_busy(sid, true)?;
        let cancel = Arc::new(AtomicBool::new(false));
        running.insert(
            sid.into(),
            Running {
                app: app.clone(),
                cancel: cancel.clone(),
                waiters: waiter
                    .into_iter()
                    .map(|(mid, tx)| (mid, vec![tx]))
                    .collect(),
            },
        );
        update_status(app, sid, json!({"type":"busy"}));
        Ok(cancel)
    }

    async fn drain(&self, app: App, sid: String, cancel: Arc<AtomicBool>) {
        // Keep the ownership lease fresh while this instance owns the session,
        // so a sibling instance's restart recovery ignores its live work.
        let heartbeat = {
            let (app, sid, cancel) = (app.clone(), sid.clone(), cancel.clone());
            tokio::spawn(async move {
                loop {
                    tokio::select! {
                        _ = tokio::time::sleep(std::time::Duration::from_secs(20)) => { let _ = app.store.heartbeat(&sid); }
                        _ = cancelled(&cancel) => break,
                    }
                }
            })
        };
        self.drain_inner(&app, &sid, &cancel).await;
        heartbeat.abort();
    }

    async fn drain_inner(&self, app: &App, sid: &str, cancel: &Arc<AtomicBool>) {
        let sid = sid.to_string();
        let app = app.clone();
        let cancel = cancel.clone();
        loop {
            // Admission and the idle transition share this lock. A prompt cannot
            // land between the final inbox check and releasing session ownership.
            let request = {
                let mut running = self.running.lock().unwrap();
                if cancel.load(Ordering::Acquire) {
                    if let Err(e) = app.store.cancel_pending(&sid) {
                        emit_error(&sid, &e, false);
                    }
                    finish_running(&app, &sid, running.remove(&sid), Some("Aborted"));
                    return;
                }
                match app.store.next_prompt(&sid) {
                    Ok(Some(request)) => request,
                    Ok(None) => {
                        finish_running(&app, &sid, running.remove(&sid), None);
                        return;
                    }
                    Err(error) => {
                        emit_error(&sid, &error, false);
                        finish_running(&app, &sid, running.remove(&sid), Some(&error));
                        return;
                    }
                }
            };
            let mid = request.message_id.clone().unwrap_or_default();
            let engine: Arc<dyn EngineApi> = Arc::new(self.clone());
            let result = run_turn(app.clone(), engine, &sid, request, cancel.clone()).await;
            let value = match result {
                Ok(value) => value,
                Err(error) => {
                    emit_error(&sid, &error, cancel.load(Ordering::Acquire));
                    json!({"error":error})
                }
            };
            if let Err(error) = app.store.finish_prompt(&mid) {
                emit_error(&sid, &error, false);
            }
            let mut running = self.running.lock().unwrap();
            if let Some(state) = running.get_mut(&sid) {
                for waiter in state.waiters.remove(&mid).unwrap_or_default() {
                    let _ = waiter.send(value.clone());
                }
            }
        }
    }
}

fn finish_running(app: &App, sid: &str, running: Option<Running>, error: Option<&str>) {
    app.perms.reject_all(sid);
    app.questions.reject_all(sid);
    app.store.release(sid);
    if let Err(error) = app.store.set_busy(sid, false) {
        eprintln!("ocmini: {error}");
    }
    if let Some(running) = running {
        let value = json!({"error":error.unwrap_or("Prompt is no longer pending")});
        for waiters in running.waiters.into_values() {
            for waiter in waiters {
                let _ = waiter.send(value.clone());
            }
        }
    }
    update_status(app, sid, json!({"type":"idle"}));
    bus().emit("session.idle", json!({"sessionID":sid}));
}

fn update_status(app: &App, sid: &str, status: Value) {
    if status["type"] == "idle" {
        app.statuses.lock().unwrap().remove(sid);
    } else {
        app.statuses
            .lock()
            .unwrap()
            .insert(sid.into(), status.clone());
    }
    bus().emit("session.status", json!({"sessionID":sid,"status":status}));
}

impl EngineApi for Engine {
    fn start_turn(
        &self,
        app: App,
        sid: String,
        mut request: PromptRequest,
    ) -> Result<oneshot::Receiver<Value>, String> {
        let session = app.store.get_session(&sid)?.ok_or("Session not found")?;
        let mut guard = self.running.lock().unwrap();
        let mid = request.message_id.clone().unwrap_or_else(|| hex_id("msg_"));
        if mid.is_empty()
            || mid.len() > 256
            || !mid
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
        {
            return Err(
                "messageID must contain 1–256 ASCII letters, digits, underscores or hyphens".into(),
            );
        }
        let (tx, rx) = oneshot::channel();
        if let Some(owner) = app.store.message_owner(&mid)? {
            if owner != sid {
                return Err("messageID already belongs to another session".into());
            }
            let existing = app.store.message(&sid, &mid)?.ok_or("Message not found")?;
            if existing.info.role != "user" {
                return Err("messageID already belongs to an assistant message".into());
            }
            if app.store.is_pending_message(&mid)? {
                if let Some(state) = guard.get_mut(&sid) {
                    state.waiters.entry(mid).or_default().push(tx);
                    return Ok(rx);
                }
                let scoped = app.for_directory(&session.directory)?;
                let cancel = self.claim(&mut guard, &scoped, &sid, Some((mid, tx)))?;
                drop(guard);
                let engine = self.clone();
                tokio::spawn(async move {
                    engine.drain(scoped, sid, cancel).await;
                });
                return Ok(rx);
            }
            let result = app.store.reply_to(&sid, &mid)?.unwrap_or(existing);
            let _ = tx.send(serde_json::to_value(result).map_err(|e| e.to_string())?);
            return Ok(rx);
        }
        let app = app.for_directory(&session.directory)?;
        let cfg = app.config.lock().unwrap().clone();
        request.message_id = Some(mid.clone());
        let agent = request
            .agent
            .get_or_insert_with(|| {
                cfg.extra
                    .get("default_agent")
                    .and_then(Value::as_str)
                    .unwrap_or("build")
                    .to_string()
            })
            .clone();
        if !["build", "plan", "general", "explore", "subagent"].contains(&agent.as_str())
            && !cfg.agent.contains_key(&agent)
        {
            return Err(format!("Unknown agent '{agent}'"));
        }
        if cfg.agent.get(&agent).is_some_and(|a| a["disable"] == true) {
            return Err(format!("Agent '{agent}' is disabled"));
        }
        let model = resolve_model(&cfg, &app, &sid, request.model.as_ref(), Some(&agent))?;
        if !model_available(&cfg, &model) {
            return Err(format!("Provider or model is disabled: {model}"));
        }
        request.model = Some(model);
        if request.parts.is_empty() {
            return Err("Prompt requires at least one text or file part".into());
        }
        let mut seen_parts = HashSet::new();
        for part in &mut request.parts {
            let kind = part["type"]
                .as_str()
                .ok_or("Every prompt part requires a type")?;
            match kind {
                "text" if part["text"].is_string() => {}
                "file" if part["url"].is_string() && part["mime"].is_string() => {}
                "agent" if part["name"].is_string() => {}
                "compaction" if request.command.as_deref() == Some("__ocmini_compact") => {}
                _ => return Err(format!("Invalid or unsupported prompt part '{kind}'")),
            }
            let id = part["id"]
                .as_str()
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| hex_id("prt_"));
            if !seen_parts.insert(id.clone()) {
                return Err("Duplicate part id in prompt".into());
            }
            part["id"] = json!(id);
            part["messageID"] = json!(mid);
            part["sessionID"] = json!(sid);
        }
        // rowid is the stable tie breaker for messages created in one millisecond.
        let created = now_ms();
        let mut info = MessageInfo {
            id: mid.clone(),
            session_id: sid.clone(),
            role: "user".into(),
            time: MessageTime {
                created,
                completed: None,
            },
            model: request.model.clone(),
            agent: Some(agent),
            system: request.system.clone(),
            variant: request.variant.clone(),
            ..Default::default()
        };
        if let Some(tools) = &request.tools {
            info.extra.insert("tools".into(), json!(tools));
        }
        if let Some(command) = &request.command {
            info.extra.insert("command".into(), json!(command));
        }
        let message = MessageWithParts {
            info,
            parts: request.parts.clone(),
        };
        app.store.admit(&request, &message)?;
        emit_message(&message.info);
        for part in &message.parts {
            emit_part(part);
        }
        // A launcher title ("tui session", "run") says nothing about the chat.
        // Name the session after its first prompt, with the model writing the
        // title in the background when `title_model` is configured.
        if let Some(model) = &request.model {
            title::launch(app.clone(), sid.clone(), &message.parts, model);
        }
        if request.no_reply {
            let _ = tx.send(serde_json::to_value(message).map_err(|e| e.to_string())?);
            return Ok(rx);
        }
        if let Some(state) = guard.get_mut(&sid) {
            state.waiters.entry(mid).or_default().push(tx);
            return Ok(rx);
        }
        let cancel = self.claim(&mut guard, &app, &sid, Some((mid, tx)))?;
        drop(guard);
        let engine = self.clone();
        tokio::spawn(async move {
            engine.drain(app, sid, cancel).await;
        });
        Ok(rx)
    }

    fn abort(&self, sid: &str) {
        let mut queue = vec![sid.to_string()];
        let running = self.running.lock().unwrap();
        let mut visited = HashSet::new();
        while let Some(id) = queue.pop() {
            if !visited.insert(id.clone()) {
                continue;
            }
            crate::tools::cancel_background_jobs(&id);
            if let Some(state) = running.get(&id) {
                state.cancel.store(true, Ordering::Release);
                state.app.perms.reject_all(&id);
                state.app.questions.reject_all(&id);
                queue.extend(
                    state
                        .app
                        .store
                        .children(&id)
                        .unwrap_or_default()
                        .into_iter()
                        .map(|s| s.id),
                );
            }
        }
    }

    fn run_sync(
        &self,
        app: App,
        sid: String,
        request: PromptRequest,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Value, String>> + Send>> {
        let admitted = self.start_turn(app, sid, request);
        Box::pin(async move {
            let value = admitted?.await.map_err(|_| {
                "Session execution stopped before a response was produced".to_string()
            })?;
            if let Some(error) = value["error"].as_str() {
                return Err(error.to_string());
            }
            Ok(value)
        })
    }

    fn summarize(
        &self,
        app: App,
        sid: String,
        model: Option<ModelRef>,
    ) -> Result<oneshot::Receiver<Value>, String> {
        self.start_turn(
            app,
            sid,
            PromptRequest {
                model,
                parts: vec![json!({"type":"compaction","auto":false})],
                command: Some("__ocmini_compact".into()),
                ..Default::default()
            },
        )
    }

    fn recover(&self, app: App) {
        for sid in app.store.pending_sessions().unwrap_or_default() {
            if let Some(session) = app.store.get_session(&sid).unwrap_or_default() {
                match app.for_directory(&session.directory) {
                    Ok(scoped) => self.wake(scoped, sid),
                    Err(error) => emit_error(&sid, &error, false),
                }
            }
        }
    }
}

fn resolve_model(
    cfg: &Config,
    app: &App,
    sid: &str,
    selected: Option<&ModelRef>,
    agent: Option<&str>,
) -> Result<ModelRef, String> {
    if let Some(model) = selected {
        if model.provider_id.is_empty() || model.model_id.is_empty() {
            return Err("model requires nonempty providerID and modelID".into());
        }
        return Ok(model.clone());
    }
    if let Some(model) = agent
        .and_then(|a| cfg.agent.get(a))
        .and_then(|a| a["model"].as_str())
        .and_then(|m| m.split_once('/'))
    {
        return Ok(ModelRef {
            provider_id: model.0.into(),
            model_id: model.1.into(),
        });
    }
    if let Some(model) = cfg
        .model
        .split_once('/')
        .filter(|(p, m)| !p.is_empty() && !m.is_empty())
    {
        return Ok(ModelRef {
            provider_id: model.0.into(),
            model_id: model.1.into(),
        });
    }
    if let Some(model) = app.store.last_model(sid)? {
        return Ok(model);
    }
    let catalog = cfg.catalog();
    if catalog.len() == 1 {
        return Ok(ModelRef {
            provider_id: catalog[0]["providerID"].as_str().unwrap_or_default().into(),
            model_id: catalog[0]["modelID"].as_str().unwrap_or_default().into(),
        });
    }
    Err("No model selected. Set model to 'provider/model' in opencode.json or supply model.providerID and model.modelID.".into())
}

/// True when the config allows a model: provider enabled, model present and
/// not blacklisted, whitelisted away, or disabled.
pub(crate) fn model_available(cfg: &Config, model: &ModelRef) -> bool {
    if !cfg.provider_enabled(&model.provider_id) {
        return false;
    }
    cfg.provider(&model.provider_id).is_none_or(|provider| {
        !(provider
            .extra
            .get("blacklist")
            .and_then(Value::as_array)
            .is_some_and(|models| models.iter().any(|id| id == &model.model_id))
            || provider
                .extra
                .get("whitelist")
                .and_then(Value::as_array)
                .is_some_and(|models| !models.iter().any(|id| id == &model.model_id))
            || provider.models.get(&model.model_id).is_some_and(|info| {
                info.extra.get("disabled").and_then(Value::as_bool) == Some(true)
            }))
    })
}

fn emit_message(info: &MessageInfo) {
    bus().emit(
        "message.updated",
        json!({"sessionID":info.session_id,"info":info}),
    );
}
fn emit_part(part: &Value) {
    bus().emit(
        "message.part.updated",
        json!({"sessionID":part["sessionID"],"part":part,"time":{"created":now_ms()}}),
    );
}
fn error_value(error: &str, aborted: bool) -> Value {
    json!({"name":if aborted {"MessageAbortedError"} else {"UnknownError"},"data":{"message":error}})
}
fn emit_error(sid: &str, error: &str, aborted: bool) {
    bus().emit(
        "session.error",
        json!({"sessionID":sid,"error":error_value(error,aborted)}),
    );
}

pub async fn cancelled(cancel: &Arc<AtomicBool>) {
    while !cancel.load(Ordering::Acquire) {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

const COMPACTION_PROMPT: &str = "Summarize the conversation for another coding agent to continue. Preserve the user's goal, constraints, decisions, changed files, executed commands and their results, unresolved errors, and next actions. Quote relevant identifiers and paths. Do not perform the task or call tools. Produce a substantive continuation summary.";

#[derive(Default)]
struct CompactionPolicy {
    settings: CompactionCfg,
    context: Option<u64>,
    input: Option<u64>,
    output: u64,
}

impl CompactionPolicy {
    fn for_model(cfg: &Config, model: &ModelRef) -> Self {
        let provider = cfg.provider(&model.provider_id);
        let configured = provider.and_then(|provider| provider.models.get(&model.model_id));
        let limit = configured.and_then(|model| model.limit.as_ref());
        let output = limit
            .and_then(|limit| limit["output"].as_u64())
            .filter(|output| *output > 0)
            .unwrap_or(32_000)
            .min(32_000);
        Self {
            settings: cfg.compaction.clone(),
            context: limit.and_then(|limit| limit["context"].as_u64()),
            input: limit.and_then(|limit| limit["input"].as_u64()),
            output,
        }
    }

    fn usable(&self) -> Option<u64> {
        self.settings.usable(self.context, self.input, self.output)
    }
}

/// Keep roughly 15k tokens verbatim (less for small windows). Budget whole
/// messages, including tool arguments/results; never start with an orphan result.
/// The latest user message takes priority even when it exceeds the soft budget.
fn compaction_messages(
    mut messages: Vec<Value>,
    has_summary: bool,
    policy: &CompactionPolicy,
) -> (Vec<Value>, Vec<Value>) {
    let first = usize::from(has_summary);
    let user = messages
        .iter()
        .enumerate()
        .skip(first)
        .rfind(|(_, message)| message["role"] == "user")
        .map(|(index, _)| index);
    let budget = policy.settings.recent_bytes(policy.usable());
    let retain = budget > 0 && policy.settings.tail_turns != Some(0);
    let user = user.filter(|_| retain);
    let recent_first = policy
        .settings
        .tail_turns
        .and_then(|turns| {
            messages
                .iter()
                .enumerate()
                .skip(first)
                .rev()
                .filter(|(_, message)| message["role"] == "user")
                .nth(usize::try_from(turns.saturating_sub(1)).unwrap_or(usize::MAX))
                .map(|(index, _)| index)
        })
        .unwrap_or(first);
    let mut bytes = user.map_or(0, |index| messages[index].to_string().len());
    let mut start = messages.len();
    for index in (recent_first..messages.len()).rev().filter(|_| retain) {
        let size = if Some(index) == user {
            0
        } else {
            messages[index].to_string().len()
        };
        if bytes.saturating_add(size) > budget {
            break;
        }
        bytes += size;
        if messages[index]["role"] == "user" || messages[index]["role"] == "assistant" {
            start = index;
        }
    }
    let mut recent = messages.split_off(start);
    if let Some(index) = user.filter(|index| *index < start) {
        recent.insert(0, messages[index].clone());
    }
    if messages.is_empty() {
        // Short conversations can fit entirely in the verbatim tail. Still
        // produce a useful checkpoint when compaction is explicitly requested.
        messages = recent.clone();
    }

    // Leave room for the summarizer's reply, preserving the previous checkpoint
    // and whole assistant/tool groups when older context needs to be shortened.
    let budget = usize::try_from(
        policy
            .usable()
            .unwrap_or(64_000)
            .saturating_mul(2)
            .max(8_000),
    )
    .unwrap_or(usize::MAX);
    let mut total = 0usize;
    let mut start = messages.len();
    while start > first {
        let size = messages[start - 1].to_string().len();
        if start < messages.len() && total.saturating_add(size) > budget {
            break;
        }
        total = total.saturating_add(size);
        start -= 1;
    }
    while start > first && messages[start]["role"] == "tool" {
        start -= 1;
    }
    let older = messages
        .into_iter()
        .enumerate()
        .filter(|(index, _)| *index >= start || has_summary && *index == 0)
        .map(|(_, message)| message)
        .collect();
    (older, recent)
}

/// Compact history mid-turn: ask the model for a continuation summary and
/// persist it as a summary message. build_messages restarts history from the
/// newest summary, so the next request carries the summary instead of the
/// transcript that overflowed the provider's context window.
async fn compact_history(
    app: &App,
    adapter: &dyn provider::ProviderAdapter,
    current: &MessageInfo,
    messages: Vec<Value>,
    has_summary: bool,
    policy: &CompactionPolicy,
    cancel: &Arc<AtomicBool>,
) -> Result<(), String> {
    let sid = &current.session_id;
    let (mut request_messages, recent) = compaction_messages(messages, has_summary, policy);
    if request_messages.is_empty() {
        return Err("Nothing to compact".into());
    }
    request_messages
        .push(json!({"role": "user", "content": "Produce the continuation summary now."}));
    let request = ChatRequest {
        model: current.model_id.clone().ok_or("Missing compaction model")?,
        system: COMPACTION_PROMPT.into(),
        messages: request_messages,
        tools: Vec::new(),
        variant: None,
        session_id: Some(sid.into()),
    };
    // The summary request carries most of the window, so it is the largest and
    // slowest request of the turn. The transport deaths the main loop retries
    // must not end recovery here, or the overflow error surfaces verbatim and
    // the turn is lost.
    const MAX_COMPACTION_RETRIES: u32 = 3;
    let mut summary = String::new();
    let mut attempt = 0u32;
    'summary: loop {
        summary.clear();
        let mut stream = match tokio::select! {
            result = adapter.stream(&request) => result,
            _ = cancelled(cancel) => return Err("Aborted".into()),
        } {
            Ok(stream) => stream,
            Err(error) if error.retryable && attempt < MAX_COMPACTION_RETRIES => {
                attempt += 1;
                tokio::select! {
                    _ = tokio::time::sleep(retry_delay(error.retry_after, attempt)) => {}
                    _ = cancelled(cancel) => return Err("Aborted".into()),
                }
                continue 'summary;
            }
            Err(error) => return Err(error.message),
        };
        let mut failure: Option<String> = None;
        let mut finished = false;
        while let Some(event) = tokio::select! {
            event = stream.next() => event,
            _ = cancelled(cancel) => return Err("Aborted".into()),
        } {
            match event {
                ProviderEvent::TextDelta(delta) => summary.push_str(&delta),
                ProviderEvent::Finished { .. } => {
                    finished = true;
                    break;
                }
                ProviderEvent::Error(error) => {
                    failure = Some(error);
                    break;
                }
                _ => {}
            }
        }
        drop(stream);
        // A stream that never completed holds a partial summary: discard it and
        // re-request, exactly like the main loop does.
        let error = match failure {
            Some(error) => error,
            None if finished => break 'summary,
            None => "provider compaction stream ended before a completion event".to_string(),
        };
        if provider::stream_retryable(&error) && attempt < MAX_COMPACTION_RETRIES {
            attempt += 1;
            tokio::select! {
                _ = tokio::time::sleep(retry_delay(None, attempt)) => {}
                _ = cancelled(cancel) => return Err("Aborted".into()),
            }
            continue 'summary;
        }
        return Err(error);
    }
    let text = summary.trim();
    if text.is_empty() {
        return Err("provider returned an empty compaction summary".into());
    }
    let now = now_ms();
    let mid = hex_id("msg_");
    let mut part = json!({"id": hex_id("prt_"), "sessionID": sid, "messageID": mid, "type": "text", "text": text, "time": {"start": now, "end": now}});
    part["compactionRecent"] = Value::Array(recent);
    if current.time.completed.is_none() {
        // Overflow recovery finishes this already-created response after the
        // checkpoint. Its final text/results must remain visible on later turns.
        part["compactionPending"] = json!(current.id);
    }
    let info = MessageInfo {
        id: mid,
        session_id: sid.into(),
        role: "assistant".into(),
        time: MessageTime {
            created: now,
            completed: Some(now),
        },
        parent_id: current.parent_id.clone(),
        model_id: current.model_id.clone(),
        provider_id: current.provider_id.clone(),
        agent: Some("compaction".into()),
        mode: Some("compaction".into()),
        cost: Some(0.0),
        tokens: Some(Tokens::default()),
        summary: Some(true),
        ..Default::default()
    };
    app.store.add_message(&MessageWithParts {
        info: info.clone(),
        parts: vec![part.clone()],
    })?;
    emit_message(&info);
    emit_part(&part);
    bus().emit("session.compacted", json!({"sessionID": sid}));
    Ok(())
}

async fn run_turn(
    app: App,
    engine: Arc<dyn EngineApi>,
    sid: &str,
    request: PromptRequest,
    cancel: Arc<AtomicBool>,
) -> Result<Value, String> {
    let session = app.store.get_session(sid)?.ok_or("Session not found")?;
    let app = app.for_directory(&session.directory)?;
    let uid = request
        .message_id
        .as_deref()
        .ok_or("Admitted prompt has no messageID")?;
    let model = request
        .model
        .as_ref()
        .ok_or("Admitted prompt has no model")?;
    let mut cfg = app.config.lock().unwrap().clone();
    if cfg
        .provider(&model.provider_id)
        .is_none_or(|provider| provider.models.is_empty())
    {
        let (discovered, errors) = tokio::select! {
            result = provider::discover_models(&cfg, &app.auth) => result,
            _ = cancelled(&cancel) => return Err("Aborted".into()),
        };
        for error in errors {
            eprintln!("ocmini: {error}");
        }
        cfg = discovered;
    }
    let agent = request.agent.as_deref().unwrap_or("build");
    let compact = request.command.as_deref() == Some("__ocmini_compact");
    let read_only = ["plan", "explore"].contains(&agent);
    tokio::select! {
        _ = app.wait_mcp_ready() => {},
        _ = cancelled(&cancel) => return Err("Aborted".into()),
    }
    let mut system = tokio::select! {
        result=system_prompt(&cfg,&session.directory,agent,request.system.as_deref())=>result?,
        _=cancelled(&cancel)=>return Err("Aborted".into()),
    };
    if read_only {
        system.push_str("\n\nThis is a read-only analysis session. Use the supplied reading and search tools. Do not modify files or request execution tools.");
    }
    if compact {
        system.push_str("\n\n");
        system.push_str(COMPACTION_PROMPT);
    }
    if request.command.is_some() && !compact {
        expand_shell_templates(&app, engine.clone(), &session, &request, &cfg, &cancel).await?;
    }
    let adapter = provider::build(
        &model.provider_id,
        cfg.provider(&model.provider_id),
        &app.auth,
    );
    // Chat adapters replay chain of thought once per turn, so history is
    // merged per turn there. Responses and Anthropic rebuild each step from
    // their own opaque payloads and need the per-step shape kept.
    let protocol_probe = ChatRequest {
        model: model.model_id.clone(),
        system: String::new(),
        messages: Vec::new(),
        tools: Vec::new(),
        variant: request.variant.clone(),
        session_id: Some(sid.into()),
    };
    let chat = tokio::select! {
        result = adapter.protocol_of(&protocol_probe) => matches!(result, Ok(provider::Protocol::Chat)),
        _ = cancelled(&cancel) => return Err("Aborted".into()),
    };
    let mut defs = tools::tool_defs();
    defs.extend(tokio::select! {
        value = app.mcp.tool_defs_async() => value,
        _ = cancelled(&cancel) => return Err("Aborted".into()),
    });
    defs.retain(|def| {
        let name = def["function"]["name"].as_str().unwrap_or_default();
        !compact
            && tool_enabled(name, agent, &cfg, &request)
            && !(read_only && (execution_tool(name) || app.mcp.route(name).is_some()))
    });
    let allowed: HashSet<String> = defs
        .iter()
        .filter_map(|d| d["function"]["name"].as_str().map(str::to_string))
        .collect();
    let max_steps = cfg
        .agent
        .get(agent)
        .and_then(|a| a["steps"].as_u64().or_else(|| a["maxSteps"].as_u64()))
        .map(|steps| steps.max(1));
    let mut step = 0u64;
    let mut recent_calls = VecDeque::new();
    let mut compaction = CompactionPolicy::for_model(&cfg, model);
    // A turn runs until completion, steering or cancellation unless the user
    // explicitly configured an agent step budget.
    loop {
        step = step.saturating_add(1);
        if cancel.load(Ordering::Acquire) {
            return Err("Aborted".into());
        }
        // History reloads can span megabytes of JSON; keep them off the async workers.
        let (blocking_store, blocking_sid, blocking_uid, blocking_chat) =
            (app.store.clone(), sid.to_string(), uid.to_string(), chat);
        let (mut messages, mut has_summary) = tokio::task::spawn_blocking(move || {
            build_messages(&blocking_store, &blocking_sid, &blocking_uid, blocking_chat)
        })
        .await
        .map_err(|e| e.to_string())??;
        let mut recent = Vec::new();
        if compact {
            (messages, recent) = compaction_messages(messages, has_summary, &compaction);
            messages.push(json!({"role":"user","content":"Produce the continuation summary now."}));
        }
        let now = now_ms();
        let amid = hex_id("msg_");
        let mut info = MessageInfo {
            id: amid.clone(),
            session_id: sid.into(),
            role: "assistant".into(),
            time: MessageTime {
                created: now,
                completed: None,
            },
            parent_id: Some(uid.into()),
            model_id: Some(model.model_id.clone()),
            provider_id: Some(model.provider_id.clone()),
            agent: Some(agent.into()),
            mode: Some(agent.into()),
            path: Some(json!({"cwd":session.directory,"root":session.directory})),
            cost: Some(0.0),
            tokens: Some(Tokens::default()),
            summary: compact.then_some(true),
            ..Default::default()
        };
        let start_part =
            json!({"id":hex_id("prt_"),"sessionID":sid,"messageID":amid,"type":"step-start"});
        app.store.add_message(&MessageWithParts {
            info: info.clone(),
            parts: vec![start_part.clone()],
        })?;
        emit_message(&info);
        emit_part(&start_part);
        const MAX_RETRIES: u32 = 5;
        let mut attempt: u32 = 0;
        // Compaction may repeat while it reduces an overflowing request. Stop
        // when a checkpoint makes no progress, rather than after a fixed count.
        let mut previous_overflow_bytes = None;
        let mut req = ChatRequest {
            model: model.model_id.clone(),
            system: system.clone(),
            messages,
            tools: defs.clone(),
            variant: request.variant.clone(),
            session_id: Some(sid.into()),
        };
        let (mut text_part, mut tool_parts, metadata, finished) = 'attempt: loop {
            let mut text_part: Option<StreamedPart> = None;
            let mut reasoning_part: Option<StreamedPart> = None;
            let mut tool_parts: Vec<Value> = Vec::new();
            let mut metadata = Value::Null;
            let mut finished = None;
            let mut failure: Option<(String, bool)> = None; // (message, mid-stream)
            let opened = tokio::select! {
                result = adapter.stream(&req) => result,
                _ = cancelled(&cancel) => Err(crate::provider::ProviderError::fatal("Aborted")),
            };
            match opened {
                Ok(mut stream) => {
                    loop {
                        let event = tokio::select! {
                            event = stream.next() => event,
                            _ = cancelled(&cancel) => { failure = Some(("Aborted".to_string(), false)); break; }
                        };
                        let Some(event) = event else {
                            break;
                        };
                        match event {
                            ProviderEvent::TextDelta(delta) => {
                                append_delta(&app, sid, &amid, "text", &mut text_part, &delta)?
                            }
                            ProviderEvent::ReasoningDelta(delta) => append_delta(
                                &app,
                                sid,
                                &amid,
                                "reasoning",
                                &mut reasoning_part,
                                &delta,
                            )?,
                            ProviderEvent::Metadata(value) => metadata = value,
                            ProviderEvent::ToolCallStart {
                                call_id,
                                name,
                                args,
                            } => {
                                if call_id.is_empty()
                                    || name.is_empty()
                                    || tool_parts.iter().any(|p| p["callID"] == call_id)
                                {
                                    failure = Some(("Provider returned a missing or duplicate tool call identifier".to_string(), false));
                                    break;
                                }
                                let input = serde_json::from_str::<Value>(&args);
                                let state = match input {
                                    Ok(input) if input.is_object() => {
                                        json!({"status":"pending","input":input,"raw":args})
                                    }
                                    _ => {
                                        json!({"status":"error","input":{},"error":"Provider returned invalid JSON tool arguments","time":{"start":now_ms(),"end":now_ms()}})
                                    }
                                };
                                let part = json!({"id":hex_id("prt_"),"sessionID":sid,"messageID":amid,"type":"tool","tool":name,"callID":call_id,"state":state});
                                app.store.update_part(sid, &part)?;
                                emit_part(&part);
                                tool_parts.push(part);
                            }
                            ProviderEvent::Finished { finish, usage } => {
                                finished = Some((finish, usage))
                            }
                            ProviderEvent::Error(error) => {
                                failure = Some((error, true));
                                break;
                            }
                        }
                    }
                    drop(stream);
                    for part in [&mut text_part, &mut reasoning_part].into_iter().flatten() {
                        part.part["time"]["end"] = json!(now_ms());
                        app.store.update_part(sid, &part.part)?;
                        emit_part(&part.part);
                    }
                    if failure.is_none() && finished.is_none() {
                        failure = Some((
                            "Provider stream ended before a completion event".to_string(),
                            true,
                        ));
                    }
                    let (error, mid_stream) = match failure {
                        Some(failure) => failure,
                        None => break 'attempt (text_part, tool_parts, metadata, finished),
                    };
                    // Flaky connections kill streams mid-body, usually right after
                    // headers arrive. Nothing executed yet, so a fresh request is safe.
                    let transient = mid_stream
                        && tool_parts.is_empty()
                        && provider::stream_retryable(&error)
                        && attempt < MAX_RETRIES
                        && !cancel.load(Ordering::Acquire);
                    if transient {
                        for part in [&mut text_part, &mut reasoning_part].into_iter().flatten() {
                            part.part["text"] = json!("");
                            app.store.update_part(sid, &part.part)?;
                            emit_part(&part.part);
                        }
                        attempt += 1;
                        let delay = retry_delay(None, attempt);
                        update_status(
                            &app,
                            sid,
                            json!({"type":"retry","attempt":attempt,"message":error,"next":now_ms()+delay.as_millis() as i64}),
                        );
                        tokio::select! { _=tokio::time::sleep(delay)=>{}, _=cancelled(&cancel)=>{} }
                        if cancel.load(Ordering::Acquire) {
                            fail_message(&app, &mut info, "Aborted", true)?;
                            return Err("Aborted".into());
                        }
                        update_status(&app, sid, json!({"type":"busy"}));
                        continue 'attempt;
                    }
                    for part in &mut tool_parts {
                        if ["pending", "running"]
                            .contains(&part["state"]["status"].as_str().unwrap_or_default())
                        {
                            part["state"] = json!({"status":"error","input":part["state"]["input"],"error":error,"time":{"start":now_ms(),"end":now_ms()}});
                            app.store.update_part(sid, part)?;
                            emit_part(part);
                        }
                    }
                    fail_message(&app, &mut info, &error, cancel.load(Ordering::Acquire))?;
                    return Err(error);
                }
                Err(error)
                    if error.retryable
                        && attempt < MAX_RETRIES
                        && !cancel.load(Ordering::Acquire) =>
                {
                    attempt += 1;
                    let delay = retry_delay(error.retry_after, attempt);
                    update_status(
                        &app,
                        sid,
                        json!({"type":"retry","attempt":attempt,"message":error.message,"next":now_ms()+delay.as_millis() as i64}),
                    );
                    tokio::select! { _=tokio::time::sleep(delay)=>{}, _=cancelled(&cancel)=>{} }
                    if cancel.load(Ordering::Acquire) {
                        fail_message(&app, &mut info, "Aborted", true)?;
                        return Err("Aborted".into());
                    }
                    update_status(&app, sid, json!({"type":"busy"}));
                }
                Err(error) => {
                    // A context overflow can never succeed verbatim: compact
                    // the history to the provider's stated window and replay
                    // this step against the summary.
                    let request_bytes = req
                        .messages
                        .iter()
                        .map(|message| message.to_string().len())
                        .sum::<usize>();
                    let mut message = error.message;
                    if error.overflow
                        && !compact
                        && compaction.settings.automatic()
                        && previous_overflow_bytes.is_none_or(|previous| request_bytes < previous)
                        && !cancel.load(Ordering::Acquire)
                    {
                        previous_overflow_bytes = Some(request_bytes);
                        if let Some(parsed) = provider::parse_context_limit(&message) {
                            compaction.context = Some(parsed);
                        }
                        match compact_history(
                            &app,
                            adapter.as_ref(),
                            &info,
                            std::mem::take(&mut req.messages),
                            has_summary,
                            &compaction,
                            &cancel,
                        )
                        .await
                        {
                            Ok(()) => {
                                let (store, sid_text, uid_text) =
                                    (app.store.clone(), sid.to_string(), uid.to_string());
                                (req.messages, has_summary) =
                                    tokio::task::spawn_blocking(move || {
                                        build_messages(&store, &sid_text, &uid_text, chat)
                                    })
                                    .await
                                    .map_err(|e| e.to_string())??;
                                if compact {
                                    req.messages.push(json!({"role":"user","content":"Produce the continuation summary now."}));
                                }
                                continue 'attempt;
                            }
                            Err(compact_error) => {
                                // The server's stderr is invisible to the TUI, so
                                // the reason recovery gave up must reach the user.
                                eprintln!("ocmini: auto-compaction failed: {compact_error}");
                                message = format!(
                                    "{} (auto-compaction failed: {compact_error})",
                                    message
                                );
                            }
                        }
                    }
                    fail_message(&app, &mut info, &message, cancel.load(Ordering::Acquire))?;
                    return Err(message);
                }
            }
        };
        drop(req);
        let (finish, usage) = finished.ok_or("Provider stream did not complete")?;
        let cost = cost_for(&cfg, model, &usage);
        info.tokens = Some(usage.clone());
        info.cost = Some(cost);
        if !metadata.is_null() {
            info.extra
                .insert("providerMetadata".into(), metadata.clone());
            app.store.add_message(&MessageWithParts {
                info: info.clone(),
                parts: vec![],
            })?;
        }
        for part in &mut tool_parts {
            if part["state"]["status"] == "error" {
                continue;
            }
            let name = part["tool"].as_str().unwrap_or_default().to_string();
            let input = part["state"]["input"].clone();
            let start = now_ms();
            part["state"] = json!({"status":"running","input":input,"time":{"start":start}});
            app.store.update_part(sid, part)?;
            emit_part(part);
            let ctx = ToolCtx {
                dir: session.directory.clone(),
                app: app.clone(),
                session_id: sid.into(),
                engine: engine.clone(),
                cancel: cancel.clone(),
                message_id: amid.clone(),
                call_id: part["callID"].as_str().unwrap_or_default().into(),
                part_id: part["id"].as_str().unwrap_or_default().into(),
            };
            let signature = format!("{name}:{}", input);
            recent_calls.push_back(signature.clone());
            if recent_calls.len() > 3 {
                recent_calls.pop_front();
            }
            let repeated = recent_calls.len() == 3 && recent_calls.iter().all(|s| s == &signature);
            let output = if !allowed.contains(&name) {
                Err(ToolError {
                    message: format!("Tool '{name}' is not enabled for agent '{agent}'"),
                })
            } else {
                tokio::select! {
                    result=execute_tool(&ctx,&name,&input,&cfg,&request,repeated)=>result,
                    _=cancelled(&cancel)=>Err(ToolError{message:"Aborted".into()}),
                }
            };
            // Task/question tools may publish metadata while blocked. Preserve it.
            let existing = app
                .store
                .message(sid, &amid)
                .ok()
                .flatten()
                .and_then(|m| m.parts.into_iter().find(|p| p["id"] == part["id"]));
            let running_metadata = existing
                .as_ref()
                .map(|p| p["state"]["metadata"].clone())
                .unwrap_or_else(|| json!({}));
            part["state"] = match output {
                Ok(out) => {
                    json!({"status":"completed","input":input,"output":out.output,"title":out.title,"metadata":out.metadata,"time":{"start":start,"end":now_ms()}})
                }
                Err(error) => {
                    json!({"status":"error","input":input,"error":error.message,"metadata":running_metadata,"time":{"start":start,"end":now_ms()}})
                }
            };
            app.store.update_part(sid, part)?;
            emit_part(part);
            if cancel.load(Ordering::Acquire) {
                break;
            }
        }
        if cancel.load(Ordering::Acquire) {
            for part in &mut tool_parts {
                if ["pending", "running"]
                    .contains(&part["state"]["status"].as_str().unwrap_or_default())
                {
                    part["state"] = json!({"status":"error","input":part["state"]["input"],"error":"Aborted","time":{"start":now_ms(),"end":now_ms()}});
                    app.store.update_part(sid, part)?;
                    emit_part(part);
                }
            }
            fail_message(&app, &mut info, "Aborted", true)?;
            return Err("Aborted".into());
        }
        let finish = match finish.as_str() {
            "tool_calls" | "tool_use" => "tool-calls",
            "end_turn" => "stop",
            other => other,
        }
        .to_string();
        let mut step_part = json!({"id":hex_id("prt_"),"sessionID":sid,"messageID":amid,"type":"step-finish","reason":finish,"cost":cost,"tokens":usage});
        if !metadata.is_null() {
            step_part["providerMetadata"] = metadata;
        }
        app.store.update_part(sid, &step_part)?;
        emit_part(&step_part);
        info.time.completed = Some(now_ms().max(info.time.created));
        info.finish = Some(finish);
        if compact {
            if let Some(part) = &mut text_part {
                part.part["compactionRecent"] = Value::Array(recent);
                app.store.update_part(sid, &part.part)?;
                emit_part(&part.part);
            }
        }
        app.store.add_message(&MessageWithParts {
            info: info.clone(),
            parts: vec![],
        })?;
        app.usage
            .record(&amid, sid, &model.provider_id, &model.model_id, &usage, now);
        record_session_usage(&app, sid, &usage, cost)?;
        emit_message(&info);
        let response = app
            .store
            .message(sid, &amid)?
            .ok_or("Persisted response not found")?;
        let response = serde_json::to_value(response).map_err(|e| e.to_string())?;
        if compact {
            if text_part
                .as_ref()
                .and_then(|p| p.part["text"].as_str())
                .is_none_or(|s| s.trim().is_empty())
            {
                fail_message(
                    &app,
                    &mut info,
                    "Provider returned an empty compaction summary",
                    false,
                )?;
                return Err("Provider returned an empty compaction summary".into());
            }
            bus().emit("session.compacted", json!({"sessionID":sid}));
            return Ok(response);
        }
        // Compacting only after the window is full burns a failed request;
        // when the window is known, compact as the step approaches it.
        let used =
            usage.input + usage.output + usage.reasoning + usage.cache.read + usage.cache.write;
        if compaction.settings.automatic()
            && compaction
                .usable()
                .is_some_and(|limit| u64::try_from(used).unwrap_or(0) >= limit)
        {
            let (store, sid_text, uid_text) = (app.store.clone(), sid.to_string(), uid.to_string());
            match tokio::task::spawn_blocking(move || {
                build_messages(&store, &sid_text, &uid_text, chat)
            })
            .await
            {
                Ok(Ok((history, has_summary))) => {
                    if let Err(error) = compact_history(
                        &app,
                        adapter.as_ref(),
                        &info,
                        history,
                        has_summary,
                        &compaction,
                        &cancel,
                    )
                    .await
                    {
                        eprintln!("ocmini: proactive compaction failed: {error}");
                    }
                }
                Ok(Err(error)) => eprintln!("ocmini: proactive compaction rebuild failed: {error}"),
                Err(error) => eprintln!("ocmini: proactive compaction rebuild failed: {error}"),
            }
        }
        if tool_parts.is_empty() || app.store.has_waiting(sid)? {
            if compaction.settings.prune == Some(true) {
                let (store, sid_text, uid_text) =
                    (app.store.clone(), sid.to_string(), uid.to_string());
                let pruned = tokio::select! {
                    result = tokio::task::spawn_blocking(move || store.prune_tool_outputs(&sid_text, &uid_text)) => result,
                    _ = cancelled(&cancel) => return Err("Aborted".into()),
                };
                match pruned {
                    Ok(Ok(parts)) => {
                        for part in parts {
                            emit_part(&part);
                        }
                    }
                    Ok(Err(error)) => eprintln!("ocmini: tool-output pruning failed: {error}"),
                    Err(error) => eprintln!("ocmini: tool-output pruning failed: {error}"),
                }
            }
            return Ok(response);
        }
        if max_steps.is_some_and(|limit| step >= limit) {
            let message=format!("Agent step limit ({step}) reached. Send another message to continue from the saved tool results.");
            emit_error(sid, &message, false);
            return Err(message);
        }
    }
}

/// Streaming parts persist to SQLite at most every flush interval; deltas are
/// announced on the bus immediately. Crash recovery marks the interrupted
/// message, so the bounded persistence lag never presents partial text as done.
struct StreamedPart {
    part: Value,
    last_flush: std::time::Instant,
}

const STREAM_FLUSH: std::time::Duration = std::time::Duration::from_millis(250);

fn append_delta(
    app: &App,
    sid: &str,
    mid: &str,
    kind: &str,
    current: &mut Option<StreamedPart>,
    delta: &str,
) -> Result<(), String> {
    if delta.is_empty() {
        return Ok(());
    }
    if current.is_none() {
        let part = json!({"id":hex_id("prt_"),"sessionID":sid,"messageID":mid,"type":kind,"text":"","time":{"start":now_ms()}});
        app.store.update_part(sid, &part)?;
        emit_part(&part);
        *current = Some(StreamedPart {
            part,
            last_flush: std::time::Instant::now(),
        });
    }
    let entry = current.as_mut().ok_or("Missing streamed part")?;
    if let Value::String(text) = &mut entry.part["text"] {
        text.push_str(delta);
    }
    bus().emit("message.part.delta",json!({"sessionID":sid,"messageID":mid,"partID":entry.part["id"],"field":"text","delta":delta}));
    if entry.last_flush.elapsed() >= STREAM_FLUSH {
        app.store.update_part(sid, &entry.part)?;
        entry.last_flush = std::time::Instant::now();
    }
    Ok(())
}

fn retry_delay(retry_after: Option<std::time::Duration>, attempt: u32) -> std::time::Duration {
    let base = std::env::var("OCMINI_RETRY_BASE_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(1000);
    let base = std::time::Duration::from_millis(base);
    let backoff = base * (1u32 << attempt.min(6));
    let delay = retry_after
        .unwrap_or(backoff)
        .min(std::time::Duration::from_secs(120));
    let jitter =
        std::time::Duration::from_millis(rand::random::<u64>() % (base.as_millis() as u64 / 4 + 1));
    delay + jitter
}

fn fail_message(
    app: &App,
    info: &mut MessageInfo,
    error: &str,
    aborted: bool,
) -> Result<(), String> {
    info.time.completed = Some(now_ms().max(info.time.created));
    info.error = Some(error_value(error, aborted));
    app.store.add_message(&MessageWithParts {
        info: info.clone(),
        parts: vec![],
    })?;
    emit_message(info);
    Ok(())
}

fn cost_for(cfg: &Config, model: &ModelRef, usage: &Tokens) -> f64 {
    let Some(info) = cfg
        .provider(&model.provider_id)
        .and_then(|p| p.models.get(&model.model_id))
    else {
        return 0.0;
    };
    let rates = info.extra.get("cost").cloned().unwrap_or_default();
    let price = |name: &str| {
        rates[name]
            .as_f64()
            .filter(|n| n.is_finite() && *n >= 0.0)
            .unwrap_or(0.0)
    };
    (usage.input as f64 * price("input")
        + (usage.output + usage.reasoning) as f64 * price("output")
        + usage.cache.read as f64 * price("cache_read")
        + usage.cache.write as f64 * price("cache_write"))
        / 1_000_000.0
}

fn record_session_usage(app: &App, sid: &str, usage: &Tokens, cost: f64) -> Result<(), String> {
    let Some(mut session) = app.store.get_session(sid)? else {
        return Ok(());
    };
    session.cost += cost;
    session.tokens.input += usage.input;
    session.tokens.output += usage.output;
    session.tokens.reasoning += usage.reasoning;
    session.tokens.cache.read += usage.cache.read;
    session.tokens.cache.write += usage.cache.write;
    session.time.updated = Some(now_ms());
    app.store.update_session(&session)?;
    bus().emit("session.updated", json!({"sessionID":sid,"info":session}));
    Ok(())
}

/// One turn of assistant output, merged into the single assistant message
/// OpenCode's Chat Completions converter sends: every step's chain of thought
/// in order, every step's tool calls, then the tool results.
#[derive(Default)]
struct ChatRun {
    texts: Vec<String>,
    calls: Vec<Value>,
    reasoning: Vec<(String, String)>,
    details: Vec<Value>,
    tail: Vec<Value>,
}

fn flush_chat_run(out: &mut Vec<Value>, run: Option<ChatRun>) {
    let Some(run) = run else { return };
    let mut message = json!({"role":"assistant"});
    let content = run.texts.join("\n");
    if !content.is_empty() {
        message["content"] = json!(content);
    }
    if !run.calls.is_empty() {
        message["tool_calls"] = json!(run.calls);
    }
    if !run.reasoning.is_empty() || !run.details.is_empty() {
        let mut meta_message = json!({"role":"assistant"});
        let mut keys: Vec<String> = Vec::new();
        let mut joined: HashMap<String, Vec<String>> = HashMap::new();
        for (key, text) in run.reasoning {
            if !joined.contains_key(&key) {
                keys.push(key.clone());
            }
            joined.entry(key).or_default().push(text);
        }
        for key in keys {
            meta_message[&key] = json!(joined[&key].join("\n"));
        }
        if !run.details.is_empty() {
            meta_message["reasoning_details"] = json!(run.details);
        }
        message["providerMetadata"] = json!({"provider":"openai-chat","message":meta_message});
    }
    if message.get("content").is_none()
        && message.get("tool_calls").is_none()
        && message.get("providerMetadata").is_none()
    {
        return;
    }
    out.push(message);
    out.extend(run.tail);
}

fn build_messages(
    store: &Store,
    sid: &str,
    through: &str,
    chat: bool,
) -> Result<(Vec<Value>, bool), String> {
    let mut history = store.context_messages(sid, through)?;
    let summary = history.iter().rposition(|m| {
        m.info.summary == Some(true)
            && m.info.time.completed.is_some()
            && m.info.error.is_none()
            && m.parts.iter().any(|p| {
                p["type"] == "text" && p["text"].as_str().is_some_and(|s| !s.trim().is_empty())
            })
    });
    let mut out = Vec::new();
    let mut run: Option<ChatRun> = None;
    let start = if let Some(index) = summary {
        let text = history[index]
            .parts
            .iter()
            .filter(|p| p["type"] == "text")
            .filter_map(|p| p["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n");
        out.push(json!({"role":"user","content":format!("Continuation summary of the earlier conversation:\n{text}")}));
        for part in &mut history[index].parts {
            if let Some(value) = part.get_mut("compactionRecent") {
                if let Value::Array(recent) = value.take() {
                    out.extend(recent);
                }
            }
        }
        index + 1
    } else {
        0
    };
    for message in history.into_iter().skip(start) {
        if message.info.role == "user" {
            flush_chat_run(&mut out, run.take());
            let mut content = Vec::new();
            for part in &message.parts {
                if part["ignored"] == true {
                    continue;
                }
                match part["type"].as_str() {
                    Some("text")=>content.push(json!({"type":"text","text":part["text"]})),
                    Some("file")=>content.push(file_content(part)?),
                    Some("agent")=>content.push(json!({"type":"text","text":format!("Requested agent: {}",part["name"].as_str().unwrap_or_default())})),
                    _=>{},
                }
            }
            if content.is_empty() {
                continue;
            }
            let content = if content.iter().all(|p| p["type"] == "text") {
                json!(content
                    .iter()
                    .filter_map(|p| p["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("\n"))
            } else {
                json!(content)
            };
            out.push(json!({"role":"user","content":content}));
            continue;
        }
        if message.info.role != "assistant" {
            continue;
        }
        let text = message
            .parts
            .iter()
            .filter(|p| p["type"] == "text")
            .filter_map(|p| p["text"].as_str())
            .collect::<Vec<_>>()
            .join("");
        let calls: Vec<_> = message
            .parts
            .iter()
            .filter(|p| p["type"] == "tool")
            .collect();
        let metadata = message
            .parts
            .iter()
            .rev()
            .find_map(|p| p.get("providerMetadata"))
            .or_else(|| message.info.extra.get("providerMetadata"))
            .filter(|value| !value.is_null())
            .cloned();
        let mut attachments = Vec::new();
        let mut tail: Vec<Value> = Vec::new();
        for part in &calls {
            let content = match part["state"]["status"].as_str() {
                Some("completed") if part["state"]["time"]["compacted"].is_number() => {
                    "[Old tool result content cleared]".into()
                }
                Some("completed") => part["state"]["output"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
                Some("error") => format!(
                    "Tool failed: {}",
                    part["state"]["error"]
                        .as_str()
                        .unwrap_or("Unknown tool error")
                ),
                _ => "Tool execution was interrupted before a result was recorded.".into(),
            };
            tail.push(json!({"role":"tool","tool_call_id":part["callID"],"content":content,"is_error":part["state"]["status"]!="completed"}));
            if let Some(blocks) = part["state"]["metadata"]["attachments"]
                .as_array()
                .filter(|_| !part["state"]["time"]["compacted"].is_number())
            {
                for block in blocks {
                    match block["type"].as_str() {
                        Some("image") => {
                            let mime = block["mimeType"].as_str().unwrap_or("image/png");
                            let data = block["data"].as_str().ok_or("MCP image has no data")?;
                            attachments.push(json!({"type":"image_url","image_url":{"url":format!("data:{mime};base64,{data}")}}));
                        }
                        Some("audio") => {
                            let format=match block["mimeType"].as_str().unwrap_or_default() { "audio/wav"|"audio/x-wav"=>"wav","audio/mpeg"|"audio/mp3"=>"mp3",mime=>return Err(format!("MCP audio format '{mime}' cannot be sent to the configured chat adapters")) };
                            attachments.push(json!({"type":"input_audio","input_audio":{"data":block["data"],"format":format}}));
                        }
                        Some("resource") => {
                            let resource = &block["resource"];
                            let mime = resource["mimeType"]
                                .as_str()
                                .unwrap_or("application/octet-stream");
                            if let Some(blob) = resource["blob"].as_str() {
                                attachments.push(file_content(&json!({"mime":mime,"filename":resource["uri"],"url":format!("data:{mime};base64,{blob}")}))?);
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        if !attachments.is_empty() {
            tail.push(json!({"role":"user","content":attachments}));
        }
        if chat {
            let run = run.get_or_insert_with(ChatRun::default);
            if !text.is_empty() {
                run.texts.push(text);
            }
            for part in &calls {
                run.calls.push(json!({"id":part["callID"],"type":"function","function":{"name":part["tool"],"arguments":part["state"]["input"].to_string()}}));
            }
            if let Some(meta) = metadata
                .as_ref()
                .filter(|m| m["provider"] == "openai-chat")
                .and_then(|m| m.get("message"))
            {
                for key in ["reasoning_content", "reasoning", "reasoning_text"] {
                    if let Some(value) = meta[key].as_str() {
                        if !value.is_empty() {
                            run.reasoning.push((key.to_string(), value.to_string()));
                        }
                    }
                }
                if let Some(list) = meta["reasoning_details"].as_array() {
                    run.details.extend(list.iter().cloned());
                }
            }
            run.tail.extend(tail);
            continue;
        }
        if text.is_empty() && calls.is_empty() && metadata.is_none() {
            continue;
        }
        let mut value =
            json!({"role":"assistant","content":if text.is_empty(){Value::Null}else{json!(text)}});
        if !calls.is_empty() {
            value["tool_calls"]=json!(calls.iter().map(|p|json!({"id":p["callID"],"type":"function","function":{"name":p["tool"],"arguments":p["state"]["input"].to_string()}})).collect::<Vec<_>>());
        }
        if let Some(metadata) = metadata {
            value["providerMetadata"] = metadata;
        }
        out.push(value);
        out.extend(tail);
    }
    flush_chat_run(&mut out, run.take());
    Ok((out, summary.is_some()))
}

fn file_content(part: &Value) -> Result<Value, String> {
    let mime = part["mime"].as_str().ok_or("File part has no MIME type")?;
    let mut url = part["url"]
        .as_str()
        .ok_or("File part has no URL")?
        .to_string();
    let filename = part["filename"].as_str().unwrap_or("attachment");
    if url.starts_with("file:") {
        let path = reqwest::Url::parse(&url)
            .map_err(|e| format!("Invalid file URL: {e}"))?
            .to_file_path()
            .map_err(|_| "Invalid local file URL")?;
        let meta = std::fs::metadata(&path)
            .map_err(|e| format!("Cannot read attachment {}: {e}", path.display()))?;
        if meta.len() > 20 * 1024 * 1024 {
            return Err("Attachment exceeds 20 MiB".into());
        }
        let bytes = std::fs::read(&path).map_err(|e| e.to_string())?;
        url = format!(
            "data:{mime};base64,{}",
            base64::engine::general_purpose::STANDARD.encode(bytes)
        );
    }
    if mime.starts_with("image/") {
        return Ok(json!({"type":"image_url","image_url":{"url":url}}));
    }
    if (mime.starts_with("text/")
        || [
            "application/json",
            "application/xml",
            "application/javascript",
            "application/x-yaml",
        ]
        .contains(&mime))
        && url.starts_with("data:")
    {
        let (header, data) = url.split_once(',').ok_or("Malformed attachment data URL")?;
        let bytes = if header.ends_with(";base64") {
            base64::engine::general_purpose::STANDARD
                .decode(data)
                .map_err(|e| format!("Invalid attachment base64: {e}"))?
        } else {
            percent_bytes(data)?
        };
        if bytes.len() > 20 * 1024 * 1024 {
            return Err("Attachment exceeds 20 MiB".into());
        }
        let text = String::from_utf8(bytes).map_err(|_| "Text attachment is not UTF-8")?;
        return Ok(json!({"type":"text","text":format!("Attachment {filename}:\n{text}")}));
    }
    if !url.starts_with("data:") && !url.starts_with("https:") && !url.starts_with("http:") {
        return Err("Attachment URL must use data:, file:, http: or https:".into());
    }
    Ok(json!({"type":"file","file":{"file_data":url,"filename":filename}}))
}

fn percent_bytes(value: &str) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    let mut input = value.as_bytes().iter().copied();
    while let Some(byte) = input.next() {
        if byte != b'%' {
            bytes.push(byte);
            continue;
        }
        let a = input
            .next()
            .and_then(|b| (b as char).to_digit(16))
            .ok_or("Invalid percent escape")?;
        let b = input
            .next()
            .and_then(|b| (b as char).to_digit(16))
            .ok_or("Invalid percent escape")?;
        bytes.push((a * 16 + b) as u8);
    }
    Ok(bytes)
}

async fn system_prompt(
    cfg: &Config,
    directory: &str,
    agent: &str,
    provided: Option<&str>,
) -> Result<String, String> {
    let mut system = system_prompt_base(cfg, directory, agent).await?;
    if let Some(provided) = provided {
        system.push_str("\n\n");
        system.push_str(provided);
    }
    Ok(system)
}

/// The AGENTS.md/skills/agent-prompt base is identical for every turn in a
/// directory; reading those files per turn scales with sessions, so cache it.
async fn system_prompt_base(cfg: &Config, directory: &str, agent: &str) -> Result<String, String> {
    type PromptCache = Mutex<HashMap<(String, String), (std::time::Instant, String)>>;
    static CACHE: std::sync::OnceLock<PromptCache> = std::sync::OnceLock::new();
    let cache = CACHE.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()));
    let key = (directory.to_string(), agent.to_string());
    if let Some((created, prompt)) = cache.lock().unwrap().get(&key) {
        if created.elapsed() < std::time::Duration::from_secs(10) {
            return Ok(prompt.clone());
        }
    }
    let mut system=format!("You are ocmini, a coding agent. Work in {directory}. Read the code before changing it, follow project instructions, and report real tool results. Use available tools to complete the user's task. Operating system: {}.",std::env::consts::OS);
    let mut files = Vec::new();
    let global = PathBuf::from(crate::config::config_home()).join("opencode/AGENTS.md");
    if global.is_file() {
        files.push(global);
    }
    let mut ancestors = Vec::new();
    for parent in Path::new(directory).ancestors() {
        ancestors.push(parent.to_path_buf());
        if parent.join(".git").exists() {
            break;
        }
    }
    ancestors.reverse();
    for base in ancestors {
        if base.join("AGENTS.md").is_file() {
            files.push(base.join("AGENTS.md"));
        }
    }
    let mut seen = HashSet::new();
    for instruction in &cfg.instructions {
        if instruction.starts_with("https://") || instruction.starts_with("http://") {
            let response = provider::client()
                .get(instruction)
                .timeout(std::time::Duration::from_secs(15))
                .send()
                .await
                .map_err(|e| format!("Instruction URL {instruction}: {e}"))?
                .error_for_status()
                .map_err(|e| e.to_string())?;
            let mut bytes = Vec::new();
            let mut stream = response.bytes_stream();
            while let Some(chunk) = stream.next().await {
                bytes.extend_from_slice(&chunk.map_err(|e| e.to_string())?);
                if bytes.len() > 1024 * 1024 {
                    return Err("Instruction URL exceeds 1 MiB".into());
                }
            }
            system.push_str(&format!(
                "\n\n# Instructions from {instruction}\n{}",
                String::from_utf8(bytes).map_err(|_| "Instructions are not UTF-8")?
            ));
            continue;
        }
        let path = tools::resolve_path(directory, instruction);
        let pattern = path.to_string_lossy();
        for matched in
            glob::glob(&pattern).map_err(|e| format!("Instruction path {instruction}: {e}"))?
        {
            let path = matched.map_err(|e| e.to_string())?;
            if path.is_file() {
                files.push(path);
            }
        }
    }
    for path in files {
        let canonical = std::fs::canonicalize(&path).map_err(|e| e.to_string())?;
        if !seen.insert(canonical) {
            continue;
        }
        let contents = std::fs::read_to_string(&path)
            .map_err(|e| format!("Cannot read instructions {}: {e}", path.display()))?;
        system.push_str(&format!(
            "\n\n# Instructions from {}\n{}",
            path.display(),
            contents
        ));
    }
    if let Some(prompt) = cfg.agent.get(agent).and_then(|a| a["prompt"].as_str()) {
        system.push_str("\n\n");
        system.push_str(prompt);
    }
    let skills = tools::skill_list_with_paths(Some(directory), &cfg.skill_paths());
    if !skills.is_empty() {
        system.push_str(
            "\n\nAvailable skills (load their instructions with the skill tool when relevant):",
        );
        for skill in skills {
            system.push_str(&format!(
                "\n- {}: {}",
                skill["name"].as_str().unwrap_or_default(),
                skill["description"].as_str().unwrap_or_default()
            ));
        }
    }
    cache
        .lock()
        .unwrap()
        .insert(key, (std::time::Instant::now(), system.clone()));
    Ok(system)
}

fn execution_tool(name: &str) -> bool {
    ["bash", "write", "edit", "apply_patch", "task"].contains(&name) || name.starts_with("mcp_")
}
fn tool_enabled(name: &str, agent: &str, cfg: &Config, request: &PromptRequest) -> bool {
    if request.tools.as_ref().and_then(|t| t.get(name)) == Some(&false) {
        return false;
    }
    cfg.agent
        .get(agent)
        .and_then(|a| a["tools"].get(name))
        .and_then(Value::as_bool)
        != Some(false)
}

fn permission_rules(
    cfg: &Config,
    session: &Session,
    request: &PromptRequest,
) -> Vec<PermissionRule> {
    let mut rules = vec![
        PermissionRule {
            permission: "external_directory".into(),
            pattern: "*".into(),
            action: "ask".into(),
        },
        PermissionRule {
            permission: "doom_loop".into(),
            pattern: "*".into(),
            action: "ask".into(),
        },
        PermissionRule {
            permission: "read".into(),
            pattern: "*.env".into(),
            action: "ask".into(),
        },
        PermissionRule {
            permission: "read".into(),
            pattern: "*.env.*".into(),
            action: "ask".into(),
        },
    ];
    rules.extend(cfg.permission_rules());
    if let Some(permission) = cfg
        .agent
        .get(request.agent.as_deref().unwrap_or("build"))
        .and_then(|a| a.get("permission"))
    {
        let custom = Config {
            permission: permission.clone(),
            ..Config::default()
        };
        rules.extend(custom.permission_rules());
    }
    rules.extend(session.permission.clone().unwrap_or_default());
    rules
}

async fn execute_tool(
    ctx: &ToolCtx,
    name: &str,
    input: &Value,
    cfg: &Config,
    request: &PromptRequest,
    repeated: bool,
) -> Result<ToolOutput, ToolError> {
    let agent = request.agent.as_deref().unwrap_or("build");
    if !tool_enabled(name, agent, cfg, request)
        || (["plan", "explore"].contains(&agent)
            && (execution_tool(name) || ctx.app.mcp.route(name).is_some()))
    {
        return Err(ToolError {
            message: format!("Tool '{name}' is disabled for this request"),
        });
    }
    let session = ctx
        .app
        .store
        .get_session(&ctx.session_id)
        .map_err(|e| ToolError { message: e })?
        .ok_or_else(|| ToolError {
            message: "Session not found".into(),
        })?;
    let mut rules = permission_rules(cfg, &session, request);
    if let Some(always) = ctx.app.always.lock().unwrap().get(&ctx.session_id) {
        rules.extend(always.iter().cloned());
    }
    let paths = tool_paths(&ctx.dir, name, input).map_err(|e| ToolError { message: e })?;
    let mut permission_paths = paths.clone();
    let root = canonical_target(Path::new(&ctx.dir)).map_err(|e| ToolError { message: e })?;
    for path in &paths {
        let canonical = canonical_target(path).map_err(|e| ToolError { message: e })?;
        if !permission_paths.contains(&canonical) {
            permission_paths.push(canonical.clone());
        }
        if !canonical.starts_with(&root) {
            check_permission(
                ctx,
                &rules,
                "external_directory",
                &canonical.to_string_lossy(),
                name,
                input,
            )
            .await?;
        }
    }
    if repeated {
        check_permission(ctx, &rules, "doom_loop", name, name, input).await?;
    }
    let permission = match name {
        "write" | "edit" | "apply_patch" => "edit",
        other => other,
    };
    let patterns = match name {
        "bash" => vec![input["command"].as_str().unwrap_or_default().to_string()],
        "read" | "write" | "edit" | "apply_patch" => permission_paths
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect(),
        "glob" | "grep" => vec![input["pattern"].as_str().unwrap_or("*").to_string()],
        "webfetch" => vec![input["url"].as_str().unwrap_or_default().to_string()],
        "task" => vec![input["subagent_type"]
            .as_str()
            .unwrap_or("general")
            .to_string()],
        "skill" => vec![input["name"].as_str().unwrap_or_default().to_string()],
        _ => vec!["*".into()],
    };
    for pattern in patterns {
        check_permission(ctx, &rules, permission, &pattern, name, input).await?;
    }
    tools::run_tool(ctx, name, input).await
}

async fn check_permission(
    ctx: &ToolCtx,
    rules: &[PermissionRule],
    permission: &str,
    pattern: &str,
    tool: &str,
    input: &Value,
) -> Result<(), ToolError> {
    let denied = || ToolError {
        message: format!("Permission '{permission}' denied for {pattern}"),
    };
    match crate::permission::decide(rules, permission, pattern) {
        "allow" => return Ok(()),
        "deny" => return Err(denied()),
        _ => {}
    }
    let (req, rx) = ctx.app.perms.ask_for_tool(
        &ctx.session_id,
        permission,
        vec![pattern.into()],
        json!({"tool":tool,"input":input}),
        vec![pattern.into()],
        Some(json!({"messageID":ctx.message_id,"callID":ctx.call_id})),
    );
    bus().emit(
        "permission.asked",
        serde_json::to_value(&req).map_err(|e| ToolError {
            message: e.to_string(),
        })?,
    );
    let reply = tokio::select! { reply=rx=>reply.unwrap_or_else(|_|"reject".into()), _=cancelled(&ctx.cancel)=>"reject".into() };
    if reply == "reject" {
        ctx.app.perms.reply(&req.id, "reject");
        return Err(denied());
    }
    if reply == "always" {
        let rule = PermissionRule {
            permission: permission.into(),
            pattern: pattern.into(),
            action: "allow".into(),
        };
        ctx.app
            .always
            .lock()
            .unwrap()
            .entry(ctx.session_id.clone())
            .or_default()
            .push(rule.clone());
        if let Some(mut session) = ctx
            .app
            .store
            .get_session(&ctx.session_id)
            .map_err(|e| ToolError { message: e })?
        {
            session.permission.get_or_insert_with(Vec::new).push(rule);
            ctx.app
                .store
                .update_session(&session)
                .map_err(|e| ToolError { message: e })?;
        }
    }
    Ok(())
}

fn tool_paths(directory: &str, name: &str, input: &Value) -> Result<Vec<PathBuf>, String> {
    match name {
        "read" | "write" | "edit" => Ok(vec![tools::resolve_path(
            directory,
            input["filePath"].as_str().ok_or("Tool requires filePath")?,
        )]),
        "glob" | "grep" => Ok(vec![tools::resolve_path(
            directory,
            input["path"].as_str().unwrap_or("."),
        )]),
        "bash" => Ok(vec![tools::resolve_path(
            directory,
            input["workdir"]
                .as_str()
                .or_else(|| input["cwd"].as_str())
                .unwrap_or("."),
        )]),
        "apply_patch" => tools::patch::affected_paths(
            directory,
            input["patchText"]
                .as_str()
                .or_else(|| input["patch"].as_str())
                .ok_or("apply_patch requires patchText")?,
        ),
        _ => Ok(vec![]),
    }
}

fn canonical_target(path: &Path) -> Result<PathBuf, String> {
    let mut parent = path;
    let mut suffix = Vec::new();
    loop {
        match std::fs::canonicalize(parent) {
            Ok(mut canonical) => {
                for component in suffix.iter().rev() {
                    canonical.push(component);
                }
                return Ok(canonical);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                suffix.push(
                    parent
                        .file_name()
                        .ok_or_else(|| format!("Cannot resolve {}", path.display()))?
                        .to_os_string(),
                );
                parent = parent
                    .parent()
                    .ok_or_else(|| format!("Cannot resolve {}", path.display()))?;
            }
            Err(error) => return Err(format!("Cannot resolve {}: {error}", path.display())),
        }
    }
}

async fn expand_shell_templates(
    app: &App,
    engine: Arc<dyn EngineApi>,
    session: &Session,
    request: &PromptRequest,
    cfg: &Config,
    cancel: &Arc<AtomicBool>,
) -> Result<(), String> {
    let mid = request
        .message_id
        .as_deref()
        .ok_or("Command prompt has no messageID")?;
    let mut user = app
        .store
        .message(&session.id, mid)?
        .ok_or("Command prompt not found")?;
    let re = regex::Regex::new(r"!`([^`]+)`").map_err(|e| e.to_string())?;
    for part in &mut user.parts {
        if part["type"] != "text" {
            continue;
        }
        let original = part["text"].as_str().unwrap_or_default().to_string();
        let mut replaced = String::new();
        let mut offset = 0;
        for capture in re.captures_iter(&original) {
            let matched = capture.get(0).ok_or("Invalid shell template")?;
            let command = capture.get(1).ok_or("Empty shell template")?.as_str();
            if ["plan", "explore"].contains(&request.agent.as_deref().unwrap_or("build")) {
                return Err("Shell interpolation is disabled in read-only agents".into());
            }
            let ctx = ToolCtx {
                dir: session.directory.clone(),
                app: app.clone(),
                session_id: session.id.clone(),
                engine: engine.clone(),
                cancel: cancel.clone(),
                message_id: mid.into(),
                call_id: hex_id("call_"),
                part_id: part["id"].as_str().unwrap_or_default().into(),
            };
            let input = json!({"command":command,"timeout":120_000});
            let output = tokio::select! { result=execute_tool(&ctx,"bash",&input,cfg,request,false)=>result.map_err(|e|e.message)?, _=cancelled(cancel)=>return Err("Aborted".into()) };
            replaced.push_str(&original[offset..matched.start()]);
            replaced.push_str(&output.output);
            offset = matched.end();
        }
        replaced.push_str(&original[offset..]);
        part["text"] = json!(replaced);
    }
    app.store.add_message(&user)?;
    for part in user.parts {
        emit_part(&part);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn two_step_turn_store() -> (Store, String) {
        let store = Store::open(std::path::Path::new(":memory:"), "test").unwrap();
        let session = store.create_session_owned("steps", "/tmp");
        store
            .add_message(&MessageWithParts {
                info: MessageInfo {
                    id: "prompt".into(),
                    session_id: session.id.clone(),
                    role: "user".into(),
                    ..Default::default()
                },
                parts: vec![
                    json!({"id":"p1","messageID":"prompt","sessionID":session.id,
                                   "type":"text","text":"do the work"}),
                ],
            })
            .unwrap();
        // Step 1 answers with a tool call and its own chain of thought.
        store
            .add_message(&MessageWithParts {
                info: MessageInfo {
                    id: "step-1".into(),
                    session_id: session.id.clone(),
                    role: "assistant".into(),
                    ..Default::default()
                },
                parts: vec![
                    json!({"id":"t1","messageID":"step-1","sessionID":session.id,
                           "type":"tool","tool":"read","callID":"call_1",
                           "state":{"status":"completed","input":{"filePath":"note.txt"},
                                    "output":"the note"}}),
                    json!({"id":"f1","messageID":"step-1","sessionID":session.id,
                           "type":"step-finish",
                           "providerMetadata":{"provider":"openai-chat","message":{
                               "role":"assistant","content":null,
                               "reasoning_content":"first thinking",
                               "reasoning_details":[{"id":"rs_1","data":"one"}]}}}),
                ],
            })
            .unwrap();
        // Step 2 answers with the final text and its own chain of thought.
        store
            .add_message(&MessageWithParts {
                info: MessageInfo {
                    id: "step-2".into(),
                    session_id: session.id.clone(),
                    role: "assistant".into(),
                    ..Default::default()
                },
                parts: vec![
                    json!({"id":"x2","messageID":"step-2","sessionID":session.id,
                           "type":"text","text":"done"}),
                    json!({"id":"f2","messageID":"step-2","sessionID":session.id,
                           "type":"step-finish",
                           "providerMetadata":{"provider":"openai-chat","message":{
                               "role":"assistant","content":"done",
                               "reasoning_content":"second thinking",
                               "reasoning_details":[{"id":"rs_2","data":"two"}]}}}),
                ],
            })
            .unwrap();
        (store, session.id)
    }

    #[test]
    fn chat_requests_merge_the_turn_into_one_assistant_message() {
        let (store, sid) = two_step_turn_store();
        let (messages, _) = build_messages(&store, &sid, "prompt", true).unwrap();
        let assistants: Vec<&Value> = messages
            .iter()
            .filter(|m| m["role"] == "assistant")
            .collect();
        assert_eq!(assistants.len(), 1, "every step of the turn merges");
        let assistant = assistants[0];
        assert_eq!(assistant["content"], "done");
        assert_eq!(assistant["tool_calls"][0]["function"]["name"], "read");
        assert_eq!(
            assistant["providerMetadata"]["message"]["reasoning_content"],
            "first thinking\nsecond thinking"
        );
        assert_eq!(
            assistant["providerMetadata"]["message"]["reasoning_details"],
            json!([{"id":"rs_1","data":"one"},{"id":"rs_2","data":"two"}])
        );
        let assistant_index = messages
            .iter()
            .position(|m| m["role"] == "assistant")
            .unwrap();
        let tool_index = messages.iter().position(|m| m["role"] == "tool").unwrap();
        assert!(
            assistant_index < tool_index,
            "tool results follow the merged assistant message"
        );
        assert_eq!(messages[tool_index]["tool_call_id"], "call_1");
    }

    #[test]
    fn responses_history_keeps_one_assistant_message_per_step() {
        let (store, sid) = two_step_turn_store();
        let (messages, _) = build_messages(&store, &sid, "prompt", false).unwrap();
        let assistants: Vec<&Value> = messages
            .iter()
            .filter(|m| m["role"] == "assistant")
            .collect();
        assert_eq!(assistants.len(), 2, "one message per step is kept");
        assert_eq!(
            assistants[0]["providerMetadata"]["message"]["reasoning_content"],
            "first thinking"
        );
        assert_eq!(
            assistants[1]["providerMetadata"]["message"]["reasoning_content"],
            "second thinking"
        );
        assert_eq!(assistants[1]["content"], "done");
    }

    fn retention(bytes: u64) -> CompactionPolicy {
        CompactionPolicy {
            context: Some(bytes),
            settings: CompactionCfg {
                preserve_recent_tokens: Some(bytes / 4),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    #[test]
    fn compaction_retention_honors_turn_limits_and_explicit_zero_budgets() {
        let messages = vec![
            json!({"role":"user","content":"first user"}),
            json!({"role":"assistant","content":"first response"}),
            json!({"role":"user","content":"latest user"}),
            json!({"role":"assistant","tool_calls":[{"id":"latest_call","function":{"name":"read","arguments":"{}"}}]}),
            json!({"role":"tool","tool_call_id":"latest_call","content":"latest tool result"}),
        ];
        let mut policy = retention(4000);
        policy.settings.tail_turns = Some(1);
        let (older, recent) = compaction_messages(messages.clone(), false, &policy);
        assert_eq!(recent, messages[2..]);
        assert_eq!(older, messages[..2]);
        policy.settings.tail_turns = Some(0);
        assert!(compaction_messages(messages.clone(), false, &policy)
            .1
            .is_empty());
        policy.settings.tail_turns = None;
        policy.settings.preserve_recent_tokens = Some(0);
        let (older, recent) = compaction_messages(messages.clone(), false, &policy);
        assert!(recent.is_empty());
        assert_eq!(older, messages);
    }

    #[test]
    fn compaction_preserves_recent_messages_and_tool_pairs_exactly() {
        let checkpoint = json!({"role":"user","content":"previous checkpoint"});
        let archived = json!({"role":"assistant","content":"old output".repeat(1000)});
        let recent = vec![
            json!({"role":"user","content":"Keep this exact task: café 中文\nDo not change the API."}),
            json!({"role":"assistant","content":null,"tool_calls":[{"id":"call_1","type":"function","function":{"name":"read","arguments":"{\"filePath\":\"file.txt\"}"}}],"providerMetadata":{"opaque":"keep"}}),
            json!({"role":"tool","tool_call_id":"call_1","content":"1: exact output\n","is_error":false}),
            json!({"role":"assistant","content":"Next: verify the existing API."}),
        ];
        let mut messages = vec![
            checkpoint.clone(),
            json!({"role":"user","content":"old task"}),
            archived.clone(),
        ];
        messages.extend(recent.clone());
        let (older, kept) = compaction_messages(messages, true, &retention(4096));
        assert_eq!(kept, recent);
        assert_eq!(older.first(), Some(&checkpoint));
        assert!(older.contains(&archived));
    }

    #[test]
    fn a_long_tool_turn_keeps_the_latest_user_and_complete_recent_steps() {
        let user =
            json!({"role":"user","content":"Finish the migration; preserve all public names."});
        let expensive = json!({"role":"assistant","content":null,"tool_calls":[{"id":"old_call","type":"function","function":{"name":"apply_patch","arguments":"x".repeat(20_000)}}]});
        let tool = json!({"role":"tool","tool_call_id":"old_call","content":"Done"});
        let tail = vec![
            json!({"role":"assistant","content":null,"tool_calls":[{"id":"new_call","type":"function","function":{"name":"read","arguments":"{}"}}]}),
            json!({"role":"tool","tool_call_id":"new_call","content":"recent result"}),
            json!({"role":"assistant","content":"One test remains."}),
        ];
        let mut messages = vec![user.clone(), expensive.clone(), tool.clone()];
        messages.extend(tail.clone());
        let (older, recent) = compaction_messages(messages, false, &retention(2048));
        assert_eq!(recent, [vec![user], tail].concat());
        assert!(older.contains(&expensive));
        assert!(older.contains(&tool));
        assert_ne!(older.first().unwrap()["role"], "tool");
    }

    #[test]
    fn repeated_compactions_bound_the_tail_and_prioritize_the_latest_user() {
        let mut recent = Vec::new();
        for turn in 0..50 {
            let user = json!({"role":"user","content":format!("exact instruction {turn}: {}", "é".repeat(50))});
            let mut messages = vec![json!({"role":"user","content":"prior checkpoint"})];
            messages.append(&mut recent);
            messages.extend([
                user.clone(),
                json!({"role":"assistant","content":"reply".repeat(30)}),
            ]);
            let (_, kept) = compaction_messages(messages, true, &retention(2048));
            assert!(kept.contains(&user));
            assert!(!kept.iter().any(|m| m["content"] == "prior checkpoint"));
            assert!(kept.iter().map(|m| m.to_string().len()).sum::<usize>() <= 2048);
            recent = kept;
        }
        let user = json!({"role":"user","content":"important".repeat(1000)});
        let (_, kept) = compaction_messages(
            vec![user.clone(), json!({"role":"assistant","content":"reply"})],
            false,
            &retention(1024),
        );
        assert_eq!(kept, vec![user]);
    }

    #[test]
    fn text_attachment_preserves_unicode_and_decodes_content() {
        let encoded = base64::engine::general_purpose::STANDARD.encode("caffè ☕");
        let part = json!({"type":"file","mime":"text/plain","filename":"notes.txt","url":format!("data:text/plain;base64,{encoded}")});
        assert_eq!(
            file_content(&part).unwrap()["text"],
            "Attachment notes.txt:\ncaffè ☕"
        );
    }
    #[test]
    fn image_attachment_is_not_flattened_to_text() {
        let part = json!({"type":"file","mime":"image/png","url":"data:image/png;base64,YQ=="});
        assert_eq!(
            file_content(&part).unwrap()["image_url"]["url"],
            part["url"]
        );
    }
    #[test]
    fn plan_does_not_offer_opaque_execution() {
        for name in [
            "bash",
            "write",
            "edit",
            "apply_patch",
            "task",
            "mcp_custom_write",
        ] {
            assert!(execution_tool(name));
        }
        assert!(!execution_tool("read"));
        assert!(!execution_tool("grep"));
    }
}
