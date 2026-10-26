use crate::events as event;
use tokio_util::sync::CancellationToken;
mod config;
mod events;
mod payload;
mod process;
mod recovery;
pub use recovery::recover;
mod types;

use crate::config::Config;
use config::{FailurePolicy, Resolved, ResolvedRegistration};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};

#[cfg(test)]
use std::sync::OnceLock;
use std::sync::{Arc, Mutex};
use tokio::sync::Semaphore;
use tokio::time::{Duration, Instant};
pub use types::{
    CommandHook, CompactionHook, FileHook, Hook, HookName, HookResult, InvocationKind, MessageHook,
    ModelHook, PermissionHook, PromptHook, QuestionHook, SessionHook, Shape, ShellHook, SkillHook,
    TaskHook, ToolHook, TurnHook,
};

#[cfg(test)]
fn test_hook(name: &str) -> Hook {
    types::declarations()
        .find(|h| h.name.as_str() == name)
        .expect("known test hook")
}
#[cfg(test)]
impl From<&str> for Hook {
    fn from(name: &str) -> Self {
        test_hook(name)
    }
}

pub use crate::types::{FailureKind, OperationError};

type ActiveInvocations = Arc<Mutex<HashMap<String, String>>>;
type HookCache = Mutex<HashMap<String, (String, Arc<Hooks>)>>;
pub(crate) use events::Registry as ObserverRegistry;

/// Application-owned resolved scopes, active invocations and subscribers.
/// The bus keeps only a weak subscription to this runtime's observer registry.
pub struct Runtime {
    cache: HookCache,
    process: Arc<process::Runtime>,
    active: ActiveInvocations,
    registry: Arc<events::Registry>,
    started: AtomicBool,
    stopping: AtomicBool,
    instance: String,
}

pub struct Hooks {
    registry: Arc<events::Registry>,
    process: Arc<process::Runtime>,
    resolved: Resolved,
    snapshot: Value,
    fingerprint: String,
    handlers: HashMap<String, Vec<usize>>,
    capacity: Arc<Semaphore>,
    active: ActiveInvocations,
    _observers: Vec<Arc<events::Observer>>,
}
impl Runtime {
    pub fn new() -> Arc<Self> {
        let runtime = Arc::new(Self {
            cache: Mutex::new(HashMap::new()),
            process: process::Runtime::new(),
            active: Arc::new(Mutex::new(HashMap::new())),
            registry: Arc::new(events::Registry::default()),
            started: AtomicBool::new(false),
            stopping: AtomicBool::new(false),
            instance: crate::types::hex_id("ocm_"),
        });
        crate::engine::bus().register_observers(&runtime.registry);
        runtime
    }
    pub async fn shutdown(&self) {
        if self.started.load(Ordering::Acquire) && !self.stopping.swap(true, Ordering::AcqRel) {
            crate::engine::bus().emit(
                event::Runtime::Stopping {
                    instance_id: &self.instance,
                    mode: event::RuntimeMode::Local,
                },
                json!({"version":1,"directory":"","time":crate::types::now_ms()}),
            );
            tokio::task::yield_now().await;
        }
        self.process.shutdown().await;
    }

    pub fn disabled(&self, directory: &str) -> Result<Arc<Hooks>, String> {
        self.build(Resolved::from_config(&Config::default(), directory)?, false)
    }
    pub fn install(&self, config: &Config, directory: &str) -> Result<Arc<Hooks>, String> {
        let normalized = config::normalize_directory(directory);
        let directory = normalized.as_str();
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
        let cache = &self.cache;
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
        events::retire_scope(&self.registry, directory);
        self.process.configure(&resolved.settings);
        let hooks = self.build(resolved, true)?;
        if cache.len() >= 128 && !cache.contains_key(directory) {
            if let Some(old) = cache.keys().next().cloned() {
                cache.remove(&old);
            }
        }
        cache.insert(directory.to_owned(), (key, hooks.clone()));
        drop(cache);
        if !self.started.swap(true, Ordering::AcqRel) {
            hooks.emit(
                event::Runtime::Started {
                    instance_id: &self.instance,
                    mode: event::RuntimeMode::Local,
                },
                &json!({}),
            );
        }
        let sources = hooks
            .resolved
            .handlers
            .iter()
            .chain(&hooks.resolved.observers)
            .map(|r| event::Source {
                id: &r.registration.id,
                source: &r.source,
                trusted: r.trusted,
                enabled: r.active(),
            })
            .collect::<Vec<_>>();
        hooks.emit(
            event::Config::Loaded {
                fingerprint: hooks.fingerprint(),
                sources: &sources,
            },
            &json!({"directory":directory}),
        );

        Ok(hooks)
    }
    fn build(&self, resolved: Resolved, register_observers: bool) -> Result<Arc<Hooks>, String> {
        let snapshot = serde_json::to_value(&resolved).map_err(|e| e.to_string())?;
        let fingerprint = config::fingerprint(&snapshot);
        let active = self.active.clone();
        let capacity = Arc::new(Semaphore::new(resolved.settings.max_processes));
        let observer_capacity = Arc::new(Semaphore::new(resolved.settings.max_processes));
        let settings = Arc::new(resolved.settings.clone());
        let observers = resolved
            .observers
            .iter()
            .filter(|r| register_observers && r.active())
            .map(|r| {
                events::Observer::register_in(
                    self.registry.clone(),
                    self.process.clone(),
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
        Ok(Arc::new(Hooks {
            process: self.process.clone(),
            registry: self.registry.clone(),
            resolved,
            snapshot,
            fingerprint,
            handlers,
            capacity,
            active,
            _observers: observers,
        }))
    }
    pub fn restore_snapshot(&self, snapshot: Value, directory: &str) -> Result<Arc<Hooks>, String> {
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
        self.build(resolved, false)
    }
}

impl Hooks {
    pub fn snapshot(&self) -> Value {
        self.snapshot.clone()
    }
    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }
    pub fn is_empty(&self) -> bool {
        self.handlers.is_empty() && self._observers.is_empty()
    }
    pub fn has(&self, hook: impl Into<Hook>) -> bool {
        self.handlers.contains_key(hook.into().name.as_str())
    }
    pub fn admission_timeout_ms(&self) -> u64 {
        self.resolved.settings.admission_chain_timeout_ms
    }
    pub fn matches(&self, hook: impl Into<Hook>, context: &Value) -> bool {
        let hook = hook.into();
        self.handlers
            .get(hook.name.as_str())
            .is_some_and(|indices| {
                indices.iter().any(|&i| {
                    self.resolved.handlers[i]
                        .registration
                        .matcher
                        .matches_context(context, hook.defaults)
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
    /// A typed producer projection stays unchanged when no handler matches.
    pub async fn transform<T: serde::Serialize + serde::de::DeserializeOwned>(
        &self,
        hook: Hook,
        context: &Value,
        input: T,
        cancel: Option<&CancellationToken>,
    ) -> Result<T, OperationError> {
        if !self.matches(hook, context) {
            return Ok(input);
        }
        let encode_error = |error: serde_json::Error| OperationError {
            kind: FailureKind::Error,
            message: format!("invalid hook state: {error}"),
        };
        let input = serde_json::to_value(input).map_err(encode_error)?;
        let result = self.run(hook, context, input, cancel).await?;
        serde_json::from_value(result.input).map_err(encode_error)
    }

    pub async fn run(
        &self,
        hook: impl Into<Hook>,
        context: &Value,
        input: Value,
        cancel: Option<&CancellationToken>,
    ) -> Result<HookResult, OperationError> {
        self.run_with(hook, context, input, cancel, |_, _| Ok(()))
            .await
    }
    pub async fn run_with(
        &self,
        hook: impl Into<Hook>,
        context: &Value,
        mut input: Value,
        cancel: Option<&CancellationToken>,
        mut validate: impl FnMut(&Value, &mut serde_json::Map<String, Value>) -> Result<(), String>,
    ) -> Result<HookResult, OperationError> {
        let hook = hook.into();
        let name = hook.name.as_str();
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
        let budget = hook.chain_timeout(&self.resolved.settings);
        let deadline = budget.map(|n| Instant::now() + Duration::from_millis(n));
        for &index in handlers {
            let registration = &self.resolved.handlers[index];
            if !registration
                .registration
                .matcher
                .matches_context(&context, hook.defaults)
            {
                continue;
            }
            let invocation = crate::types::hex_id("hki_");
            let session = context["sessionID"].as_str().unwrap_or("");
            let _guard = InvocationGuard::new(self.active.clone(), &invocation, session);
            let start = Instant::now();
            diagnostic(
                event::HookPhase::Started,
                registration,
                event::HookTarget::Handler(hook.name),
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
            let process_result = self
                .process
                .run(
                    registration,
                    &self.resolved.settings,
                    &self.resolved.directory,
                    &envelope,
                    cancel,
                    deadline,
                    self.capacity.clone(),
                    hook.kind,
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
                            hook.apply_with(response, &mut input, &mut validate)
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
            if self.process.is_shutdown() || cancel.is_some_and(|c| c.is_cancelled()) {
                cancelled = true;
                result = Err("hook cancelled".into());
            }
            if explicit_denial {
                diagnostic_context["hookFailureKind"] = json!("Rejected");
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
                    event::HookPhase::Completed
                } else {
                    event::HookPhase::Failed
                },
                registration,
                event::HookTarget::Handler(hook.name),
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
                        || self.process.is_shutdown()
                        || cancel.is_some_and(|c| c.is_cancelled())
                        || registration.registration.failure_policy == FailurePolicy::Block;
                    if must_stop {
                        let details = artifact
                            .map(|path| format!("\nhook stderr tail: {}", path.display()))
                            .unwrap_or_default();
                        return Err(OperationError {
                            kind: if cancelled {
                                FailureKind::Cancelled
                            } else if explicit_denial {
                                FailureKind::HookRejected
                            } else {
                                FailureKind::Error
                            },
                            message: format!(
                                "hook {} ({name}): {error}{details}",
                                registration.registration.id
                            ),
                        });
                    }
                }
            }
        }
        Ok(HookResult {
            input,
            feedback: None,
        })
    }
    pub fn emit<'a>(&self, event: impl Into<event::Event<'a>>, context: &Value) {
        crate::engine::bus().publish(self.event(event, context));
    }
    pub fn event<'a>(
        &self,
        event: impl Into<event::Event<'a>>,
        context: &Value,
    ) -> Arc<crate::types::BusEvent> {
        Arc::new(event::Envelope::scoped(
            event,
            &self.resolved.directory,
            context,
        ))
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
    phase: event::HookPhase,
    registration: &ResolvedRegistration,
    target: event::HookTarget,
    context: &Value,
    invocation: &str,
    start: Instant,
    error: Option<&str>,
) {
    let data = event::Diagnostic {
        invocation_id: invocation,
        handler_id: &registration.registration.id,
        hook: target,
        source: &registration.source,
        trusted: registration.trusted,
        duration_ms: start.elapsed().as_millis() as u64,
        observer: matches!(target, event::HookTarget::Observer),
        timing: context.get("hookTiming"),
        failure_kind: context.get("hookFailureKind"),
        stderr_artifact: context.get("stderrArtifact"),
        // Never include stdout, stderr, transformed headers or environment values.
        diagnostic: error.map(|_| {
            if context["hookFailureKind"] == "Rejected" {
                "hook rejected the operation"
            } else {
                "hook invocation failed; inspect the originating operation for details"
            }
        }),
    };
    crate::engine::bus().emit(phase.event(data), json!({"version":1,"directory":context["directory"],"sessionID":context["sessionID"],"turnID":context["turnID"],"time":crate::types::now_ms()}));
}

pub fn validate_config(config: &Config, directory: &str) -> Result<(), String> {
    Resolved::from_config(config, directory).map(|_| ())
}
pub fn debug_config(config: &Config, directory: &str) -> Result<Value, String> {
    Resolved::from_config(config, directory).map(|r| r.debug())
}
impl Hooks {
    pub fn prepare_deliveries(
        &self,
        events: &[Arc<crate::types::BusEvent>],
    ) -> Result<Vec<crate::store::OutboxIntent>, String> {
        events.iter().try_fold(Vec::new(), |mut intents, event| {
            intents.extend(events::prepare_committed_in(&self.registry, event)?);
            Ok(intents)
        })
    }
}

#[cfg(test)]
fn test_runtime() -> &'static Arc<Runtime> {
    static RUNTIME: OnceLock<Arc<Runtime>> = OnceLock::new();
    RUNTIME.get_or_init(Runtime::new)
}
#[cfg(test)]
impl Hooks {
    pub fn for_config(config: &Config, directory: &str) -> Result<Arc<Self>, String> {
        test_runtime().install(config, directory)
    }
}
#[cfg(test)]
pub fn prepare_deliveries(
    events: &[Arc<crate::types::BusEvent>],
) -> Result<Vec<crate::store::OutboxIntent>, String> {
    events.iter().try_fold(Vec::new(), |mut intents, event| {
        intents.extend(events::prepare_committed_in(
            &test_runtime().registry,
            event,
        )?);
        Ok(intents)
    })
}

#[cfg(test)]
mod tests;
