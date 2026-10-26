use crate::events as event;
use tokio_util::sync::CancellationToken;
mod admission;
mod compaction;
mod history;
pub(crate) mod hooks;
mod instructions;
mod model_step;
mod tool_execution;
mod turn;
use crate::config::{CompactionCfg, Config};
use crate::engine::{bus, App, EngineApi};
use crate::provider::{self, ChatRequest};
use crate::store::Store;
use crate::title;
use crate::tools::{self, PreparedTool, ToolCtx};
use crate::types::*;
use compaction::*;
#[cfg(test)]
use history::build_messages;
use instructions::system_prompt;
use model_step::*;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio::sync::oneshot;
use tool_execution::*;
use turn::run_turn;

struct Running {
    app: App,
    cancel: CancellationToken,
    done: CancellationToken,
    waiters: HashMap<String, Vec<oneshot::Sender<TurnResult>>>,
    end_gate: Option<CancellationToken>,
}
impl Drop for Running {
    fn drop(&mut self) {
        self.done.cancel();
    }
}
struct EndGateGuard {
    engine: Arc<dyn EngineApi>,
    session_id: String,
    token: CancellationToken,
}
impl Drop for EndGateGuard {
    fn drop(&mut self) {
        self.engine.finish_end_gate(&self.session_id, &self.token);
    }
}

struct Admitting {
    app: App,
    session_id: String,
    cancel: CancellationToken,
}

struct AdmissionGuard {
    admissions: Arc<Mutex<HashMap<String, Admitting>>>,
    app: App,
    mid: String,
    token: String,
    cancel: CancellationToken,
}
impl Drop for AdmissionGuard {
    fn drop(&mut self) {
        self.cancel.cancel();
        self.admissions.lock().unwrap().remove(&self.mid);
        let _ = self.app.store.reject_hook_admission(
            &self.mid,
            &self.token,
            "Prompt admission interrupted",
        );
    }
}

#[derive(Clone, Default)]
pub struct Engine {
    running: Arc<Mutex<HashMap<String, Running>>>,
    admissions: Arc<Mutex<HashMap<String, Admitting>>>,
}

impl Engine {
    fn recover_once(&self, app: &App) {
        if let Err(error) = crate::hooks::recover(&app.store) {
            emit_error(app, "", &error, false);
            return;
        }
        crate::blocking::run(|| {
            for sid in app.store.pending_sessions().unwrap_or_default() {
                if let Some(session) = app.store.get_session(&sid).unwrap_or_default() {
                    match app.for_directory(&session.directory) {
                        Ok(scoped) => self.wake(scoped, sid),
                        Err(error) => emit_error(app, &sid, &error, false),
                    }
                }
            }
        })
    }

    pub fn new() -> Self {
        Self::default()
    }

    pub async fn abort_and_wait(&self, sid: &str) {
        self.abort(sid);
        self.wait_for_idle(sid).await;
    }

    pub async fn wait_for_idle(&self, sid: &str) {
        loop {
            let done = self
                .running
                .lock()
                .unwrap()
                .get(sid)
                .map(|state| state.done.clone());
            match done {
                Some(done) => done.cancelled().await,
                None => return,
            }
        }
    }

    fn wake(&self, app: App, sid: String) {
        crate::blocking::run(|| {
            let mut running = self.running.lock().unwrap();
            if running.contains_key(&sid) {
                return;
            }
            let cancel = match self.claim(&mut running, &app, &sid, None) {
                Ok(cancel) => cancel,
                Err(error) => {
                    emit_error(&app, &sid, &error, false);
                    return;
                }
            };
            drop(running);
            let engine = self.clone();
            tokio::spawn(async move {
                engine.drain(app, sid, cancel).await;
            });
        })
    }

    fn claim(
        &self,
        running: &mut HashMap<String, Running>,
        app: &App,
        sid: &str,
        waiter: Option<(String, oneshot::Sender<TurnResult>)>,
    ) -> Result<CancellationToken, OperationError> {
        app.store.claim(sid)?;
        let cancel = CancellationToken::new();
        running.insert(
            sid.into(),
            Running {
                end_gate: None,
                done: CancellationToken::new(),
                app: app.clone(),
                cancel: cancel.clone(),
                waiters: waiter
                    .into_iter()
                    .map(|(mid, tx)| (mid, vec![tx]))
                    .collect(),
            },
        );
        crate::sessions::set_status(app, sid, event::SessionStatus::Busy);
        Ok(cancel)
    }

    async fn drain(&self, app: App, sid: String, cancel: CancellationToken) {
        // Keep the ownership lease fresh while this instance owns the session,
        // so a sibling instance's restart recovery ignores its live work.
        let heartbeat = {
            let (app, sid, cancel) = (app.clone(), sid.clone(), cancel.clone());
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(std::time::Duration::from_secs(20)).await;
                    if !matches!(app.store.heartbeat(&sid), Ok(true)) {
                        cancel.cancel();
                        app.perms.reject_all(&sid);
                        app.questions.reject_all(&sid);
                        break;
                    }
                }
            })
        };
        self.drain_inner(&app, &sid, &cancel).await;
        heartbeat.abort();
    }

    async fn drain_inner(&self, app: &App, sid: &str, cancel: &CancellationToken) {
        let sid = sid.to_string();
        let app = app.clone();
        let cancel = cancel.clone();
        loop {
            if cancel.is_cancelled() {
                // Local admissions are fenced by Running.cancel under the
                // running lock; the owner lease fences sibling instances.
                // Keep both fences until every queued finalizer has settled.
                let queued = app.store.queued_hook_prompts(&sid).unwrap_or_default();
                for request in queued {
                    let mid = request.message_id.as_deref().unwrap_or_default();
                    let value = Err("Aborted".into());
                    let mut scoped = app.clone();
                    if let Ok(Some(snapshot)) = app.store.turn_snapshot(mid) {
                        let directory = app.store.directory_of(&sid).unwrap_or_default();
                        scoped.hooks = app
                            .hook_runtime
                            .restore_snapshot(snapshot, &directory)
                            .or_else(|_| app.hook_runtime.disabled(&directory))
                            .unwrap_or_else(|_| app.hooks.clone());
                    }
                    let _ =
                        finalize_turn(&scoped, &sid, &request, &value, "cancelled", false).await;
                    if let Some(waiters) = self
                        .running
                        .lock()
                        .unwrap()
                        .get_mut(&sid)
                        .and_then(|s| s.waiters.remove(mid))
                    {
                        for waiter in waiters {
                            let _ = waiter.send(value.clone());
                        }
                    }
                }
            }
            // Admission and the idle transition share this lock. A prompt cannot
            // land between the final inbox check and releasing session ownership.
            let request = crate::blocking::run(|| {
                let mut running = self.running.lock().unwrap();
                if cancel.is_cancelled() {
                    if let Err(e) = app.store.cancel_pending(&sid) {
                        emit_error(&app, &sid, &e, false);
                    }
                    finish_running(&app, &sid, running.remove(&sid), Some("Aborted"));
                    return None;
                }
                match app.store.next_prompt(&sid) {
                    Ok(Some(request)) => Some(request),
                    Ok(None) => {
                        finish_running(&app, &sid, running.remove(&sid), None);
                        None
                    }
                    Err(error) => {
                        emit_error(&app, &sid, &error, false);
                        finish_running(&app, &sid, running.remove(&sid), Some(&error));
                        None
                    }
                }
            });
            let Some(request) = request else {
                return;
            };
            let mid = request.message_id.clone().unwrap_or_default();
            let mut scoped = app.clone();
            let directory = app.store.directory_of(&sid).unwrap_or_default();
            let snapshot_error = match app.store.turn_snapshot(&mid) {
                Ok(Some(snapshot)) => match app.hook_runtime.restore_snapshot(snapshot, &directory)
                {
                    Ok(hooks) => {
                        scoped.hooks = hooks;
                        None
                    }
                    Err(error) => Some(error.to_string()),
                },
                Ok(None) => None,
                Err(error) => Some(error.to_string()),
            };
            let result = if let Some(error) = snapshot_error {
                scoped.hooks = app
                    .hook_runtime
                    .disabled(&directory)
                    .unwrap_or_else(|_| app.hooks.clone());
                Err(error.into())
            } else {
                let engine: Arc<dyn EngineApi> = Arc::new(self.clone());
                run_turn(
                    scoped.clone(),
                    engine,
                    &sid,
                    request.clone(),
                    cancel.clone(),
                )
                .await
            };
            let value = result;
            if let Err(error) = &value {
                emit_error(&app, &sid, error, cancel.is_cancelled());
            }
            if request.command.as_deref() == Some("__ocmini_compact") {
                if let Err(error) = &value {
                    scoped.hooks.emit(
                        event::Compaction::Failed {
                            error: &error.message,
                        },
                        &turn_context(&scoped, &sid, &request),
                    );
                }
            }
            let reason = if cancel.is_cancelled() {
                "cancelled"
            } else if let Err(error) = &value {
                error.kind.as_str()
            } else if app.store.has_waiting(&sid).unwrap_or(false) {
                "steered"
            } else {
                "completed"
            };
            if let Err(error) = finalize_turn(&scoped, &sid, &request, &value, reason, true).await {
                let _ = app.store.finish_prompt(&mid);
                emit_error(&app, &sid, &error, false);
            }
            let idle = crate::blocking::run(|| {
                let mut running = self.running.lock().unwrap();
                let waiters = running
                    .get_mut(&sid)
                    .and_then(|state| state.waiters.remove(&mid))
                    .unwrap_or_default();
                let idle = match app.store.has_waiting(&sid) {
                    Ok(waiting) => !waiting,
                    Err(error) => {
                        emit_error(&app, &sid, &error, false);
                        finish_running(&app, &sid, running.remove(&sid), Some(&error));
                        for waiter in waiters {
                            let _ = waiter.send(value.clone());
                        }
                        return true;
                    }
                };
                if idle {
                    // A completed synchronous response promises resumable work.
                    // Release ownership before waking its client, which may exit
                    // or restart the server as soon as it receives the response.
                    finish_running(&app, &sid, running.remove(&sid), None);
                }
                for waiter in waiters {
                    let _ = waiter.send(value.clone());
                }
                idle
            });
            if idle {
                return;
            }
        }
    }
}

fn finish_running(app: &App, sid: &str, running: Option<Running>, error: Option<&str>) {
    app.perms.reject_all(sid);
    app.questions.reject_all(sid);
    app.store.release(sid);
    if let Some(mut running) = running {
        let value = Err(error.unwrap_or("Prompt is no longer pending").into());
        for waiters in std::mem::take(&mut running.waiters).into_values() {
            for waiter in waiters {
                let _ = waiter.send(value.clone());
            }
        }
    }
    crate::sessions::set_status(app, sid, event::SessionStatus::Idle);
    app.hooks.emit(
        event::Session::Idle { session_id: sid },
        &app.session_context(sid, json!({})),
    );
}

async fn wait_for_finalization(
    store: Arc<Store>,
    mid: String,
    mut waiter: oneshot::Sender<TurnResult>,
) {
    loop {
        if waiter.is_closed() {
            return;
        }
        // Poll only the state column while a sibling finalizes. Do not rebuild
        // a potentially large stored response on every wakeup.
        match store.hook_turn_finalizing(&mid) {
            Ok(true) => {}
            Ok(false) => {
                let value = match store.turn_result(&mid) {
                    Ok(Some((value, _))) => value,
                    Ok(None) => Err("Recorded turn result not found".into()),
                    Err(error) => Err(error),
                };
                let _ = waiter.send(value);
                return;
            }
            Err(error) => {
                let _ = waiter.send(Err(error));
                return;
            }
        }
        tokio::select! {
            _ = waiter.closed() => return,
            _ = tokio::time::sleep(std::time::Duration::from_millis(50)) => {}
        }
    }
}

fn validate_message_id(mid: &str) -> Result<(), OperationError> {
    if mid.is_empty()
        || mid.len() > 256
        || !mid
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
    {
        return Err(OperationError::invalid(
            "messageID must contain 1–256 ASCII letters, digits, underscores or hyphens",
        ));
    }
    Ok(())
}

fn turn_context(app: &App, sid: &str, request: &PromptRequest) -> Value {
    app.session_context(sid, json!({"turnID":request.message_id,"turnKind":if request.command.as_deref()==Some("__ocmini_compact"){"compaction"}else{"primary"},"agent":request.agent.as_deref().unwrap_or("build"),"model":request.model,"configFingerprint":app.hooks.fingerprint()}))
}

async fn finalize_turn(
    app: &App,
    sid: &str,
    request: &PromptRequest,
    value: &TurnResult,
    reason: &str,
    started: bool,
) -> Result<(), String> {
    let mid = request
        .message_id
        .as_deref()
        .ok_or("Missing turn identity")?;
    let mut context = turn_context(app, sid, request);
    context["outcome"] = json!(reason);
    let response_id = value.as_ref().ok().map(|message| message.info.id.as_str());
    let error = value.as_ref().err().map(|error| error.message.as_str());
    let mut events = Vec::new();
    if started && value.is_ok() {
        if let Some(command) = request
            .command
            .as_deref()
            .filter(|s| *s != "__ocmini_compact")
        {
            events.push(app.hooks.event(
                event::Command::Executed {
                    name: command,
                    arguments: request.command_arguments.as_deref().unwrap_or_default(),
                    message_id: response_id,
                },
                &context,
            ));
        }
    }
    events.push(app.hooks.event(
        event::Turn::Ended {
            reason,
            started,
            response_id,
            error,
            finalizer_error: None,
        },
        &context,
    ));
    for event in &mut events {
        Arc::make_mut(event).id = format!("evt_{}_{}", mid, event.kind.as_str().replace('.', "_"));
    }
    app.store
        .settle_hook_turn(mid, sid, value, reason, started, &events)?;
    let mut diagnostic = None;
    if app
        .hooks
        .matches(crate::engine_impl::hooks::TURN_END_AFTER, &context)
    {
        app.store.finalizer_started(mid)?;
        let input =
            json!({"reason":reason,"started":started,"responseID":response_id,"error":error});
        let hooks = app.hooks.run(
            crate::engine_impl::hooks::TURN_END_AFTER,
            &context,
            input,
            None,
        );
        if let Err(error) = hooks.await {
            diagnostic = Some(error);
        }
    }
    for event in &mut events {
        if event.kind == event::EventKind::Turn(event::TurnKind::Ended) {
            Arc::make_mut(event).properties["finalizerError"] = json!(diagnostic);
        }
    }
    let delivery_failures = app.store.finish_hook_turn(
        mid,
        diagnostic.as_deref(),
        &app.hooks.prepare_deliveries(&events)?,
    )?;
    for event in events {
        bus().publish(event);
    }
    for error in delivery_failures {
        app.hooks.emit(
            event::Hook::Overflow {
                data: event::Overflow::Durable {
                    diagnostic: &(error.chars().take(512).collect::<String>()),
                },
            },
            &context,
        );
    }
    Ok(())
}

fn error_value(error: &str, aborted: bool) -> Value {
    json!({"name":if aborted {"MessageAbortedError"} else {"UnknownError"},"data":{"message":error}})
}
fn emit_error(app: &App, sid: &str, error: &str, aborted: bool) {
    app.hooks.emit(
        event::Session::Error {
            session_id: sid,
            error: &(error_value(error, aborted)),
        },
        &app.session_context(sid, json!({})),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;

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
    fn summarizer_caps_tool_outputs_at_the_upstream_length() {
        assert_eq!(truncate_tool_output("short"), "short");
        let long = "x".repeat(TOOL_OUTPUT_MAX_CHARS + 10);
        assert_eq!(
            truncate_tool_output(&long),
            format!("{}\n[truncated]", "x".repeat(TOOL_OUTPUT_MAX_CHARS))
        );
        // Counts scalar characters, so a cap boundary never splits an escape.
        let unicode = "é".repeat(TOOL_OUTPUT_MAX_CHARS + 5);
        assert_eq!(
            truncate_tool_output(&unicode).chars().count(),
            TOOL_OUTPUT_MAX_CHARS + "\n[truncated]".chars().count()
        );
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
            crate::attachments::content(&part).unwrap()["text"],
            "Attachment notes.txt:\ncaffè ☕"
        );
    }
    #[test]
    fn image_attachment_is_not_flattened_to_text() {
        let part = json!({"type":"file","mime":"image/png","url":"data:image/png;base64,YQ=="});
        assert_eq!(
            crate::attachments::content(&part).unwrap()["image_url"]["url"],
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
