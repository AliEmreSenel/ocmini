mod config;
mod events;
mod payload;
mod process;
mod types;

use crate::config::Config;
use config::{FailurePolicy, Resolved, ResolvedRegistration};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};

static RUNTIME_STARTED: AtomicBool = AtomicBool::new(false);
static RUNTIME_STOPPING: AtomicBool = AtomicBool::new(false);
static INSTANCE_ID: OnceLock<String> = OnceLock::new();
use std::sync::{Arc, Mutex, OnceLock};
use tokio::sync::Semaphore;
use tokio::time::{Duration, Instant};
pub use types::HookResult;

type ActiveInvocations = Arc<Mutex<HashMap<String, String>>>;
type HookCache = Mutex<HashMap<String, (String, Arc<Hooks>)>>;
pub struct Hooks {
    resolved: Resolved,
    snapshot: Value,
    fingerprint: String,
    handlers: HashMap<String, Vec<usize>>,
    capacity: Arc<Semaphore>,
    active: ActiveInvocations,
    _observers: Vec<Arc<events::Observer>>,
}
impl Hooks {
    pub fn disabled(directory: &str) -> Result<Arc<Self>, String> {
        Self::build(Resolved::from_config(&Config::default(), directory)?, false)
    }
    pub fn for_config(config: &Config, directory: &str) -> Result<Arc<Self>, String> {
        let normalized = config::normalize_directory(directory);
        let directory = normalized.as_str();
        static CACHE: OnceLock<HookCache> = OnceLock::new();
        let source = if let Some(sources) = &config.hook_sources {
            json!(sources
                .iter()
                .map(
                    |s| json!({"source":s.source,"base":s.base,"trusted":s.trusted,"hooks":s.value})
                )
                .collect::<Vec<_>>())
        } else {
            config
                .extra
                .get("ocmini")
                .and_then(|v| v.get("hooks"))
                .cloned()
                .unwrap_or(Value::Null)
        };
        let key = config::fingerprint(&source);
        let cache = CACHE.get_or_init(Default::default);
        if let Some((fingerprint, hooks)) = cache.lock().unwrap().get(directory) {
            if fingerprint == &key {
                return Ok(hooks.clone());
            }
        }
        let resolved = Resolved::from_config(config, directory)?;
        // Serialize cache insertion, but never start external work under its lock.
        let mut cache = cache.lock().unwrap();
        if let Some((fingerprint, hooks)) = cache.get(directory) {
            if fingerprint == &key {
                return Ok(hooks.clone());
            }
        }
        events::retire_scope(directory);
        process::configure_limits(&resolved.settings);
        let hooks = Self::build(resolved, true)?;
        if cache.len() >= 128 && !cache.contains_key(directory) {
            if let Some(old) = cache.keys().next().cloned() {
                cache.remove(&old);
            }
        }
        cache.insert(directory.to_owned(), (key, hooks.clone()));
        drop(cache);
        if !RUNTIME_STARTED.swap(true, Ordering::AcqRel) {
            hooks.emit("ocmini.runtime.started", &json!({}), json!({"instanceID":INSTANCE_ID.get_or_init(||crate::types::hex_id("ocm_")),"mode":"local"}));
        }
        hooks.emit("ocmini.config.loaded",&json!({"directory":directory}),json!({"fingerprint":hooks.fingerprint(),"sources":hooks.resolved.handlers.iter().chain(&hooks.resolved.observers).map(|r|json!({"id":r.registration.id,"source":r.source,"trusted":r.trusted,"enabled":r.active()})).collect::<Vec<_>>()}));
        Ok(hooks)
    }
    fn build(resolved: Resolved, register_observers: bool) -> Result<Arc<Self>, String> {
        let snapshot = serde_json::to_value(&resolved).map_err(|e| e.to_string())?;
        let fingerprint = config::fingerprint(&snapshot);
        static ACTIVE: OnceLock<ActiveInvocations> = OnceLock::new();
        let active = ACTIVE
            .get_or_init(|| Arc::new(Mutex::new(HashMap::new())))
            .clone();
        let capacity = Arc::new(Semaphore::new(resolved.settings.max_processes));
        let observer_capacity = Arc::new(Semaphore::new(resolved.settings.max_processes));
        let settings = Arc::new(resolved.settings.clone());
        let observers = resolved
            .observers
            .iter()
            .filter(|r| register_observers && r.active())
            .map(|r| {
                events::Observer::register(
                    r.clone(),
                    settings.clone(),
                    &resolved.directory,
                    observer_capacity.clone(),
                    active.clone(),
                )
            })
            .collect();
        let mut handlers: HashMap<String, Vec<usize>> = HashMap::new();
        for (index, r) in resolved
            .handlers
            .iter()
            .enumerate()
            .filter(|(_, r)| r.active())
        {
            for name in r.registration.on.values() {
                handlers.entry(name.to_owned()).or_default().push(index);
            }
        }
        Ok(Arc::new(Self {
            resolved,
            snapshot,
            fingerprint,
            handlers,
            capacity,
            active,
            _observers: observers,
        }))
    }
    pub fn from_snapshot(snapshot: Value, directory: &str) -> Result<Arc<Self>, String> {
        let mut resolved: Resolved =
            serde_json::from_value(snapshot).map_err(|e| format!("invalid hook snapshot: {e}"))?;
        resolved.validate_snapshot(directory)?;
        // Recovery freezes commands. Current trusted policy may only disable them.
        let current = Config::load_for(directory)?;
        let current = Resolved::from_config(&current, directory)?;
        for (registrations, policy) in [
            (&mut resolved.handlers, &current.handlers),
            (&mut resolved.observers, &current.observers),
        ] {
            for old in registrations {
                if !old.trusted && !current.settings.allow_project_hooks.allows(directory) {
                    old.blocked = Some("current trusted policy disables project hooks".into());
                }
                if policy.iter().any(|p| {
                    p.trusted && p.registration.id == old.registration.id && !p.registration.enabled
                }) {
                    old.blocked = Some("disabled by current trusted configuration".into());
                }
            }
        }
        Self::build(resolved, false)
    }
    pub fn snapshot(&self) -> Value {
        self.snapshot.clone()
    }
    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }
    pub fn is_empty(&self) -> bool {
        self.handlers.is_empty() && self._observers.is_empty()
    }
    pub fn has(&self, name: &str) -> bool {
        self.handlers.contains_key(name)
    }
    pub fn admission_timeout_ms(&self) -> u64 {
        self.resolved.settings.admission_chain_timeout_ms
    }
    pub fn matches(&self, name: &str, context: &Value) -> bool {
        self.handlers.get(name).is_some_and(|indices| {
            indices.iter().any(|&i| {
                self.resolved.handlers[i]
                    .registration
                    .matcher
                    .matches(name, context, true)
            })
        })
    }
    pub fn attach_store(&self, store: Arc<crate::store::Store>) {
        for observer in &self._observers {
            observer.attach_store(store.clone());
        }
    }
    pub fn max_end_continuations(&self) -> Option<u32> {
        self.resolved.settings.max_end_continuations
    }
    pub fn is_active_invocation(&self, id: &str, session: &str) -> bool {
        self.active
            .lock()
            .unwrap()
            .get(id)
            .is_some_and(|s| s == session)
    }
    pub async fn run(
        &self,
        name: &str,
        context: &Value,
        mut input: Value,
        cancel: Option<&Arc<AtomicBool>>,
    ) -> Result<HookResult, String> {
        let Some(handlers) = self.handlers.get(name) else {
            return Ok(HookResult {
                input,
                feedback: None,
            });
        };
        let mut context = context.clone();
        if let Some(object) = context.as_object_mut() {
            object.insert("directory".into(), json!(self.resolved.directory));
            object.insert("configFingerprint".into(), json!(self.fingerprint));
        }
        events::remember(&context);
        let budget = if types::is_admission(name) {
            Some(self.resolved.settings.admission_chain_timeout_ms)
        } else if name == "turn.end.after" {
            self.resolved.settings.finalizer_chain_timeout_ms
        } else {
            self.resolved.settings.chain_timeout_ms
        };
        let deadline = budget.map(|n| Instant::now() + Duration::from_millis(n));
        for &index in handlers {
            let registration = &self.resolved.handlers[index];
            if !registration
                .registration
                .matcher
                .matches(name, &context, true)
            {
                continue;
            }
            let invocation = crate::types::hex_id("hki_");
            let session = context["sessionID"].as_str().unwrap_or("");
            let _guard = InvocationGuard::new(self.active.clone(), &invocation, session);
            let start = Instant::now();
            diagnostic(
                "started",
                registration,
                name,
                &context,
                &invocation,
                start,
                None,
            );
            let operation = format!(
                "{}:{}:{}:{}:{}:{}:{}:{}",
                context["operationID"]
                    .as_str()
                    .or(context["turnID"].as_str())
                    .unwrap_or(session),
                context["requestID"].as_str().unwrap_or(""),
                context["callID"].as_str().unwrap_or(""),
                context["messageID"].as_str().unwrap_or(""),
                context["partID"].as_str().unwrap_or(""),
                name,
                context["endAttempt"].as_u64().unwrap_or(0),
                registration.registration.id
            );
            #[derive(serde::Serialize)]
            #[serde(rename_all = "camelCase")]
            struct Envelope<'a> {
                version: u32,
                hook: &'a str,
                #[serde(rename = "invocationID")]
                invocation_id: &'a str,
                #[serde(rename = "operationID")]
                operation_id: &'a str,
                context: &'a Value,
                input: &'a Value,
            }
            let envelope = Envelope {
                version: 1,
                hook: name,
                invocation_id: &invocation,
                operation_id: &operation,
                context: &context,
                input: &input,
            };
            let process_result = process::run(
                registration,
                &self.resolved.settings,
                &self.resolved.directory,
                &envelope,
                cancel,
                deadline,
                self.capacity.clone(),
                name,
                &invocation,
                session,
            )
            .await;
            let mut diagnostic_context = context.clone();
            let mut cancelled = false;
            let stderr_tail;
            let mut explicit_denial = false;
            let mut result = match process_result {
                Ok(outcome) => {
                    diagnostic_context["hookTiming"] = json!({"queueMs":outcome.queue_ms,"executionMs":outcome.execution_ms,"stderrBytes":outcome.stderr_bytes,"stderrRetainedBytes":outcome.stderr_tail.len()});
                    let response = outcome
                        .response_value(self.resolved.settings.max_payload_bytes)
                        .map_err(|e| e.to_string());
                    let result = response.and_then(|response| match response {
                        None => Ok(None),
                        Some(response) => {
                            explicit_denial =
                                matches!(response["decision"].as_str(), Some("block" | "deny"));
                            types::apply_response(name, response, &mut input)
                        }
                    });
                    stderr_tail = outcome.stderr_tail;
                    result
                }
                Err(error) => {
                    cancelled = matches!(error.kind, process::FailureKind::Cancelled);
                    diagnostic_context["hookTiming"] = json!({"queueMs":error.queue_ms,"executionMs":error.execution_ms,"stderrBytes":error.stderr_bytes,"stderrRetainedBytes":error.stderr_tail.len(),"exitCode":error.exit_code});
                    diagnostic_context["hookFailureKind"] = json!(format!("{:?}", error.kind));
                    let summary = error.to_string();
                    stderr_tail = error.stderr_tail;
                    Err(summary)
                }
            };
            if process::is_shutdown() || cancel.is_some_and(|c| c.load(Ordering::Acquire)) {
                cancelled = true;
                result = Err("hook cancelled".into());
            }
            let artifact = if result.is_err() {
                payload::diagnostic_artifact(&invocation, &stderr_tail)
                    .ok()
                    .flatten()
            } else {
                None
            };
            if let Some(path) = &artifact {
                diagnostic_context["stderrArtifact"] = json!(path);
            }
            diagnostic(
                if result.is_ok() {
                    "completed"
                } else {
                    "failed"
                },
                registration,
                name,
                &diagnostic_context,
                &invocation,
                start,
                result.as_ref().err().map(String::as_str),
            );
            match result {
                Ok(Some(feedback)) => {
                    return Ok(HookResult {
                        input,
                        feedback: Some(feedback),
                    })
                }
                Ok(None) => {}
                Err(error) => {
                    let must_stop = cancelled
                        || explicit_denial
                        || process::is_shutdown()
                        || cancel.is_some_and(|c| c.load(Ordering::Acquire))
                        || error.starts_with("hook rejected:")
                        || registration.registration.failure_policy == FailurePolicy::Block;
                    if must_stop {
                        let details = artifact
                            .map(|path| format!("\nhook stderr tail: {}", path.display()))
                            .unwrap_or_default();
                        return Err(format!(
                            "hook {} ({name}): {error}{details}",
                            registration.registration.id
                        ));
                    }
                }
            }
        }
        Ok(HookResult {
            input,
            feedback: None,
        })
    }
    pub fn emit(&self, name: &str, context: &Value, data: Value) {
        crate::engine::bus().publish(self.event(name, context, data));
    }
    pub fn event(&self, name: &str, context: &Value, data: Value) -> Arc<crate::types::BusEvent> {
        let mut properties = match data {
            Value::Object(object) => object,
            other => {
                let mut object = serde_json::Map::new();
                object.insert("data".into(), other);
                object
            }
        };
        properties.insert("version".into(), json!(1));
        properties.insert("directory".into(), json!(self.resolved.directory));
        properties.insert("time".into(), json!(crate::types::now_ms()));
        for key in [
            "sessionID",
            "partID",
            "operationID",
            "turnID",
            "messageID",
            "callID",
            "requestID",
            "turnKind",
            "requestKind",
            "parentID",
            "parentSessionID",
            "agent",
            "model",
            "origin",
            "tool",
            "outcome",
            "reason",
            "endAttempt",
            "step",
        ] {
            if let Some(value) = context.get(key) {
                properties.entry(key).or_insert_with(|| value.clone());
            }
        }
        let properties = Value::Object(properties);
        events::remember(&properties);
        Arc::new(crate::types::BusEvent {
            id: crate::types::hex_id("evt_"),
            r#type: name.to_owned(),
            properties,
        })
    }
}
struct InvocationGuard {
    active: ActiveInvocations,
    id: String,
}
impl InvocationGuard {
    fn new(active: ActiveInvocations, id: &str, session: &str) -> Self {
        active
            .lock()
            .unwrap()
            .insert(id.to_owned(), session.to_owned());
        Self {
            active,
            id: id.to_owned(),
        }
    }
}
impl Drop for InvocationGuard {
    fn drop(&mut self) {
        self.active.lock().unwrap().remove(&self.id);
    }
}
fn diagnostic(
    state: &str,
    registration: &ResolvedRegistration,
    name: &str,
    context: &Value,
    invocation: &str,
    start: Instant,
    error: Option<&str>,
) {
    let mut properties = json!({"version":1,"directory":context["directory"],"sessionID":context["sessionID"],"turnID":context["turnID"],"time":crate::types::now_ms(),"invocationID":invocation,"handlerID":registration.registration.id,"hook":name,"source":registration.source,"trusted":registration.trusted,"durationMs":start.elapsed().as_millis() as u64});
    if name == "event" {
        properties["observer"] = json!(true);
    }
    if let Some(timing) = context.get("hookTiming") {
        properties["timing"] = timing.clone();
    }
    if let Some(kind) = context.get("hookFailureKind") {
        properties["failureKind"] = kind.clone();
    }
    if let Some(path) = context.get("stderrArtifact") {
        properties["stderrArtifact"] = path.clone();
    }
    // Never include stdout, stderr, transformed headers, or environment values.
    if let Some(error) = error {
        properties["diagnostic"] = json!(if error.starts_with("hook rejected:") {
            "hook rejected the operation".to_owned()
        } else {
            "hook invocation failed; inspect the originating operation for details".to_owned()
        });
    }
    crate::engine::bus().emit(&format!("ocmini.hook.{state}"), properties);
}
pub fn validate_config(config: &Config, directory: &str) -> Result<(), String> {
    Resolved::from_config(config, directory).map(|_| ())
}
pub fn debug_config(config: &Config, directory: &str) -> Result<Value, String> {
    Resolved::from_config(config, directory).map(|r| {
        let mut value = r.debug();
        value["liveProcessPolicy"] = process::limits_debug();
        value
    })
}
pub fn remember_session(session: &crate::types::Session) {
    events::remember(&json!({
        "sessionID":session.id,
        "directory":session.directory,
        "parentID":session.parent_id
    }));
}

pub fn observe(event: &Arc<crate::types::BusEvent>) {
    events::observe(event)
}
pub fn persist_committed(
    tx: &rusqlite::Transaction<'_>,
    event: &crate::types::BusEvent,
) -> Result<Vec<String>, String> {
    events::persist_committed(tx, event)
}
pub fn persist_recovered(
    tx: &rusqlite::Transaction<'_>,
    event: &crate::types::BusEvent,
    snapshot: Option<&str>,
) -> Result<Vec<String>, String> {
    events::persist_recovered(tx, event, snapshot)
}
pub fn has_observers() -> bool {
    events::has_observers()
}
pub fn protected_tool_metadata() -> &'static [&'static str] {
    types::PROTECTED_TOOL_METADATA
}
pub async fn shutdown() {
    if RUNTIME_STARTED.load(Ordering::Acquire) && !RUNTIME_STOPPING.swap(true, Ordering::AcqRel) {
        crate::engine::bus().emit("ocmini.runtime.stopping", json!({"version":1,"directory":"","time":crate::types::now_ms(),"instanceID":INSTANCE_ID.get(),"mode":"local"}));
        // Give already-notified observers a chance to receive the stopping fact.
        tokio::task::yield_now().await;
    }
    process::shutdown();
    process::wait_for_shutdown().await;
}

#[cfg(test)]
mod tests;
