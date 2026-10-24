use super::{config::ResolvedRegistration, config::Settings, process, ActiveInvocations};
use crate::types::BusEvent;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use tokio::sync::{Notify, Semaphore};
use tokio::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum Delivery {
    #[default]
    BestEffort,
    Durable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum OverflowPolicy {
    #[default]
    DropNewest,
    DropOldest,
    Coalesce,
}

#[derive(Serialize)]
struct ObserverEnvelope<'a, E: Serialize> {
    version: u32,
    #[serde(rename = "subscriptionID")]
    subscription_id: &'a str,
    #[serde(rename = "invocationID")]
    invocation_id: &'a str,
    context: ObserverContext<'a>,
    events: &'a [E],
    #[serde(skip_serializing_if = "Option::is_none")]
    dropped: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    gaps: Option<u64>,
}

#[derive(Serialize)]
struct ObserverContext<'a> {
    directory: &'a str,
}

static COUNT: AtomicUsize = AtomicUsize::new(0);
static DELTA_COUNT: AtomicUsize = AtomicUsize::new(0);
static OBSERVERS: OnceLock<Mutex<Vec<Weak<Observer>>>> = OnceLock::new();
static DIRECTORIES: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();

pub fn remember(context: &Value) {
    if let (Some(session), Some(directory)) =
        (context["sessionID"].as_str(), context["directory"].as_str())
    {
        let mut directories = DIRECTORIES.get_or_init(Default::default).lock().unwrap();
        if directories.len() >= 8192 && !directories.contains_key(session) {
            if let Some(old) = directories.keys().next().cloned() {
                directories.remove(&old);
            }
        }
        directories.insert(session.to_owned(), directory.to_owned());
    }
}

pub fn has_observers() -> bool {
    COUNT.load(Ordering::Relaxed) > 0
}

pub fn subscription_key(directory: &str, reg: &ResolvedRegistration) -> String {
    let norm = super::config::normalize_directory(directory);
    let val = serde_json::to_value(&reg.registration).unwrap_or(Value::Null);
    let fp = super::config::fingerprint(&val);
    format!("{}:{}:{}", norm, reg.registration.id, fp)
}

pub fn persist_committed(
    tx: &rusqlite::Transaction<'_>,
    event: &BusEvent,
) -> Result<Vec<String>, String> {
    if event.r#type != "ocmini.turn.ended" && event.r#type != "command.executed" {
        return Ok(Vec::new());
    }
    let context = context_for(event);
    let subscribers = {
        let mut registry = OBSERVERS.get_or_init(Default::default).lock().unwrap();
        registry.retain(|o| o.strong_count() > 0);
        registry
            .iter()
            .filter_map(Weak::upgrade)
            .filter(|o| o.registration.registration.delivery == Delivery::Durable)
            .filter(|o| o.registration.active())
            .filter(|o| o.accepts(event, &context))
            .collect::<Vec<_>>()
    };
    persist_subscriptions(
        tx,
        event,
        subscribers
            .iter()
            .map(|o| (&o.registration, o.settings.as_ref(), o.directory.as_str())),
    )
}

/// Recovery must not depend on which directory has live observer workers.
/// Record deliveries from the admitted snapshot; only a worker with the same
/// current registration key can later execute them.
pub fn persist_recovered(
    tx: &rusqlite::Transaction<'_>,
    event: &BusEvent,
    snapshot: Option<&str>,
) -> Result<Vec<String>, String> {
    let Some(snapshot) = snapshot else {
        return persist_committed(tx, event);
    };
    let resolved: super::config::Resolved = serde_json::from_str(snapshot)
        .map_err(|e| format!("invalid recovered hook snapshot: {e}"))?;
    let directory = event.properties["directory"]
        .as_str()
        .ok_or("recovered hook event has no directory")?;
    resolved.validate_snapshot(directory)?;
    let context = context_for(event);
    persist_subscriptions(
        tx,
        event,
        resolved
            .observers
            .iter()
            .filter(|r| {
                r.active()
                    && r.registration.delivery == Delivery::Durable
                    && r.registration.on.values().contains(&event.r#type.as_str())
                    && r.registration
                        .matcher
                        .matches(&event.r#type, &context, false)
            })
            .map(|r| (r, &resolved.settings, resolved.directory.as_str())),
    )
}

fn persist_subscriptions<'a>(
    tx: &rusqlite::Transaction<'_>,
    event: &BusEvent,
    subscriptions: impl Iterator<Item = (&'a ResolvedRegistration, &'a Settings, &'a str)>,
) -> Result<Vec<String>, String> {
    let mut subscriptions = subscriptions.peekable();
    if subscriptions.peek().is_none() {
        return Ok(Vec::new());
    }
    let event_json =
        serde_json::to_string(event).map_err(|e| format!("cannot serialize event: {e}"))?;

    let mut rejected = Vec::new();
    for (registration, settings, directory) in subscriptions {
        let r = &registration.registration;
        let count_limit = r.queue_count.unwrap_or(settings.observer_queue_count);
        let bytes_limit = r.queue_bytes.unwrap_or(settings.observer_queue_bytes);
        let retention_ms = r.retention_ms;

        let budget = crate::store::HookDeliveryBudget {
            retention_ms,
            count: count_limit,
            bytes: bytes_limit,
            total_count: settings.observer_outbox_count,
            total_bytes: settings.observer_outbox_bytes,
        };
        // Database errors may have ended the SQLite transaction. Propagate
        // immediately so no later enqueue or cleanup can autocommit.
        let accepted = crate::store::Store::persist_hook_delivery_tx(
            tx,
            &subscription_key(directory, registration),
            &event.id,
            directory,
            &event_json,
            &budget,
        )?;

        if !accepted {
            rejected.push(format!(
                "observer outbox quota exceeded for subscription {}",
                r.id
            ));
        }
    }

    Ok(rejected)
}

pub fn observe(event: &Arc<BusEvent>) {
    if !has_observers() {
        return;
    }
    if event.r#type == "message.part.delta" && DELTA_COUNT.load(Ordering::Relaxed) == 0 {
        return;
    }
    if event.r#type == "server.heartbeat"
        || event.r#type == "server.connected"
        || event.r#type == "ocmini.hook.queue.overflow"
    {
        return;
    }
    let context = context_for(event);
    let subscribers = {
        let mut registry = OBSERVERS.get_or_init(Default::default).lock().unwrap();
        registry.retain(|o| o.strong_count() > 0);
        registry
            .iter()
            .filter_map(Weak::upgrade)
            .filter(|o| o.accepts(event, &context))
            .collect::<Vec<_>>()
    };
    if subscribers.is_empty() {
        return;
    }
    // Count once up to the largest size any best-effort subscriber can retain.
    // File subscribers keep the full event rather than an inline preview.
    let size = subscribers
        .iter()
        .filter(|o| o.registration.registration.delivery != Delivery::Durable)
        .map(|o| {
            let r = &o.registration.registration;
            if r.payload_mode == super::payload::PayloadMode::File {
                o.settings
                    .max_payload_bytes
                    .min(r.queue_bytes.unwrap_or(o.settings.observer_queue_bytes))
            } else {
                o.settings.observer_event_bytes.min(
                    r.max_input_bytes
                        .unwrap_or(o.settings.max_input_bytes)
                        .saturating_sub(1024),
                )
            }
        })
        .max()
        .and_then(|limit| encoded_size(event.as_ref(), limit));
    for observer in subscribers {
        if observer.registration.registration.delivery == Delivery::Durable {
            observer.durable_notify.notify_one();
        } else {
            observer.enqueue(event, size);
        }
    }
}

fn is_nonterminal_part_updated(event: &BusEvent) -> bool {
    if event.r#type != "message.part.updated" {
        return false;
    }
    let part = &event.properties["part"];
    let terminal = match part["type"].as_str() {
        Some("text" | "reasoning") => part["time"]["end"].is_number(),
        Some("tool") => matches!(
            part["state"]["status"].as_str(),
            Some("completed" | "error")
        ),
        _ => true,
    };
    !terminal
}

fn coalesce_key(event: &BusEvent) -> Option<String> {
    if event.r#type == "session.status" {
        let sid = event.properties["sessionID"]
            .as_str()
            .or_else(|| event.properties["info"]["id"].as_str())?;
        Some(format!("session.status:{sid}"))
    } else if is_nonterminal_part_updated(event) {
        let part = &event.properties["part"];
        let sid = part["sessionID"].as_str().unwrap_or("");
        let mid = part["messageID"].as_str().unwrap_or("");
        let pid = part["id"].as_str().unwrap_or("");
        Some(format!("part.updated:{sid}:{mid}:{pid}"))
    } else {
        None
    }
}

fn context_for(event: &BusEvent) -> Value {
    let p = &event.properties;
    let session = p["sessionID"]
        .as_str()
        .or(p["info"]["sessionID"].as_str())
        .or(p["part"]["sessionID"].as_str())
        .or_else(|| {
            if event.r#type.starts_with("session.") {
                p["info"]["id"].as_str()
            } else {
                None
            }
        });
    let directory = p["directory"]
        .as_str()
        .or(p["info"]["directory"].as_str())
        .map(str::to_owned)
        .or_else(|| {
            session.and_then(|id| {
                DIRECTORIES
                    .get_or_init(Default::default)
                    .lock()
                    .unwrap()
                    .get(id)
                    .cloned()
            })
        });
    let mut context = serde_json::Map::new();
    for key in [
        "turnKind",
        "requestKind",
        "agent",
        "model",
        "parentID",
        "parentSessionID",
        "tool",
        "toolName",
        "outcome",
        "reason",
    ] {
        if let Some(value) = p.get(key) {
            context.insert(key.to_owned(), value.clone());
        }
    }
    if let Some(session) = session {
        context.insert("sessionID".into(), json!(session));
    }
    if let Some(directory) = directory {
        context.insert("directory".into(), json!(directory));
    }
    if let Some(parent) = p["info"].get("parentID") {
        context.insert("parentID".into(), parent.clone());
    }
    let value = Value::Object(context);
    remember(&value);
    value
}

struct SizeWriter {
    size: usize,
    limit: usize,
}
impl Write for SizeWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.size = self.size.saturating_add(bytes.len());
        if self.size > self.limit {
            return Err(io::Error::other("event preview bound"));
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

// Only return exact sizes; an exceeded counting bound is not a retained size.
fn encoded_size(value: &impl Serialize, limit: usize) -> Option<usize> {
    let mut writer = SizeWriter { size: 0, limit };
    if serde_json::to_writer(&mut writer, value).is_err() {
        None
    } else {
        Some(writer.size)
    }
}

struct Item {
    event: Arc<BusEvent>,
    bytes: usize,
    coalesce_key: Option<String>,
}

#[derive(Default)]
struct Queue {
    items: VecDeque<Item>,
    bytes: usize,
    dropped: u64,
    gaps: u64,
}

pub struct Observer {
    pub registration: ResolvedRegistration,
    pub subscription_key: String,
    on: Vec<glob::Pattern>,
    deltas: bool,
    pub settings: Arc<Settings>,
    pub directory: String,
    capacity: Arc<Semaphore>,
    active: ActiveInvocations,
    queue: Mutex<Queue>,
    notify: Arc<Notify>,
    started: AtomicBool,
    retired: Arc<AtomicBool>,
    store: Mutex<Weak<crate::store::Store>>,
    durable_started: AtomicBool,
    durable_notify: Arc<Notify>,
}

impl Drop for Observer {
    fn drop(&mut self) {
        self.retire();
    }
}

pub fn retire_scope(directory: &str) {
    if let Some(registry) = OBSERVERS.get() {
        let observers = registry
            .lock()
            .unwrap()
            .iter()
            .filter_map(Weak::upgrade)
            .filter(|o| o.directory == directory)
            .collect::<Vec<_>>();
        for observer in observers {
            observer.retire();
        }
    }
}

impl Observer {
    fn retire(&self) {
        if !self.retired.swap(true, Ordering::AcqRel) {
            COUNT.fetch_sub(1, Ordering::Relaxed);
            if self.deltas {
                DELTA_COUNT.fetch_sub(1, Ordering::Relaxed);
            }
            let mut queue = self.queue.lock().unwrap();
            queue.items.clear();
            queue.bytes = 0;
            self.notify.notify_one();
            self.durable_notify.notify_one();
        }
    }

    pub fn attach_store(self: &Arc<Self>, store: Arc<crate::store::Store>) {
        if self.registration.registration.delivery != Delivery::Durable {
            return;
        }
        *self.store.lock().unwrap() = Arc::downgrade(&store);
        self.start_durable_worker();
        self.durable_notify.notify_one();
    }

    fn start_durable_worker(self: &Arc<Self>) {
        if !self.durable_started.swap(true, Ordering::AcqRel) {
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                let weak = Arc::downgrade(self);
                runtime.spawn(async move {
                    durable_worker(weak).await;
                });
            }
        }
    }

    pub fn register(
        registration: ResolvedRegistration,
        settings: Arc<Settings>,
        directory: &str,
        capacity: Arc<Semaphore>,
        active: ActiveInvocations,
    ) -> Arc<Self> {
        let on = registration
            .registration
            .on
            .values()
            .iter()
            .filter_map(|p| glob::Pattern::new(p).ok())
            .collect::<Vec<_>>();
        let deltas = registration.registration.batch_size.unwrap_or(1) >= 2
            && registration.registration.batch_ms.unwrap_or(0) > 0
            && on.iter().any(|p| p.matches("message.part.delta"));
        let sub_key = subscription_key(directory, &registration);
        let observer = Arc::new(Self {
            subscription_key: sub_key,
            on,
            deltas,
            registration,
            settings,
            directory: directory.to_owned(),
            capacity,
            active,
            queue: Mutex::new(Queue::default()),
            notify: Arc::new(Notify::new()),
            started: AtomicBool::new(false),
            retired: Arc::new(AtomicBool::new(false)),
            store: Mutex::new(Weak::new()),
            durable_started: AtomicBool::new(false),
            durable_notify: Arc::new(Notify::new()),
        });
        OBSERVERS
            .get_or_init(Default::default)
            .lock()
            .unwrap()
            .push(Arc::downgrade(&observer));
        COUNT.fetch_add(1, Ordering::Relaxed);
        if observer.deltas {
            DELTA_COUNT.fetch_add(1, Ordering::Relaxed);
        }
        observer
    }

    fn accepts(&self, event: &BusEvent, context: &Value) -> bool {
        let r = &self.registration.registration;
        if self.retired.load(Ordering::Acquire)
            || !r.enabled
            || !self.on.iter().any(|p| p.matches(&event.r#type))
        {
            return false;
        }
        if event.r#type.starts_with("ocmini.hook.")
            && (event.properties["observer"] == true
                || !r.on.values().contains(&event.r#type.as_str())
                || event.properties["handlerID"] == r.id)
        {
            return false;
        }
        if event.r#type == "message.part.delta"
            && (r.batch_size.unwrap_or(1) < 2 || r.batch_ms.unwrap_or(0) == 0)
        {
            return false;
        }
        if context["directory"].as_str() != Some(self.directory.as_str())
            && !event.r#type.starts_with("ocmini.runtime.")
        {
            return false;
        }
        if event.r#type == "message.part.updated" && r.terminal_only.unwrap_or(true) {
            let part = &event.properties["part"];
            let terminal = match part["type"].as_str() {
                Some("text" | "reasoning") => part["time"]["end"].is_number(),
                Some("tool") => matches!(
                    part["state"]["status"].as_str(),
                    Some("completed" | "error")
                ),
                _ => true,
            };
            if !terminal {
                return false;
            }
        }
        r.matcher.matches(&event.r#type, context, false)
    }

    fn enqueue(self: &Arc<Self>, event: &Arc<BusEvent>, size: Option<usize>) {
        let r = &self.registration.registration;
        let is_file_mode = r.payload_mode == super::payload::PayloadMode::File;
        let count_limit = r.queue_count.unwrap_or(self.settings.observer_queue_count);
        let bytes_limit = r.queue_bytes.unwrap_or(self.settings.observer_queue_bytes);

        let (event, size) = if is_file_mode {
            (
                event.clone(),
                size.filter(|size| {
                    *size <= self.settings.max_payload_bytes && *size <= bytes_limit
                }),
            )
        } else {
            let limit = self.settings.observer_event_bytes.min(
                r.max_input_bytes
                    .unwrap_or(self.settings.max_input_bytes)
                    .saturating_sub(1024),
            );
            if size.is_none_or(|size| size > limit) {
                let context = context_for(event);
                let mut properties = json!({
                    "directory": self.directory,
                    "sessionID": context["sessionID"],
                    "truncated": true,
                    "originalEventID": event.id,
                    "preview": "event exceeds observerEventBytes; use its persisted resource reference"
                });
                for key in ["messageID", "partID", "turnID", "requestID", "callID"] {
                    if let Some(value) = event.properties.get(key) {
                        properties[key] = value.clone();
                    }
                }
                if let Some(part) = event.properties.get("part") {
                    properties["messageID"] = part["messageID"].clone();
                    properties["partID"] = part["id"].clone();
                } else if event.r#type == "message.updated" {
                    properties["messageID"] = event.properties["info"]["id"].clone();
                }
                let event = Arc::new(BusEvent {
                    id: event.id.clone(),
                    r#type: event.r#type.clone(),
                    properties,
                });
                let size = encoded_size(event.as_ref(), bytes_limit);
                (event, size)
            } else {
                (event.clone(), size)
            }
        };

        let Some(size) = size else {
            let mut queue = self.queue.lock().unwrap();
            queue.dropped += 1;
            queue.gaps += 1;
            let dropped = queue.dropped;
            drop(queue);
            if dropped.is_power_of_two() {
                crate::engine::bus().emit(
                    "ocmini.hook.queue.overflow",
                    json!({
                        "version": 1,
                        "directory": self.directory,
                        "time": crate::types::now_ms(),
                        "subscriptionID": r.id,
                        "dropped": dropped
                    }),
                );
            }
            return;
        };

        let overflow = {
            let mut queue = self.queue.lock().unwrap();
            match r.overflow {
                OverflowPolicy::DropNewest => {
                    if queue.items.len() >= count_limit
                        || size > bytes_limit.saturating_sub(queue.bytes)
                    {
                        queue.dropped += 1;
                        queue.gaps += 1;
                        Some(queue.dropped)
                    } else {
                        queue.bytes += size;
                        queue.items.push_back(Item {
                            event,
                            bytes: size,
                            coalesce_key: None,
                        });
                        None
                    }
                }
                OverflowPolicy::DropOldest => {
                    while (queue.items.len() >= count_limit
                        || size > bytes_limit.saturating_sub(queue.bytes))
                        && !queue.items.is_empty()
                    {
                        let old = queue.items.pop_front().unwrap();
                        queue.bytes -= old.bytes;
                        queue.dropped += 1;
                        queue.gaps += 1;
                    }
                    if queue.items.len() < count_limit
                        && size <= bytes_limit.saturating_sub(queue.bytes)
                    {
                        queue.bytes += size;
                        queue.items.push_back(Item {
                            event,
                            bytes: size,
                            coalesce_key: None,
                        });
                        None
                    } else {
                        queue.dropped += 1;
                        queue.gaps += 1;
                        Some(queue.dropped)
                    }
                }
                OverflowPolicy::Coalesce => {
                    let key = coalesce_key(&event);
                    if let Some(key) = key {
                        if let Some(pos) = queue
                            .items
                            .iter()
                            .position(|i| i.coalesce_key.as_deref() == Some(&key))
                        {
                            let old_bytes = queue.items[pos].bytes;
                            let new_total_bytes = queue.bytes - old_bytes + size;
                            if new_total_bytes <= bytes_limit {
                                queue.bytes = new_total_bytes;
                                queue.items.remove(pos);
                                queue.items.push_back(Item {
                                    event,
                                    bytes: size,
                                    coalesce_key: Some(key),
                                });
                                queue.gaps += 1;
                                None
                            } else {
                                queue.dropped += 1;
                                queue.gaps += 1;
                                Some(queue.dropped)
                            }
                        } else if queue.items.len() >= count_limit
                            || size > bytes_limit.saturating_sub(queue.bytes)
                        {
                            queue.dropped += 1;
                            queue.gaps += 1;
                            Some(queue.dropped)
                        } else {
                            queue.bytes += size;
                            queue.items.push_back(Item {
                                event,
                                bytes: size,
                                coalesce_key: Some(key),
                            });
                            None
                        }
                    } else if queue.items.len() >= count_limit
                        || size > bytes_limit.saturating_sub(queue.bytes)
                    {
                        queue.dropped += 1;
                        queue.gaps += 1;
                        Some(queue.dropped)
                    } else {
                        queue.bytes += size;
                        queue.items.push_back(Item {
                            event,
                            bytes: size,
                            coalesce_key: None,
                        });
                        None
                    }
                }
            }
        };

        if let Some(dropped) = overflow {
            if dropped.is_power_of_two() {
                crate::engine::bus().emit(
                    "ocmini.hook.queue.overflow",
                    json!({
                        "version": 1,
                        "directory": self.directory,
                        "time": crate::types::now_ms(),
                        "subscriptionID": r.id,
                        "dropped": dropped
                    }),
                );
            }
            return;
        }

        if !self.started.load(Ordering::Acquire) {
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                if !self.started.swap(true, Ordering::AcqRel) {
                    let weak = Arc::downgrade(self);
                    runtime.spawn(async move {
                        worker(weak).await;
                    });
                }
            }
        }
        self.notify.notify_one();
    }
}

async fn worker(weak: Weak<Observer>) {
    loop {
        if process::is_shutdown() {
            return;
        }
        let Some(observer) = weak.upgrade() else {
            return;
        };
        if observer.retired.load(Ordering::Acquire) {
            return;
        }
        let notify = observer.notify.clone();
        let batch_size = observer.registration.registration.batch_size.unwrap_or(1);
        let batch_ms = observer.registration.registration.batch_ms.unwrap_or(0);
        let empty = observer.queue.lock().unwrap().items.is_empty();
        if empty {
            drop(observer);
            tokio::select! {
                _ = notify.notified() => {},
                _ = process::cancelled(None) => return,
            };
            continue;
        }
        if batch_ms > 0 {
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(batch_ms)) => {},
                _ = process::cancelled(None) => return,
            };
        }
        let (events, gaps) = {
            let mut q = observer.queue.lock().unwrap();
            let mut events = Vec::new();
            let budget = if observer.registration.registration.payload_mode
                == super::payload::PayloadMode::File
            {
                observer.settings.max_payload_bytes
            } else {
                observer
                    .registration
                    .registration
                    .max_input_bytes
                    .unwrap_or(observer.settings.max_input_bytes)
                    .saturating_sub(1024)
            };
            let mut bytes = 0;
            while events.len() < batch_size {
                let Some(item) = q.items.front() else {
                    break;
                };
                if !events.is_empty() && bytes + item.bytes > budget {
                    break;
                }
                let item = q.items.pop_front().unwrap();
                q.bytes -= item.bytes;
                bytes += item.bytes;
                events.push(item.event);
            }
            let gaps = q.gaps;
            q.gaps = 0;
            (events, gaps)
        };
        let invocation = crate::types::hex_id("hki_");
        let context = events
            .first()
            .map(|e| context_for(e))
            .unwrap_or_else(|| json!({"directory": observer.directory}));
        let guard = super::InvocationGuard::new(
            observer.active.clone(),
            &invocation,
            context["sessionID"].as_str().unwrap_or(""),
        );
        let start = Instant::now();
        super::diagnostic(
            "started",
            &observer.registration,
            "event",
            &context,
            &invocation,
            start,
            None,
        );

        let event_refs: Vec<&BusEvent> = events.iter().map(Arc::as_ref).collect();
        let envelope = ObserverEnvelope {
            version: 1,
            subscription_id: &observer.registration.registration.id,
            invocation_id: &invocation,
            context: ObserverContext {
                directory: &observer.directory,
            },
            events: &event_refs,
            dropped: if gaps > 0 { Some(gaps) } else { None },
            gaps: if gaps > 0 { Some(gaps) } else { None },
        };

        let result = process::run(
            &observer.registration,
            &observer.settings,
            &observer.directory,
            &envelope,
            Some(&observer.retired),
            None,
            observer.capacity.clone(),
            "event",
            &invocation,
            context["sessionID"].as_str().unwrap_or(""),
        )
        .await;

        let ack_res = match &result {
            Ok(outcome) => validate_outcome_ack(outcome, observer.settings.max_payload_bytes),
            Err(e) => Err(e.summary().to_string()),
        };

        let err_msg = ack_res.as_ref().err().map(|s| s.as_str());
        let diagnostic_context =
            observer_diagnostic_context(&context, &result, ack_res.is_err(), &invocation);
        super::diagnostic(
            if ack_res.is_ok() {
                "completed"
            } else {
                "failed"
            },
            &observer.registration,
            "event",
            &diagnostic_context,
            &invocation,
            start,
            err_msg,
        );
        drop(guard);
    }
}

async fn durable_worker(weak: Weak<Observer>) {
    loop {
        if process::is_shutdown() {
            return;
        }
        let Some(observer) = weak.upgrade() else {
            return;
        };
        if observer.retired.load(Ordering::Acquire) {
            return;
        }
        let store = observer.store.lock().unwrap().upgrade();
        let Some(store) = store else {
            let notify = observer.durable_notify.clone();
            drop(observer);
            tokio::select! {
                _ = notify.notified() => {},
                _ = process::cancelled(None) => return,
            };
            continue;
        };

        let r = &observer.registration.registration;
        let queue_timeout_ms = r
            .queue_timeout_ms
            .unwrap_or(observer.settings.queue_timeout_ms);
        let cmd_timeout_ms = observer.registration.timeout_ms(true);
        let lease_ms = (queue_timeout_ms
            .saturating_add(cmd_timeout_ms)
            .saturating_add(10_000))
        .min(i64::MAX as u64) as i64;

        let claimed =
            store.claim_hook_delivery(&observer.subscription_key, lease_ms, r.retry_attempts);
        let rec = match claimed {
            Ok(Some(r)) => r,
            Ok(None) => {
                // Sleep until next due timestamp, or wait for notify / 60s fallback
                let next_due = store
                    .next_hook_delivery_at(&observer.subscription_key)
                    .ok()
                    .flatten();
                let notify = observer.durable_notify.clone();
                drop(observer);
                drop(store);

                let sleep_duration = match next_due {
                    Some(due) => {
                        let now = crate::types::now_ms();
                        Duration::from_millis((due.saturating_sub(now)).max(0) as u64)
                            .min(Duration::from_secs(60))
                    }
                    None => Duration::from_secs(60),
                };

                tokio::select! {
                    _ = notify.notified() => {},
                    _ = tokio::time::sleep(sleep_duration) => {},
                    _ = process::cancelled(None) => return,
                };
                continue;
            }
            Err(_) => {
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };

        let lease_token = rec.lease_owner.clone().unwrap_or_default();
        if observer.retired.load(Ordering::Acquire) || process::is_shutdown() {
            let _ = store.release_hook_delivery(&rec.subscription_key, &rec.event_id, &lease_token);
            return;
        }

        let Ok(event): Result<BusEvent, _> = serde_json::from_str(&rec.event_json) else {
            let _ =
                store.fail_hook_delivery(&rec.subscription_key, &rec.event_id, &lease_token, 1, 0);
            continue;
        };

        let invocation = crate::types::hex_id("hki_");
        let context = context_for(&event);
        let guard = super::InvocationGuard::new(
            observer.active.clone(),
            &invocation,
            context["sessionID"].as_str().unwrap_or(""),
        );
        let start = Instant::now();
        super::diagnostic(
            "started",
            &observer.registration,
            "event",
            &context,
            &invocation,
            start,
            None,
        );

        let event_slice = [&event];
        let envelope = ObserverEnvelope {
            version: 1,
            subscription_id: &observer.registration.registration.id,
            invocation_id: &invocation,
            context: ObserverContext {
                directory: &observer.directory,
            },
            events: &event_slice,
            dropped: None,
            gaps: None,
        };

        let result = process::run(
            &observer.registration,
            &observer.settings,
            &observer.directory,
            &envelope,
            Some(&observer.retired),
            Some(
                start
                    + Duration::from_millis(
                        rec.expires.saturating_sub(crate::types::now_ms()).max(0) as u64,
                    ),
            ),
            observer.capacity.clone(),
            "event",
            &invocation,
            context["sessionID"].as_str().unwrap_or(""),
        )
        .await;

        let ack_res = match &result {
            Ok(outcome) => validate_outcome_ack(outcome, observer.settings.max_payload_bytes),
            Err(e) => Err(e.summary().to_string()),
        };

        let err_msg = ack_res.as_ref().err().map(|s| s.as_str());
        let diagnostic_context =
            observer_diagnostic_context(&context, &result, ack_res.is_err(), &invocation);
        super::diagnostic(
            if ack_res.is_ok() {
                "completed"
            } else {
                "failed"
            },
            &observer.registration,
            "event",
            &diagnostic_context,
            &invocation,
            start,
            err_msg,
        );
        drop(guard);

        if observer.retired.load(Ordering::Acquire) || process::is_shutdown() {
            let _ = store.release_hook_delivery(&rec.subscription_key, &rec.event_id, &lease_token);
            return;
        }

        match ack_res {
            Ok(()) => {
                let _ = store.complete_hook_delivery(
                    &rec.subscription_key,
                    &rec.event_id,
                    &lease_token,
                );
            }
            Err(_) => {
                let r = &observer.registration.registration;
                let _ = store.fail_hook_delivery(
                    &rec.subscription_key,
                    &rec.event_id,
                    &lease_token,
                    r.retry_attempts,
                    r.retry_delay_ms,
                );
            }
        }
    }
}

fn observer_diagnostic_context(
    context: &Value,
    result: &Result<process::ProcessOutcome, process::ProcessError>,
    failed: bool,
    invocation: &str,
) -> Value {
    let mut context = context.clone();
    let (queue, execution, bytes, tail) = match result {
        Ok(outcome) => (
            outcome.queue_ms,
            outcome.execution_ms,
            outcome.stderr_bytes,
            &outcome.stderr_tail,
        ),
        Err(error) => {
            context["hookFailureKind"] = json!(format!("{:?}", error.kind));
            (
                error.queue_ms,
                error.execution_ms,
                error.stderr_bytes,
                &error.stderr_tail,
            )
        }
    };
    context["hookTiming"] = json!({"queueMs":queue,"executionMs":execution,"stderrBytes":bytes,"stderrRetainedBytes":tail.len()});
    if failed {
        if let Ok(Some(path)) = super::payload::diagnostic_artifact(invocation, tail) {
            context["stderrArtifact"] = json!(path);
        }
    }
    context
}
fn validate_outcome_ack(
    outcome: &process::ProcessOutcome,
    max_payload_bytes: usize,
) -> Result<(), String> {
    match outcome
        .response_value(max_payload_bytes)
        .map_err(|e| e.to_string())?
    {
        None => Ok(()),
        Some(value) => validate_ack_value(&value),
    }
}
#[cfg(test)]
fn validate_ack(bytes: &[u8]) -> Result<(), String> {
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Ok(());
    }
    let value: Value = serde_json::from_slice(bytes)
        .map_err(|_| "observer stdout is not one JSON acknowledgment object")?;
    validate_ack_value(&value)
}
fn validate_ack_value(value: &Value) -> Result<(), String> {
    let object = value
        .as_object()
        .ok_or("observer acknowledgment must be an object")?;
    if object
        .keys()
        .any(|k| !matches!(k.as_str(), "version" | "ack"))
        || object.get("version").is_some_and(|v| v != 1)
        || object.get("ack").is_some_and(|v| v != true)
    {
        return Err("observer stdout may only acknowledge processing".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::config::Resolved;
    use super::*;
    use crate::config::Config;

    fn observer(directory: &str, extra: Value) -> Arc<Observer> {
        observer_with_settings(directory, extra, json!({}))
    }

    fn observer_with_settings(directory: &str, extra: Value, mut settings: Value) -> Arc<Observer> {
        let mut registration = json!({"id":"observer-test","on":"*","command":["true"]});
        for (key, value) in extra.as_object().unwrap() {
            registration[key] = value.clone();
        }
        settings["observers"] = json!([registration]);
        let cfg: Config = serde_json::from_value(json!({"ocmini":{"hooks":settings}})).unwrap();
        let resolved = Resolved::from_config(&cfg, directory).unwrap();
        Observer::register(
            resolved.observers[0].clone(),
            Arc::new(resolved.settings),
            directory,
            Arc::new(Semaphore::new(1)),
            Arc::new(Mutex::new(HashMap::new())),
        )
    }

    fn event(name: &str, properties: Value) -> Arc<BusEvent> {
        Arc::new(BusEvent {
            id: crate::types::hex_id("evt_"),
            r#type: name.into(),
            properties,
        })
    }

    #[test]
    fn count_and_byte_overflow_are_nonblocking_and_visible() {
        let directory = tempfile::tempdir().unwrap();
        let dir = directory.path().to_str().unwrap();
        let observer = observer(
            dir,
            json!({"on":"ocmini.turn.ended","queueCount":1,"queueBytes":1024}),
        );
        let ev = event("ocmini.turn.ended", json!({"directory":dir}));
        let mut bus = crate::engine::bus().subscribe();
        observer.enqueue(&ev, Some(100));
        observer.enqueue(&ev, Some(100));
        let q = observer.queue.lock().unwrap();
        assert_eq!(q.items.len(), 1);
        assert_eq!(q.bytes, 100);
        assert_eq!(q.dropped, 1);
        drop(q);
        let mut found = false;
        while let Ok(ev) = bus.try_recv() {
            if ev.r#type == "ocmini.hook.queue.overflow" && ev.properties["directory"] == dir {
                found = true;
            }
        }
        assert!(found);
        let bytes = self::observer(dir, json!({"queueBytes":10}));
        bytes.enqueue(&ev, Some(100));
        assert!(bytes.queue.lock().unwrap().items.is_empty());
    }

    #[test]
    fn oversized_events_use_an_explicit_preview_and_actual_byte_accounting() {
        let directory = tempfile::tempdir().unwrap();
        let dir = directory.path().to_str().unwrap();
        let observer = observer(dir, json!({}));
        let ev = event(
            "message.updated",
            json!({"directory":dir,"large":"x".repeat(1024*1024)}),
        );
        observer.enqueue(&ev, Some(1024 * 1024));
        let q = observer.queue.lock().unwrap();
        assert_eq!(q.items[0].event.properties["truncated"], true);
        assert!(q.bytes < 4096);
        assert_eq!(
            Some(q.bytes),
            encoded_size(q.items[0].event.as_ref(), usize::MAX)
        );
    }

    #[test]
    fn deltas_nonterminal_parts_and_diagnostics_do_not_match_wildcards() {
        let directory = tempfile::tempdir().unwrap();
        let dir = directory.path().to_str().unwrap();
        let observer = observer(dir, json!({}));
        let ctx = json!({"directory":dir});
        for ev in [
            event("message.part.delta", json!({"directory":dir})),
            event(
                "message.part.updated",
                json!({"directory":dir,"part":{"type":"text","time":{"start":1}}}),
            ),
            event(
                "ocmini.hook.completed",
                json!({"directory":dir,"handlerID":"foreign"}),
            ),
        ] {
            assert!(!observer.accepts(&ev, &ctx));
        }
        assert!(observer.accepts(
            &event(
                "message.part.updated",
                json!({"part":{"type":"text","time":{"start":1,"end":2}}})
            ),
            &ctx
        ));
        let explicit = self::observer(dir, json!({"on":"ocmini.hook.completed"}));
        assert!(explicit.accepts(
            &event("ocmini.hook.completed", json!({"handlerID":"foreign"})),
            &ctx
        ));
        assert!(!explicit.accepts(
            &event(
                "ocmini.hook.completed",
                json!({"handlerID":"observer-test"})
            ),
            &ctx
        ));
        assert!(!explicit.accepts(
            &event(
                "ocmini.hook.completed",
                json!({"handlerID":"foreign","observer":true})
            ),
            &ctx
        ));
    }

    #[test]
    fn observer_ack_cannot_mutate_or_block() {
        for bytes in [&b""[..], &b"{}"[..], &b"{\"ack\":true,\"version\":1}"[..]] {
            assert!(validate_ack(bytes).is_ok());
        }
        for bytes in [
            &b"{\"set\":{}}"[..],
            &b"{\"decision\":\"block\"}"[..],
            &b"{}{}"[..],
            &b"[]"[..],
            &b"{\"ack\":false}"[..],
        ] {
            assert!(validate_ack(bytes).is_err());
        }
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn observers_receive_ordered_batched_envelopes_and_bus_payloads_stay_shared() {
        let directory = tempfile::tempdir().unwrap();
        let dir = directory.path().to_str().unwrap();
        let cfg: Config = serde_json::from_value(json!({"ocmini":{"hooks":{"observers":[{"id":"batch","on":"ocmini.turn.ended","batchSize":8,"batchMs":40,"command":["sh","-c","cat > observed.json"]}]}}})).unwrap();
        let hooks =
            super::super::Hooks::build(Resolved::from_config(&cfg, dir).unwrap(), true).unwrap();
        let bus = crate::bus::Bus::new();
        let mut first = bus.subscribe();
        let mut second = bus.subscribe();
        for n in 0..3 {
            bus.emit("ocmini.turn.ended", json!({"directory":dir,"sequence":n}));
        }
        let a = first.recv().await.unwrap();
        let b = second.recv().await.unwrap();
        assert!(Arc::ptr_eq(&a, &b));
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if std::fs::read_to_string(directory.path().join("observed.json"))
                    .is_ok_and(|s| serde_json::from_str::<Value>(&s).is_ok())
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        let value: Value = serde_json::from_str(
            &std::fs::read_to_string(directory.path().join("observed.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(value["subscriptionID"], "batch");
        assert_eq!(value["events"].as_array().unwrap().len(), 3);
        assert_eq!(value["events"][0]["properties"]["sequence"], 0);
        assert_eq!(value["events"][2]["properties"]["sequence"], 2);
        drop(hooks);
    }

    #[test]
    fn drop_oldest_overflow_policy_evicts_until_under_limits() {
        let directory = tempfile::tempdir().unwrap();
        let dir = directory.path().to_str().unwrap();
        let observer = observer(
            dir,
            json!({
                "on": "ocmini.turn.ended",
                "overflow": "dropOldest",
                "queueCount": 2,
                "queueBytes": 1000
            }),
        );
        let ev1 = event("ocmini.turn.ended", json!({"directory": dir, "seq": 1}));
        let ev2 = event("ocmini.turn.ended", json!({"directory": dir, "seq": 2}));
        let ev3 = event("ocmini.turn.ended", json!({"directory": dir, "seq": 3}));

        observer.enqueue(&ev1, Some(100));
        observer.enqueue(&ev2, Some(100));
        {
            let q = observer.queue.lock().unwrap();
            assert_eq!(q.items.len(), 2);
            assert_eq!(q.items[0].event.properties["seq"], 1);
            assert_eq!(q.items[1].event.properties["seq"], 2);
            assert_eq!(q.dropped, 0);
        }

        observer.enqueue(&ev3, Some(100));
        {
            let q = observer.queue.lock().unwrap();
            assert_eq!(q.items.len(), 2);
            assert_eq!(q.items[0].event.properties["seq"], 2);
            assert_eq!(q.items[1].event.properties["seq"], 3);
            assert_eq!(q.dropped, 1);
            assert_eq!(q.gaps, 1);
        }

        let byte_obs = self::observer(
            dir,
            json!({
                "on": "ocmini.turn.ended",
                "overflow": "dropOldest",
                "queueCount": 10,
                "queueBytes": 150
            }),
        );
        byte_obs.enqueue(&ev1, Some(80));
        byte_obs.enqueue(&ev2, Some(60));
        byte_obs.enqueue(&ev3, Some(80));
        {
            let q = byte_obs.queue.lock().unwrap();
            assert_eq!(q.items.len(), 2);
            assert_eq!(q.items[0].event.properties["seq"], 2);
            assert_eq!(q.items[1].event.properties["seq"], 3);
            assert_eq!(q.bytes, 140);
        }
    }

    #[test]
    fn coalesce_overflow_policy_replaces_only_replaceable_state() {
        let directory = tempfile::tempdir().unwrap();
        let dir = directory.path().to_str().unwrap();
        let obs = observer(
            dir,
            json!({
                "on": "*",
                "overflow": "coalesce",
                "queueCount": 5,
                "queueBytes": 2000
            }),
        );

        let s1 = event(
            "session.status",
            json!({"directory": dir, "sessionID": "s_1", "status": "busy"}),
        );
        let s2 = event(
            "session.status",
            json!({"directory": dir, "sessionID": "s_1", "status": "idle"}),
        );
        obs.enqueue(&s1, Some(50));
        obs.enqueue(&s2, Some(50));
        {
            let q = obs.queue.lock().unwrap();
            assert_eq!(q.items.len(), 1);
            assert_eq!(q.items[0].event.properties["status"], "idle");
            assert_eq!(q.gaps, 1);
        }

        let p1 = event(
            "message.part.updated",
            json!({
                "directory": dir,
                "part": {"sessionID": "s_1", "messageID": "m_1", "id": "p_1", "type": "text", "time": {"start": 10}}
            }),
        );
        let p2 = event(
            "message.part.updated",
            json!({
                "directory": dir,
                "part": {"sessionID": "s_1", "messageID": "m_1", "id": "p_1", "type": "text", "time": {"start": 10}, "text": "hello"}
            }),
        );
        obs.enqueue(&p1, Some(50));
        obs.enqueue(&p2, Some(60));
        {
            let q = obs.queue.lock().unwrap();
            assert_eq!(q.items.len(), 2);
            assert_eq!(q.items[1].event.properties["part"]["text"], "hello");
        }

        let p_term = event(
            "message.part.updated",
            json!({
                "directory": dir,
                "part": {"sessionID": "s_1", "messageID": "m_1", "id": "p_1", "type": "text", "time": {"start": 10, "end": 20}, "text": "final"}
            }),
        );
        obs.enqueue(&p_term, Some(70));
        {
            let q = obs.queue.lock().unwrap();
            assert_eq!(q.items.len(), 3);
        }

        let t1 = event(
            "ocmini.turn.ended",
            json!({"directory": dir, "turnID": "t1"}),
        );
        let t2 = event(
            "ocmini.turn.ended",
            json!({"directory": dir, "turnID": "t2"}),
        );
        obs.enqueue(&t1, Some(40));
        obs.enqueue(&t2, Some(40));
        {
            let q = obs.queue.lock().unwrap();
            assert_eq!(q.items.len(), 5);
        }
    }

    #[test]
    fn observe_large_file_mode_payload_preserves_exact_event_and_bytes() {
        let directory = tempfile::tempdir().unwrap();
        let dir = directory.path().to_str().unwrap();
        let obs = observer(
            dir,
            json!({
                "on": "ocmini.turn.ended",
                "payloadMode": "file",
                "queueCount": 10,
                "queueBytes": 10 * 1024 * 1024
            }),
        );
        let inline = observer(dir, json!({"id": "inline", "on": "ocmini.turn.ended"}));

        let large_text = "x".repeat(300 * 1024);
        let ev = event(
            "ocmini.turn.ended",
            json!({"directory": dir, "payload": large_text}),
        );
        let real_size = serde_json::to_vec(ev.as_ref()).unwrap().len();
        assert!(real_size > 128 * 1024);

        observe(&ev);
        let q = obs.queue.lock().unwrap();
        assert_eq!(q.items.len(), 1);
        assert_eq!(q.items[0].event.properties.get("truncated"), None);
        assert!(Arc::ptr_eq(&q.items[0].event, &ev));
        assert_eq!(q.bytes, real_size);
        assert_eq!(q.items[0].bytes, real_size);

        let q = inline.queue.lock().unwrap();
        assert_eq!(q.items.len(), 1);
        assert_eq!(q.items[0].event.properties["truncated"], true);
        let preview_size = serde_json::to_vec(q.items[0].event.as_ref()).unwrap().len();
        assert_eq!(q.bytes, preview_size);
        assert_eq!(q.items[0].bytes, preview_size);
        assert!(preview_size < inline.settings.observer_event_bytes);
    }

    #[test]
    fn observe_large_file_mode_events_respect_queue_bytes() {
        for overflow in ["dropNewest", "dropOldest", "coalesce"] {
            let directory = tempfile::tempdir().unwrap();
            let dir = directory.path().to_str().unwrap();
            let obs = observer(
                dir,
                json!({
                    "on": "ocmini.turn.ended",
                    "payloadMode": "file",
                    "overflow": overflow,
                    "queueBytes": 512 * 1024
                }),
            );
            let first = event(
                "ocmini.turn.ended",
                json!({"directory": dir, "payload": "x".repeat(300 * 1024)}),
            );
            let second = event(
                "ocmini.turn.ended",
                json!({"directory": dir, "payload": "x".repeat(320 * 1024)}),
            );
            observe(&first);
            observe(&second);

            let retained = if overflow == "dropOldest" {
                &second
            } else {
                &first
            };
            let retained_size = serde_json::to_vec(retained.as_ref()).unwrap().len();
            {
                let q = obs.queue.lock().unwrap();
                assert_eq!(q.items.len(), 1, "{overflow}");
                assert!(Arc::ptr_eq(&q.items[0].event, retained), "{overflow}");
                assert_eq!(q.items[0].bytes, retained_size, "{overflow}");
                assert_eq!(q.bytes, retained_size, "{overflow}");
                assert_eq!(q.dropped, 1, "{overflow}");
                assert_eq!(q.gaps, 1, "{overflow}");
            }

            let oversized = event(
                "ocmini.turn.ended",
                json!({"directory": dir, "payload": "x".repeat(600 * 1024)}),
            );
            observe(&oversized);
            let q = obs.queue.lock().unwrap();
            assert_eq!(q.items.len(), 1, "{overflow}");
            assert!(Arc::ptr_eq(&q.items[0].event, retained), "{overflow}");
            assert_eq!(q.bytes, retained_size, "{overflow}");
            assert_eq!(q.dropped, 2, "{overflow}");
            assert_eq!(q.gaps, 2, "{overflow}");
        }
    }

    #[test]
    fn observe_large_file_mode_events_respect_max_payload_bytes() {
        for inline_limit in [128 * 1024, 1024 * 1024] {
            let directory = tempfile::tempdir().unwrap();
            let dir = directory.path().to_str().unwrap();
            let obs = observer_with_settings(
                dir,
                json!({"on": "ocmini.turn.ended", "payloadMode": "file", "queueBytes": 1024 * 1024}),
                json!({"maxPayloadBytes": 256 * 1024}),
            );
            let inline = observer_with_settings(
                dir,
                json!({"id": "inline", "on": "ocmini.turn.ended"}),
                json!({"observerEventBytes": inline_limit, "maxInputBytes": 2 * 1024 * 1024}),
            );
            let accepted = event(
                "ocmini.turn.ended",
                json!({"directory": dir, "payload": "x".repeat(200 * 1024)}),
            );
            let oversized = event(
                "ocmini.turn.ended",
                json!({"directory": dir, "payload": "x".repeat(300 * 1024)}),
            );
            observe(&accepted);
            observe(&oversized);

            let accepted_size = serde_json::to_vec(accepted.as_ref()).unwrap().len();
            let q = obs.queue.lock().unwrap();
            assert_eq!(q.items.len(), 1);
            assert!(Arc::ptr_eq(&q.items[0].event, &accepted));
            assert_eq!(q.items[0].bytes, accepted_size);
            assert_eq!(q.bytes, accepted_size);
            assert_eq!(q.dropped, 1);
            assert_eq!(q.gaps, 1);

            let q = inline.queue.lock().unwrap();
            assert_eq!(q.items.len(), 2);
            let expected_bytes: usize = q
                .items
                .iter()
                .map(|item| {
                    let size = serde_json::to_vec(item.event.as_ref()).unwrap().len();
                    assert_eq!(item.bytes, size);
                    size
                })
                .sum();
            assert_eq!(q.bytes, expected_bytes);
            if inline_limit == 128 * 1024 {
                assert_eq!(q.items[1].event.properties["truncated"], true);
            } else {
                assert!(Arc::ptr_eq(&q.items[1].event, &oversized));
            }
        }
    }

    #[test]
    fn durable_store_reopen_retry_exhaustion_quota_dedup_and_lease() {
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir.path().join("store.db");
        let store = Arc::new(crate::store::Store::open(&db_path, "inst_1").unwrap());

        let dir = "/test/dir";
        let reg_json = json!({
            "id": "audit-durable",
            "on": "ocmini.turn.ended",
            "delivery": "durable",
            "retryAttempts": 3,
            "retryDelayMs": 100,
            "retentionMs": 86400000,
            "queueCount": 2,
            "queueBytes": 10000,
            "command": ["true"]
        });
        let cfg: Config =
            serde_json::from_value(json!({"ocmini":{"hooks":{"observers":[reg_json]}}})).unwrap();
        let resolved = Resolved::from_config(&cfg, dir).unwrap();
        let obs = Observer::register(
            resolved.observers[0].clone(),
            Arc::new(resolved.settings),
            dir,
            Arc::new(Semaphore::new(1)),
            Arc::new(Mutex::new(HashMap::new())),
        );
        let sub_key = &obs.subscription_key;

        let ev1 = event("ocmini.turn.ended", json!({"directory": dir, "mid": "m1"}));
        let ev2 = event("ocmini.turn.ended", json!({"directory": dir, "mid": "m2"}));
        let ev3 = event("ocmini.turn.ended", json!({"directory": dir, "mid": "m3"}));

        // 1. Dedup & Persist in Transaction
        {
            let mut conn = rusqlite::Connection::open(&db_path).unwrap();
            let tx = conn.transaction().unwrap();
            assert!(persist_committed(&tx, &ev1).is_ok());
            assert!(persist_committed(&tx, &ev1).is_ok());
            assert!(persist_committed(&tx, &ev2).is_ok());
            let rejected = persist_committed(&tx, &ev3).unwrap();
            assert!(rejected.join("; ").contains("outbox quota exceeded"));
            tx.commit().unwrap();
        }

        // 2. Debug inspection (bounded metadata without secrets)
        let list = store.hook_deliveries(dir).unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0]["state"], "pending");
        assert!(list[0].get("event_json").is_none());

        // 3. Claim ONE with unique lease token (counts attempt)
        let claimed1 = store
            .claim_hook_delivery(sub_key, 30_000, 3)
            .unwrap()
            .unwrap();
        assert_eq!(claimed1.event_id, ev1.id);
        assert_eq!(claimed1.state, "running");
        assert_eq!(claimed1.attempts, 1);
        let token1 = claimed1.lease_owner.as_ref().unwrap();

        // Sibling instance claims next available (ev2)
        let sibling_store = Arc::new(crate::store::Store::open(&db_path, "inst_2").unwrap());
        let claimed2 = sibling_store
            .claim_hook_delivery(sub_key, 30_000, 3)
            .unwrap()
            .unwrap();
        assert_eq!(claimed2.event_id, ev2.id);
        assert_eq!(claimed2.attempts, 1);
        let token2 = claimed2.lease_owner.as_ref().unwrap();

        // Fencing check: wrong token cannot complete or fail
        assert!(!store
            .complete_hook_delivery(sub_key, &ev1.id, "wrong_token")
            .unwrap());

        // 4. Complete ev1 with correct token removes row
        assert!(store
            .complete_hook_delivery(sub_key, &ev1.id, token1)
            .unwrap());
        let list_after_comp = store.hook_deliveries(dir).unwrap();
        assert_eq!(list_after_comp.len(), 1);

        // 5. Fail ev2 -> Retry backoff
        assert!(sibling_store
            .fail_hook_delivery(sub_key, &ev2.id, token2, 3, 100)
            .unwrap());
        let deliveries = sibling_store.hook_deliveries(dir).unwrap();
        assert_eq!(deliveries[0]["attempts"], 1);
        assert_eq!(deliveries[0]["state"], "pending");

        // Fail to exhaustion: claim 2nd attempt
        {
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            conn.execute(
                "UPDATE hook_delivery SET next_attempt=0 WHERE subscription_key=?1 AND event_id=?2",
                rusqlite::params![sub_key, ev2.id],
            )
            .unwrap();
        }
        let claimed_att2 = store
            .claim_hook_delivery(sub_key, 30_000, 3)
            .unwrap()
            .unwrap();
        assert_eq!(claimed_att2.attempts, 2);
        let token_att2 = claimed_att2.lease_owner.as_ref().unwrap();
        store
            .fail_hook_delivery(sub_key, &ev2.id, token_att2, 3, 100)
            .unwrap();

        // The final configured attempt executes; only its failure exhausts.
        {
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            conn.execute(
                "UPDATE hook_delivery SET next_attempt=0 WHERE subscription_key=?1 AND event_id=?2",
                rusqlite::params![sub_key, ev2.id],
            )
            .unwrap();
        }
        let claim_att3 = store
            .claim_hook_delivery(sub_key, 30_000, 3)
            .unwrap()
            .unwrap();
        assert_eq!(claim_att3.attempts, 3);
        store
            .fail_hook_delivery(
                sub_key,
                &ev2.id,
                claim_att3.lease_owner.as_deref().unwrap(),
                3,
                100,
            )
            .unwrap();

        let exhausted = store.hook_deliveries(dir).unwrap();
        assert_eq!(exhausted[0]["state"], "exhausted");
        assert_eq!(exhausted[0]["attempts"], 3);

        // 6. Reopen store verifies persistence
        drop(store);
        drop(sibling_store);
        let reopened = crate::store::Store::open(&db_path, "inst_reopened").unwrap();
        let persisted = reopened.hook_deliveries(dir).unwrap();
        assert_eq!(persisted.len(), 1);
        assert_eq!(persisted[0]["state"], "exhausted");
    }

    #[test]
    fn durable_no_replay_under_changed_registration_or_disabled() {
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir.path().join("store.db");
        let store = Arc::new(crate::store::Store::open(&db_path, "inst_1").unwrap());

        let dir = "/test/dir";
        let reg_json1 = json!({
            "id": "audit-hook",
            "on": "command.executed",
            "delivery": "durable",
            "command": ["echo", "v1"]
        });
        let cfg1: Config =
            serde_json::from_value(json!({"ocmini":{"hooks":{"observers":[reg_json1]}}})).unwrap();
        let resolved1 = Resolved::from_config(&cfg1, dir).unwrap();
        let obs1 = Observer::register(
            resolved1.observers[0].clone(),
            Arc::new(resolved1.settings),
            dir,
            Arc::new(Semaphore::new(1)),
            Arc::new(Mutex::new(HashMap::new())),
        );
        let sub_key1 = &obs1.subscription_key;

        let ev = event("command.executed", json!({"directory": dir, "cmd": "ls"}));
        {
            let mut conn = rusqlite::Connection::open(&db_path).unwrap();
            let tx = conn.transaction().unwrap();
            persist_committed(&tx, &ev).unwrap();
            tx.commit().unwrap();
        }

        // Updated registration (different command -> different fingerprint & sub_key)
        let reg_json2 = json!({
            "id": "audit-hook",
            "on": "command.executed",
            "delivery": "durable",
            "command": ["echo", "v2"]
        });
        let cfg2: Config =
            serde_json::from_value(json!({"ocmini":{"hooks":{"observers":[reg_json2]}}})).unwrap();
        let resolved2 = Resolved::from_config(&cfg2, dir).unwrap();
        let obs2 = Observer::register(
            resolved2.observers[0].clone(),
            Arc::new(resolved2.settings),
            dir,
            Arc::new(Semaphore::new(1)),
            Arc::new(Mutex::new(HashMap::new())),
        );
        let sub_key2 = &obs2.subscription_key;

        assert_ne!(sub_key1, sub_key2);
        let claimed_under_v2 = store.claim_hook_delivery(sub_key2, 30_000, 3).unwrap();
        assert!(claimed_under_v2.is_none());
    }
}
