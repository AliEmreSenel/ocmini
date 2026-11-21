//! Durable history and its coordinated session operations.
//!
//! The backend type is selected at compile time. One backend persists messages,
//! parts, paging, admissions, ownership, forks, recovery and hook delivery.
//! Operations that span these records must commit
//! atomically; a backend must not send history writes to a separate store.
//! Calls remain synchronous and use the existing blocking adapters. SQLite is
//! the default implementation and keeps its WAL, locking and migration rules.
use super::shared::{self, PreparedFork};
use crate::types::now_ms;
use crate::types::{
    MessageInfo, MessageWithParts, ModelRef, OperationError, PromptRequest, Session, TurnResult,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[derive(Clone)]
pub struct SessionScope {
    pub directory: String,
    pub parent_id: Option<String>,
}

pub enum AdmissionReservation {
    Acquired(String),
    Waiting,
    Existing,
    Rejected(String),
}

pub struct InterruptedTurn {
    pub message_id: String,
    pub session_id: String,
    pub state: String,
    pub reason: Option<String>,
    pub started: bool,
    pub session: String,
    pub message: Option<String>,
    pub prepared: Option<String>,
    pub snapshot: Option<String>,
    pub request: Option<String>,
}

pub struct RecoveredTurn {
    pub source: InterruptedTurn,
    pub events: Vec<std::sync::Arc<crate::types::BusEvent>>,
    pub deliveries: Vec<OutboxIntent>,
}

#[derive(Clone)]
pub struct HookDeliveryBudget {
    pub retention_ms: u64,
    pub count: usize,
    pub bytes: usize,
    pub total_count: usize,
    pub total_bytes: usize,
}

/// Notification routing is resolved before the domain transaction. Store only
/// inserts these opaque delivery intents under the supplied budgets.
pub struct OutboxIntent {
    pub subscription_key: String,
    pub event_id: String,
    pub directory: String,
    pub event_json: String,
    pub budget: HookDeliveryBudget,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookDeliveryRecord {
    pub subscription_key: String,
    pub event_id: String,
    pub directory: String,
    pub event_json: String,
    pub event_bytes: usize,
    pub attempts: u32,
    pub next_attempt: i64,
    pub expires: i64,
    pub state: String,
    pub lease_owner: Option<String>,
    pub lease_until: Option<i64>,
}

// Declare the contract once; the SQLite adapter only forwards to its existing
// operations, preserving their transaction boundaries and read paths.
macro_rules! backend {
    (shared { $(fn $shared:ident(&$receiver:ident $(, $sarg:ident: $sty:ty)* $(,)?) -> $sret:ty $body:block)* }
     storage { $(fn $name:ident(&self $(, $arg:ident: $ty:ty)* $(,)?) -> $ret:ty;)* }) => {
        /// Statically selected storage contract. Shared lifecycle policies live
        /// in core; backend operations retain their atomic commit boundaries.
        pub trait HistoryBackend: Send + Sync + Sized {
            $(fn $shared(&$receiver $(, $sarg: $sty)*) -> $sret $body)*
            $(fn $name(&self $(, $arg: $ty)*) -> $ret;)*
        }
        impl<B: HistoryBackend> super::Store<B> {
            $(pub fn $shared(&$receiver $(, $sarg: $sty)*) -> $sret {
                $receiver.backend.$shared($($sarg),*)
            })*
            $(pub fn $name(&self $(, $arg: $ty)*) -> $ret {
                self.backend.$name($($arg),*)
            })*
        }
        impl HistoryBackend for super::sqlite::SqliteBackend {
            $(fn $name(&self $(, $arg: $ty)*) -> $ret {
                Self::$name(self $(, $arg)*)
            })*
        }
        #[cfg(test)]
        pub(super) struct RecordingBackend {
            pub inner: super::sqlite::SqliteBackend,
            pub calls: std::sync::Arc<std::sync::Mutex<Vec<&'static str>>>,
        }
        #[cfg(test)]
        impl HistoryBackend for RecordingBackend {
            $(fn $name(&self $(, $arg: $ty)*) -> $ret {
                self.calls.lock().unwrap().push(stringify!($name));
                self.inner.$name($($arg),*)
            })*
        }
    }
}
backend! {
shared {
    fn directory_of(&self, sid: &str) -> Option<String> {
        self.scope(sid).map(|scope| scope.directory)
    }
    fn messages(&self, sid: &str) -> Result<Vec<MessageWithParts>, OperationError> {
        self.messages_limited(sid, None)
    }
    fn update_session(&self, session: &Session) -> Result<(), OperationError> {
        self.mutate_session(&session.id, |current| {
            *current = session.clone();
            Ok(true)
        })?.ok_or_else(|| OperationError::not_found("Session not found")).map(|_| ())
    }
    fn try_fork(&self, source: &Session, up_to: Option<&str>) -> Result<Session, OperationError> {
        self.fork_history(source, up_to, |history| shared::prepare_fork(source, history))
    }
    fn recover_interrupted(&self, plans: &[RecoveredTurn]) -> Result<(Vec<std::sync::Arc<crate::types::BusEvent>>, Vec<String>), OperationError> {
        self.apply_recovery(plans, shared::interrupted_message, shared::interrupted_part)
    }
    fn turn_result(&self, mid: &str) -> Result<Option<(TurnResult, bool)>, OperationError> {
        let Some((sid, result, finalizing)) = self.stored_turn_result(mid)? else { return Ok(None); };
        let value = if let Some(id) = result["responseID"].as_str() {
            Ok(self.message(&sid, id)?.ok_or("Recorded response not found")?)
        } else {
            Err(result["error"].as_str().unwrap_or("Recorded turn failed").into())
        };
        Ok(Some((value, finalizing)))
    }
    fn settle_hook_turn(&self, mid: &str, sid: &str, value: &TurnResult, reason: &str, started: bool, events: &[std::sync::Arc<crate::types::BusEvent>]) -> Result<(), OperationError> {
        let result = match value {
            Ok(message) => json!({"responseID":message.info.id}),
            Err(error) => json!({"error":error.message}),
        };
        self.commit_hook_result(mid, sid, &result, reason, started, events)
    }
    fn v2_transition(&self, sid: &str, patch: &Value, value: Value) -> Result<Value, OperationError> {
        let (value, message) = shared::transition_message(sid, value);
        self.commit_transition(sid, patch, &message)?;
        Ok(value)
    }
    fn prune_tool_outputs(&self, sid: &str, through: &str) -> Result<Vec<Value>, OperationError> {
        crate::blocking::run(|| {
            let messages = self.context_messages(sid, through)?;
            let mut turns = 0;
            let mut total = 0usize;
            let mut pruned = 0usize;
            let mut parts = Vec::new();
            'history: for message in messages.into_iter().rev() {
                if message.info.role == "user" {
                    turns += 1;
                }
                if turns < 2 {
                    continue;
                }
                if message.info.summary == Some(true) {
                    break;
                }
                for mut part in message.parts.into_iter().rev() {
                    if part["type"] != "tool"
                        || part["state"]["status"] != "completed"
                        || part["tool"] == "skill"
                    {
                        continue;
                    }
                    if part["state"]["time"]["compacted"].is_number() {
                        break 'history;
                    }
                    let tokens = part["state"]["output"]
                        .as_str()
                        .unwrap_or_default()
                        .len()
                        .div_ceil(4);
                    total = total.saturating_add(tokens);
                    if total <= 40_000 {
                        continue;
                    }
                    pruned = pruned.saturating_add(tokens);
                    part["state"]["time"]["compacted"] = json!(now_ms());
                    parts.push(part);
                }
            }
            if pruned <= 20_000 {
                return Ok(Vec::new());
            }

            self.mark_compacted(sid, &parts)?;
            Ok(parts)
        })
    }
}
storage {
    fn stored_turn_result(&self, mid: &str) -> Result<Option<(String, Value, bool)>, OperationError>;
    fn commit_hook_result(&self, mid: &str, sid: &str, result: &Value, reason: &str, started: bool, events: &[std::sync::Arc<crate::types::BusEvent>]) -> Result<(), OperationError>;
    fn commit_transition(&self, sid: &str, patch: &Value, message: &MessageWithParts) -> Result<(), OperationError>;

    fn fork_history(&self, source: &Session, up_to: Option<&str>, prepare: impl FnOnce(Vec<MessageWithParts>) -> PreparedFork) -> Result<Session, OperationError>;
    fn apply_recovery(&self, plans: &[RecoveredTurn], message: impl FnMut(&str, i64) -> Result<MessageInfo, OperationError>, part: impl FnMut(&str, i64) -> Result<Value, OperationError>) -> Result<(Vec<std::sync::Arc<crate::types::BusEvent>>, Vec<String>), OperationError>;
    fn mark_compacted(&self, sid: &str, parts: &[Value]) -> Result<(), OperationError>;
    fn claim(&self, sid: &str) -> Result<(), OperationError>;
    fn heartbeat(&self, sid: &str) -> Result<bool, OperationError>;
    fn check_ownership(&self, sid: &str) -> Result<(), OperationError>;
    fn release(&self, sid: &str) -> ();

    fn scope(&self, sid: &str) -> Option<SessionScope>;

    fn prune(&self, before: i64) -> Result<usize, OperationError>;
    fn create_session(&self, s: &Session) -> Result<(), OperationError>;
    fn list_sessions(&self, dir: Option<&str>) -> Result<Vec<Session>, OperationError>;
    fn list_sessions_filtered(
        &self,
        directory: Option<&str>,
        roots: bool,
        limit: Option<usize>,
        matches: impl FnMut(&Session) -> bool,
    ) -> Result<Vec<Session>, OperationError>;
    fn get_session(&self, id: &str) -> Result<Option<Session>, OperationError>;
    fn mutate_session(
        &self,
        id: &str,
        change: impl FnOnce(&mut Session) -> Result<bool, OperationError>,
    ) -> Result<Option<Session>, OperationError>;

    fn delete_session_checked(
        &self,
        id: &str,
        expected_ids: &[String],
    ) -> Result<bool, OperationError>;
    fn children(&self, id: &str) -> Result<Vec<Session>, OperationError>;
    fn is_busy(&self, id: &str) -> Result<bool, OperationError>;
    fn busy_sessions(&self) -> Result<Vec<String>, OperationError>;
    fn add_message(&self, m: &MessageWithParts) -> Result<(), OperationError>;
    fn update_part(&self, session_id: &str, part: &Value) -> Result<(), OperationError>;


    fn messages_limited(
        &self,
        session_id: &str,
        limit: Option<usize>,
    ) -> Result<Vec<MessageWithParts>, OperationError>;
    fn message(
        &self,
        session_id: &str,
        id: &str,
    ) -> Result<Option<MessageWithParts>, OperationError>;
    fn message_part(
        &self,
        sid: &str,
        mid: &str,
        id: &str,
    ) -> Result<Option<Value>, OperationError>;
    fn last_model(&self, sid: &str) -> Result<Option<ModelRef>, OperationError>;
    fn reply_to(
        &self,
        sid: &str,
        parent: &str,
    ) -> Result<Option<MessageWithParts>, OperationError>;
    fn context_messages(
        &self,
        sid: &str,
        through: &str,
    ) -> Result<Vec<MessageWithParts>, OperationError>;
    fn message_owner(&self, id: &str) -> Result<Option<String>, OperationError>;
    fn admit(
        &self,
        request: &PromptRequest,
        message: &MessageWithParts,
    ) -> Result<bool, OperationError>;
    fn next_prompt(&self, session_id: &str) -> Result<Option<PromptRequest>, OperationError>;
    fn finish_prompt(&self, id: &str) -> Result<(), OperationError>;
    fn cancel_pending(&self, sid: &str) -> Result<(), OperationError>;
    fn pending_sessions(&self) -> Result<Vec<String>, OperationError>;
    fn has_waiting(&self, sid: &str) -> Result<bool, OperationError>;
    fn is_pending_message(&self, id: &str) -> Result<bool, OperationError>;
    fn todos(&self, sid: &str) -> Result<Value, OperationError>;
    fn set_todos(&self, sid: &str, todos: &Value) -> Result<(), OperationError>;

    fn interrupted_turns(&self) -> Result<Vec<InterruptedTurn>, OperationError>;
    fn reserve_hook_admission(
        &self,
        mid: &str,
        sid: &str,
        deadline: i64,
    ) -> Result<AdmissionReservation, OperationError>;
    fn hook_admission_ready(&self, mid: &str, token: &str) -> Result<bool, OperationError>;
    fn reject_hook_admission(
        &self,
        mid: &str,
        token: &str,
        error: &str,
    ) -> Result<(), OperationError>;
    fn cancel_hook_admissions(&self, sid: &str) -> Result<(), OperationError>;
    fn turn_snapshot(&self, mid: &str) -> Result<Option<Value>, OperationError>;
    fn hook_turn_finalizing(&self, mid: &str) -> Result<bool, OperationError>;


    fn finalizer_started(&self, mid: &str) -> Result<(), OperationError>;
    fn finish_hook_turn(
        &self,
        mid: &str,
        diagnostic: Option<&str>,
        deliveries: &[OutboxIntent],
    ) -> Result<Vec<String>, OperationError>;
    fn queued_hook_prompts(&self, sid: &str) -> Result<Vec<PromptRequest>, OperationError>;
    fn append_hook_feedback(
        &self,
        mid: &str,
        assistant: &str,
        text: &str,
    ) -> Result<u32, OperationError>;
    fn claim_hook_delivery(
        &self,
        subscription_key: &str,
        lease_ms: i64,
        max_attempts: u32,
    ) -> Result<Option<HookDeliveryRecord>, OperationError>;
    fn complete_hook_delivery(
        &self,
        subscription_key: &str,
        event_id: &str,
        lease_token: &str,
    ) -> Result<bool, OperationError>;
    fn fail_hook_delivery(
        &self,
        subscription_key: &str,
        event_id: &str,
        lease_token: &str,
        retry_attempts: u32,
        retry_delay_ms: u64,
    ) -> Result<bool, OperationError>;
    fn release_hook_delivery(
        &self,
        subscription_key: &str,
        event_id: &str,
        lease_token: &str,
    ) -> Result<bool, OperationError>;
    fn next_hook_delivery_at(
        &self,
        subscription_key: &str,
    ) -> Result<Option<i64>, OperationError>;
    fn hook_deliveries(&self, directory: &str) -> Result<Vec<Value>, OperationError>;
    fn v2_state(&self, sid: &str) -> Result<Value, OperationError>;
    fn v2_enabled(&self, sid: &str) -> Result<bool, OperationError>;
    fn v2_update(&self, sid: &str, patch: &Value) -> Result<Value, OperationError>;
    fn v2_sequence(
        &self,
        sid: &str,
        next: i64,
        end: i64,
        reserve: i64,
    ) -> Result<(i64, i64), OperationError>;
    fn v2_inbox(&self, sid: &str) -> Result<Vec<Value>, OperationError>;
    fn v2_cancel_input(&self, sid: &str, id: &str) -> Result<(), OperationError>;
    fn v2_update_inbox_delivery(
        &self,
        sid: &str,
        id: &str,
        delivery: &str,
    ) -> Result<bool, OperationError>;
    fn has_steering(&self, sid: &str) -> Result<bool, OperationError>;

    fn v2_revert(&self, sid: &str, boundary: Option<&str>) -> Result<(), OperationError>;
    fn v2_move(&self, sid: &str, directory: &str) -> Result<(), OperationError>;
    fn v2_instructions(&self, sid: &str) -> Result<String, OperationError>;
    fn v2_page(
        &self,
        sid: &str,
        after: Option<i64>,
        descending: bool,
        limit: usize,
    ) -> Result<Vec<(i64, MessageWithParts)>, OperationError>;
    fn v2_instruction(
        &self,
        sid: &str,
        key: &str,
        value: Option<&Value>,
    ) -> Result<(), OperationError>;
    fn v2_sessions_page(
        &self,
        filter: &crate::v2::list::Filter,
        cursor: Option<&crate::v2::list::Cursor>,
        limit: usize,
    ) -> Result<Vec<(Session, Value)>, OperationError>;
}
}
