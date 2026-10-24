use super::types::{Matcher, Patterns, EVENT_NAMES, HOOK_NAMES};
use super::{
    events::{Delivery, OverflowPolicy},
    payload::PayloadMode,
};
use crate::config::{Config, HookConfigSource};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default, deny_unknown_fields)]
pub struct Settings {
    pub version: u32,
    pub handlers: Vec<Registration>,
    pub observers: Vec<Registration>,
    pub allow_project_hooks: ProjectPolicy,
    pub max_end_continuations: Option<u32>,
    pub chain_timeout_ms: Option<u64>,
    pub finalizer_chain_timeout_ms: Option<u64>,
    pub admission_chain_timeout_ms: u64,
    pub queue_timeout_ms: u64,
    pub max_input_bytes: usize,
    pub max_output_bytes: usize,
    pub max_stderr_bytes: usize,
    pub max_payload_bytes: usize,
    pub max_processes: usize,
    pub max_global_processes: usize,
    pub max_global_observers: usize,
    pub max_pending_hooks: usize,
    pub observer_queue_count: usize,
    pub observer_queue_bytes: usize,
    pub observer_event_bytes: usize,
    pub observer_outbox_count: usize,
    pub observer_outbox_bytes: usize,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            version: 1,
            handlers: vec![],
            observers: vec![],
            allow_project_hooks: ProjectPolicy::Bool(false),
            max_end_continuations: Some(3),
            chain_timeout_ms: None,
            finalizer_chain_timeout_ms: None,
            admission_chain_timeout_ms: 2_000,
            queue_timeout_ms: 30_000,
            max_input_bytes: 1024 * 1024,
            max_output_bytes: 256 * 1024,
            max_stderr_bytes: 64 * 1024,
            max_payload_bytes: 64 * 1024 * 1024,
            max_processes: 8,
            max_global_processes: 16,
            max_global_observers: 4,
            max_pending_hooks: 128,
            observer_queue_count: 128,
            observer_queue_bytes: 4 * 1024 * 1024,
            observer_event_bytes: 128 * 1024,
            observer_outbox_count: 4096,
            observer_outbox_bytes: 64 * 1024 * 1024,
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ProjectPolicy {
    Bool(bool),
    Directories(Vec<String>),
}
impl ProjectPolicy {
    pub fn allows(&self, directory: &str) -> bool {
        match self {
            Self::Bool(v) => *v,
            Self::Directories(p) => p
                .iter()
                .any(|p| glob::Pattern::new(p).is_ok_and(|p| p.matches(directory))),
        }
    }
}
fn yes() -> bool {
    true
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum FailurePolicy {
    #[default]
    Block,
    Continue,
}
fn retry_attempts() -> u32 {
    8
}
fn retry_delay_ms() -> u64 {
    1000
}
fn retention_ms() -> u64 {
    86_400_000
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Registration {
    pub id: String,
    #[serde(default = "empty_patterns")]
    pub on: Patterns,
    #[serde(default)]
    pub command: Vec<String>,
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    #[serde(default)]
    pub queue_timeout_ms: Option<u64>,
    #[serde(default)]
    pub failure_policy: FailurePolicy,
    #[serde(default)]
    pub payload_mode: PayloadMode,
    #[serde(default)]
    pub delivery: Delivery,
    #[serde(default)]
    pub overflow: OverflowPolicy,
    #[serde(default = "retry_attempts")]
    pub retry_attempts: u32,
    #[serde(default = "retry_delay_ms")]
    pub retry_delay_ms: u64,
    #[serde(default = "retention_ms")]
    pub retention_ms: u64,
    #[serde(default, rename = "match")]
    pub matcher: Matcher,
    #[serde(default)]
    pub batch_size: Option<usize>,
    #[serde(default)]
    pub batch_ms: Option<u64>,
    #[serde(default)]
    pub queue_count: Option<usize>,
    #[serde(default)]
    pub queue_bytes: Option<usize>,
    #[serde(default)]
    pub max_input_bytes: Option<usize>,
    #[serde(default)]
    pub max_output_bytes: Option<usize>,
    #[serde(default)]
    pub max_stderr_bytes: Option<usize>,
    #[serde(default)]
    pub terminal_only: Option<bool>,
}
fn empty_patterns() -> Patterns {
    Patterns::Many(vec![])
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ResolvedRegistration {
    pub registration: Registration,
    pub source: String,
    pub trusted: bool,
    pub blocked: Option<String>,
}
impl ResolvedRegistration {
    pub fn active(&self) -> bool {
        self.registration.enabled && self.blocked.is_none()
    }
    pub fn timeout_ms(&self, observer: bool) -> u64 {
        self.registration
            .timeout_ms
            .unwrap_or(if observer { 5000 } else { 30000 })
    }
    pub fn hook_timeout_ms(&self, name: &str) -> u64 {
        self.timeout_ms(matches!(name, "event" | "turn.end.after"))
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Resolved {
    pub version: u32,
    pub directory: String,
    pub settings: Settings,
    pub handlers: Vec<ResolvedRegistration>,
    pub observers: Vec<ResolvedRegistration>,
}
impl Resolved {
    pub fn from_config(config: &Config, directory: &str) -> Result<Self, String> {
        let normalized = normalize_directory(directory);
        let directory = normalized.as_str();
        let fallback;
        let sources = if let Some(sources) = &config.hook_sources {
            sources.as_ref()
        } else {
            fallback = config
                .extra
                .get("ocmini")
                .and_then(|v| v.get("hooks"))
                .map(|value| {
                    vec![HookConfigSource {
                        value: value.clone(),
                        source: "explicit configuration".into(),
                        base: PathBuf::from(directory),
                        trusted: true,
                    }]
                })
                .unwrap_or_default();
            &fallback
        };
        let mut settings = serde_json::to_value(Settings::default()).map_err(|e| e.to_string())?;
        for source in sources.iter().filter(|s| s.trusted) {
            let parsed: Settings = serde_json::from_value(source.value.clone())
                .map_err(|e| format!("hooks in {}: {e}", source.source))?;
            validate_settings(&parsed)?;
            let mut options = source.value.clone();
            let options = options.as_object_mut().ok_or("hooks must be an object")?;
            options.remove("handlers");
            options.remove("observers");
            for (key, value) in options {
                settings[key] = value.clone();
            }
        }
        let mut settings: Settings = serde_json::from_value(settings).map_err(|e| e.to_string())?;
        settings.handlers.clear();
        settings.observers.clear();
        validate_settings(&settings)?;
        let mut result = Self {
            version: 1,
            directory: directory.to_owned(),
            settings,
            handlers: vec![],
            observers: vec![],
        };
        for source in sources
            .iter()
            .filter(|s| s.trusted)
            .chain(sources.iter().filter(|s| !s.trusted))
        {
            let parsed: Settings = serde_json::from_value(source.value.clone())
                .map_err(|e| format!("hooks in {}: {e}", source.source))?;
            validate_settings(&parsed)?;
            for (registrations, observer) in [(parsed.handlers, false), (parsed.observers, true)] {
                let mut seen = HashSet::new();
                for mut registration in registrations {
                    if !seen.insert(registration.id.clone()) {
                        return Err(format!(
                            "duplicate hook id {} in {}",
                            registration.id, source.source
                        ));
                    }
                    validate_registration(&registration, observer)?;
                    resolve_executable(&mut registration.command, &source.base);
                    let target = if observer {
                        &mut result.observers
                    } else {
                        &mut result.handlers
                    };
                    let collision = target
                        .iter()
                        .position(|r| r.registration.id == registration.id);
                    let blocked = if !source.trusted
                        && !result.settings.allow_project_hooks.allows(directory)
                    {
                        Some("project hooks are not allowed by trusted configuration".into())
                    } else if !source.trusted && collision.is_some_and(|i| target[i].trusted) {
                        Some("project configuration cannot override a trusted registration".into())
                    } else {
                        None
                    };
                    let resolved = ResolvedRegistration {
                        registration,
                        source: source.source.clone(),
                        trusted: source.trusted,
                        blocked,
                    };
                    if let Some(i) = collision.filter(|_| resolved.blocked.is_none()) {
                        target[i] = resolved;
                    } else {
                        target.push(resolved);
                    }
                }
            }
        }
        Ok(result)
    }
    pub fn validate_snapshot(&self, directory: &str) -> Result<(), String> {
        if self.version != 1 || self.directory != normalize_directory(directory) {
            return Err("hook snapshot version or directory mismatch".into());
        }
        validate_settings(&self.settings)?;
        for (registrations, observer) in [(&self.handlers, false), (&self.observers, true)] {
            for r in registrations {
                validate_registration(&r.registration, observer)?;
            }
        }
        Ok(())
    }
    pub fn debug(&self) -> Value {
        let effective = self.handlers.iter().map(|r| {
            let boundaries = r.registration.on.values().iter().map(|name| {
                let chain = if super::types::is_admission(name) {
                    Some(self.settings.admission_chain_timeout_ms)
                } else if *name == "turn.end.after" {
                    self.settings.finalizer_chain_timeout_ms
                } else { self.settings.chain_timeout_ms };
                json!({"hook":name,"executionTimeoutMs":r.hook_timeout_ms(name),"queueTimeoutMs":r.registration.queue_timeout_ms.unwrap_or(self.settings.queue_timeout_ms),"chainTimeoutMs":chain,"executionCeilingMs":chain.map(|n|n.min(r.hook_timeout_ms(name))).unwrap_or(r.hook_timeout_ms(name)),"chainIncludesQueue":true})
            }).collect::<Vec<_>>();
            json!({"id":r.registration.id,"boundaries":boundaries})
        }).collect::<Vec<_>>();
        json!({"directory":self.directory,"fingerprint":fingerprint(&serde_json::to_value(self).unwrap_or(Value::Null)),"registrations":self,"catalog":super::types::catalog(),"defaults":Settings::default(),"effectiveBudgets":effective,"processPolicy":{"maxGlobalProcesses":self.settings.max_global_processes,"maxGlobalObservers":self.settings.max_global_observers,"maxPendingHooks":self.settings.max_pending_hooks,"configuration":"latest loaded trusted runtime settings; snapshots do not resize pools"}})
    }
}
pub fn normalize_directory(directory: &str) -> String {
    let path = crate::config::absolute_path(
        Path::new(directory),
        &std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
    );
    path.canonicalize()
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned()
}
pub fn fingerprint(value: &Value) -> String {
    // Canonical key ordering makes object insertion order irrelevant.
    fn feed(value: &Value, hash: &mut u64) {
        match value {
            Value::Object(map) => {
                let mut keys = map.keys().collect::<Vec<_>>();
                keys.sort();
                add(hash, b"{");
                for key in keys {
                    add(hash, &serde_json::to_vec(key).unwrap_or_default());
                    add(hash, b":");
                    feed(&map[key], hash);
                    add(hash, b",");
                }
                add(hash, b"}");
            }
            Value::Array(a) => {
                add(hash, b"[");
                for v in a {
                    feed(v, hash);
                    add(hash, b",");
                }
                add(hash, b"]");
            }
            v => add(hash, &serde_json::to_vec(v).unwrap_or_default()),
        }
    }
    fn add(hash: &mut u64, bytes: &[u8]) {
        for byte in bytes {
            *hash ^= *byte as u64;
            *hash = hash.wrapping_mul(0x100000001b3);
        }
    }
    let mut hash = 0xcbf29ce484222325;
    feed(value, &mut hash);
    format!("hooks-v1-{hash:016x}")
}
fn bound(name: &str, value: u64, max: u64) -> Result<(), String> {
    if value == 0 || value > max {
        Err(format!("{name} must be between 1 and {max}"))
    } else {
        Ok(())
    }
}
fn duration(name: &str, value: u64) -> Result<(), String> {
    if value == 0
        || std::time::Instant::now()
            .checked_add(std::time::Duration::from_millis(value))
            .is_none()
    {
        return Err(format!(
            "{name} must be positive and representable on this platform"
        ));
    }
    Ok(())
}
fn bytes(name: &str, value: usize) -> Result<(), String> {
    bound(name, value as u64, isize::MAX as u64)
}
pub fn validate_settings(s: &Settings) -> Result<(), String> {
    if s.version != 1 {
        return Err("unsupported ocmini.hooks.version (expected 1)".into());
    }
    if let Some(n) = s.chain_timeout_ms {
        duration("chainTimeoutMs", n)?;
    }
    if let Some(n) = s.finalizer_chain_timeout_ms {
        duration("finalizerChainTimeoutMs", n)?;
    }
    duration("queueTimeoutMs", s.queue_timeout_ms)?;
    bound(
        "admissionChainTimeoutMs",
        s.admission_chain_timeout_ms,
        5000,
    )?;
    for (name, value) in [
        ("maxInputBytes", s.max_input_bytes),
        ("maxOutputBytes", s.max_output_bytes),
        ("maxStderrBytes", s.max_stderr_bytes),
        ("maxPayloadBytes", s.max_payload_bytes),
        ("observerQueueBytes", s.observer_queue_bytes),
        ("observerEventBytes", s.observer_event_bytes),
        ("observerOutboxBytes", s.observer_outbox_bytes),
    ] {
        bytes(name, value)?;
    }
    for (name, value) in [
        ("maxProcesses", s.max_processes),
        ("maxGlobalProcesses", s.max_global_processes),
        ("maxGlobalObservers", s.max_global_observers),
        ("maxPendingHooks", s.max_pending_hooks),
        ("observerQueueCount", s.observer_queue_count),
        ("observerOutboxCount", s.observer_outbox_count),
    ] {
        bound(
            name,
            value as u64,
            tokio::sync::Semaphore::MAX_PERMITS.min(u32::MAX as usize) as u64,
        )?;
    }
    if let ProjectPolicy::Directories(patterns) = &s.allow_project_hooks {
        for p in patterns {
            glob::Pattern::new(p).map_err(|e| format!("allowProjectHooks: {e}"))?;
        }
    }
    Ok(())
}
fn validate_registration(r: &Registration, observer: bool) -> Result<(), String> {
    if r.id.is_empty() || r.id.len() > 128 || r.id.chars().any(char::is_control) {
        return Err("hook id must contain 1..128 printable characters".into());
    }
    if r.enabled && (r.command.is_empty() || r.command[0].is_empty() || r.on.values().is_empty()) {
        return Err(format!("hook {} requires on and command", r.id));
    }
    if r.command.len() > 256
        || r.command.iter().map(String::len).sum::<usize>() > 65536
        || r.command.iter().any(|arg| arg.contains('\0'))
    {
        return Err(format!("invalid command for hook {}", r.id));
    }
    let mut names = HashSet::new();
    for name in r.on.values() {
        if !names.insert(name) {
            return Err(format!("duplicate boundary {name} for hook {}", r.id));
        }
        if observer {
            glob::Pattern::new(name).map_err(|e| format!("invalid event pattern: {e}"))?;
            if !EVENT_NAMES
                .iter()
                .any(|event| glob::Pattern::new(name).is_ok_and(|p| p.matches(event)))
            {
                return Err(format!("unknown event name {name}"));
            }
            if name == "message.part.delta"
                && (r.batch_size.unwrap_or(1) < 2 || r.batch_ms.unwrap_or(0) == 0)
            {
                return Err("delta subscriptions require batchSize >= 2 and batchMs > 0".into());
            }
        } else if !HOOK_NAMES.contains(&name) {
            return Err(format!("unknown hook name {name}"));
        }
    }
    r.matcher.validate()?;
    if let Some(n) = r.timeout_ms {
        duration("timeoutMs", n)?;
    }
    if let Some(n) = r.queue_timeout_ms {
        duration("queueTimeoutMs", n)?;
    }
    for (name, n) in [
        ("queueBytes", r.queue_bytes),
        ("maxInputBytes", r.max_input_bytes),
        ("maxOutputBytes", r.max_output_bytes),
        ("maxStderrBytes", r.max_stderr_bytes),
    ] {
        if let Some(n) = n {
            bytes(name, n)?;
        }
    }
    if let Some(n) = r.batch_size {
        bound("batchSize", n as u64, 256)?;
    }
    if let Some(n) = r.queue_count {
        bound("queueCount", n as u64, u32::MAX as u64)?;
    }
    if r.delivery == Delivery::Durable {
        if !observer
            || r.on
                .values()
                .iter()
                .any(|n| !matches!(*n, "ocmini.turn.ended" | "command.executed"))
        {
            return Err("durable delivery requires exact ocmini.turn.ended or command.executed subscriptions".into());
        }
        if r.retry_attempts == 0 {
            return Err("retryAttempts must be positive".into());
        }
        duration("retryDelayMs", r.retry_delay_ms)?;
        duration("retentionMs", r.retention_ms)?;
        if r.retention_ms > i64::MAX as u64 / 2 || r.retry_delay_ms > i64::MAX as u64 / 2 {
            return Err("durable delivery durations exceed the database timestamp range".into());
        }
        if r.overflow != OverflowPolicy::DropNewest {
            return Err("durable delivery does not discard or coalesce accepted events".into());
        }
    }
    if let Some(n) = r.batch_ms {
        bound("batchMs", n, 5000)?;
    }
    if !observer
        && (r.batch_size.is_some()
            || r.batch_ms.is_some()
            || r.queue_count.is_some()
            || r.queue_bytes.is_some()
            || r.terminal_only.is_some()
            || r.overflow != OverflowPolicy::DropNewest
            || r.delivery != Delivery::BestEffort)
    {
        return Err("observer options are not valid for awaited hooks".into());
    }
    Ok(())
}
fn resolve_executable(command: &mut [String], base: &Path) {
    if let Some(executable) = command.first_mut() {
        let path = Path::new(executable);
        if !path.is_absolute() && (executable.contains('/') || executable.contains('\\')) {
            *executable = crate::config::absolute_path(path, base)
                .to_string_lossy()
                .into_owned();
        }
    }
}
