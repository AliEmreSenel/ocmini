//! Producers create typed facts. Delivery receives an immutable wire envelope.
use crate::types::{
    MessageInfo, ModelRef, PermissionAction, PermissionRequest, QuestionRequest,
    Session as SessionInfo, Tokens,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

macro_rules! event_tree {
    ($($group:ident($kind:ident $(<$lifetime:lifetime>)?) {
        $($variant:ident => $wire:literal { $($(#[$attr:meta])* $field:ident: $ty:ty),* $(,)? }),+ $(,)?
    }),+ $(,)?) => {
        #[derive(Serialize)]
        #[serde(untagged)]
        pub enum Event<'a> { $($group($group $(<$lifetime>)?)),+ }
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
        #[serde(untagged)]
        pub enum EventKind { $($group($kind)),+ }
        pub const NAMES: &[&str] = &[$($($wire),+),+];
        impl EventKind {
            pub fn as_str(self) -> &'static str { match self { $(Self::$group(kind) => kind.as_str()),+ } }
        }
        impl Event<'_> {
            pub fn kind(&self) -> EventKind { match self { $(Self::$group(event) => EventKind::$group(event.kind())),+ } }
        }
        $(
            #[derive(Serialize)]
            #[serde(tag = "type", content = "properties")]
            pub enum $group $(<$lifetime>)? {
                $(#[serde(rename = $wire)] $variant { $($(#[$attr])* $field: $ty),* }),+
            }
            #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
            pub enum $kind { $(#[serde(rename = $wire)] $variant),+ }
            impl $kind {
                pub fn as_str(self) -> &'static str { match self { $(Self::$variant => $wire),+ } }
            }
            impl $(<$lifetime>)? $group $(<$lifetime>)? {
                pub fn kind(&self) -> $kind { match self { $(Self::$variant { .. } => $kind::$variant),+ } }
            }
            impl<'a> From<$group $(<$lifetime>)?> for Event<'a> {
                fn from(event: $group $(<$lifetime>)?) -> Self { Self::$group(event) }
            }
        )+
    }
}

event_tree! {
    Server(ServerKind) {
        Connected => "server.connected" {},
        Heartbeat => "server.heartbeat" {}
    },
    Session(SessionKind<'a>) {
        Created => "session.created" { info: &'a SessionInfo },
        Updated => "session.updated" { info: &'a SessionInfo },
        Deleted => "session.deleted" { info: &'a SessionInfo },
        Status => "session.status" { #[serde(rename="sessionID")] session_id: &'a str, status: &'a SessionStatus },
        Idle => "session.idle" { #[serde(rename="sessionID")] session_id: &'a str },
        Error => "session.error" { #[serde(rename="sessionID")] session_id: &'a str, error: &'a Value },
        Compacted => "session.compacted" { #[serde(rename="sessionID")] session_id: &'a str },
        Title => "session.title" { #[serde(rename="sessionID")] session_id: &'a str, #[serde(flatten)] state: Title<'a> },
        Forked => "ocmini.session.forked" { #[serde(rename="sourceID")] source_id: &'a str, #[serde(rename="sessionID")] session_id: &'a str, #[serde(rename="messageID")] message_id: Option<&'a str> }
    },
    Message(MessageKind<'a>) {
        Updated => "message.updated" { #[serde(rename="sessionID")] session_id: &'a str, info: &'a MessageInfo }
    },
    Part(PartKind<'a>) {
        Updated => "message.part.updated" { #[serde(rename="sessionID")] session_id: &'a str, part: &'a Value },
        Delta => "message.part.delta" { #[serde(rename="sessionID")] session_id: &'a str, #[serde(rename="messageID")] message_id: &'a str, #[serde(rename="partID")] part_id: &'a str, field: DeltaField, delta: &'a str }
    },
    Todo(TodoKind<'a>) {
        Updated => "todo.updated" { #[serde(rename="sessionID")] session_id: &'a str, todos: &'a Value }
    },
    Permission(PermissionKind<'a>) {
        Asked => "permission.asked" { #[serde(flatten)] request: &'a PermissionRequest },
        Replied => "permission.replied" { #[serde(rename="sessionID")] session_id: &'a str, #[serde(rename="requestID")] request_id: &'a str, reply: PermissionReply },
        Decided => "ocmini.permission.decided" { permission: &'a str, pattern: &'a str, decision: PermissionAction }
    },
    Question(QuestionKind<'a>) {
        Asked => "question.asked" { #[serde(flatten)] request: &'a QuestionRequest },
        Replied => "question.replied" { #[serde(rename="sessionID")] session_id: &'a str, #[serde(rename="requestID")] request_id: &'a str, answers: &'a [Vec<String>] },
        Rejected => "question.rejected" { #[serde(rename="sessionID")] session_id: &'a str, #[serde(rename="requestID")] request_id: &'a str }
    },
    Runtime(RuntimeKind<'a>) {
        Started => "ocmini.runtime.started" { #[serde(rename="instanceID")] instance_id: &'a str, mode: RuntimeMode },
        Stopping => "ocmini.runtime.stopping" { #[serde(rename="instanceID")] instance_id: &'a str, mode: RuntimeMode }
    },
    Config(ConfigKind<'a>) {
        Loaded => "ocmini.config.loaded" { fingerprint: &'a str, sources: &'a [Source<'a>] }
    },
    Hook(HookKind<'a>) {
        Started => "ocmini.hook.started" { #[serde(flatten)] data: Diagnostic<'a> },
        Completed => "ocmini.hook.completed" { #[serde(flatten)] data: Diagnostic<'a> },
        Failed => "ocmini.hook.failed" { #[serde(flatten)] data: Diagnostic<'a> },
        Overflow => "ocmini.hook.queue.overflow" { #[serde(flatten)] data: Overflow<'a> }
    },
    Prompt(PromptKind<'a>) {
        Accepted => "ocmini.prompt.accepted" { mode: PromptMode, #[serde(rename="messageID")] message_id: &'a str },
        Rejected => "ocmini.prompt.rejected" { reason: &'a str }
    },
    Turn(TurnKind<'a>) {
        Started => "ocmini.turn.started" { #[serde(rename="messageID")] message_id: &'a str },
        EndRequested => "ocmini.turn.end.requested" { #[serde(rename="endAttempt")] end_attempt: u64, #[serde(rename="messageID")] message_id: &'a str },
        Continued => "ocmini.turn.continued" { #[serde(rename="endAttempt")] end_attempt: u64, #[serde(rename="messageID")] message_id: &'a str },
        Ended => "ocmini.turn.ended" { reason: &'a str, started: bool, #[serde(rename="responseID")] response_id: Option<&'a str>, error: Option<&'a str>, #[serde(rename="finalizerError")] finalizer_error: Option<&'a str> }
    },
    Tool(ToolKind<'a>) {
        Started => "ocmini.tool.started" { #[serde(rename="callID")] call_id: &'a str, #[serde(rename="messageID")] message_id: &'a str, #[serde(rename="partID")] part_id: &'a str, name: &'a str },
        Completed => "ocmini.tool.completed" { #[serde(rename="callID")] call_id: &'a str, #[serde(rename="messageID")] message_id: &'a str, #[serde(rename="partID")] part_id: &'a str, error: &'a Value },
        Failed => "ocmini.tool.failed" { #[serde(rename="callID")] call_id: &'a str, #[serde(rename="messageID")] message_id: &'a str, #[serde(rename="partID")] part_id: &'a str, error: &'a Value }
    },
    Model(ModelKind<'a>) {
        Started => "ocmini.model.started" { attempt: Option<u64> },
        Completed => "ocmini.model.completed" { finish: &'a str, tokens: &'a Tokens, attempt: Option<u64> },
        Failed => "ocmini.model.failed" { error: &'a str, retryable: bool, attempt: Option<u64> },
        Retrying => "ocmini.model.retrying" { attempt: u32, #[serde(rename="delayMs")] delay_ms: u64 }
    },
    Compaction(CompactionKind<'a>) {
        Started => "ocmini.compaction.started" { mode: CompactionMode },
        Failed => "ocmini.compaction.failed" { error: &'a str }
    },
    Command(CommandKind<'a>) {
        Executed => "command.executed" { name: &'a str, arguments: &'a str, #[serde(rename="messageID")] message_id: Option<&'a str> }
    },
    Task(TaskKind<'a>) {
        Started => "ocmini.task.started" { #[serde(rename="parentSessionID")] parent_session_id: &'a str, #[serde(rename="childSessionID")] child_session_id: &'a str, agent: &'a str, model: Option<&'a ModelRef> },
        Ended => "ocmini.task.ended" { #[serde(rename="parentSessionID")] parent_session_id: &'a str, #[serde(rename="childSessionID")] child_session_id: &'a str, status: TaskStatus, #[serde(rename="messageID")] message_id: Option<&'a str> }
    },
    Skill(SkillKind<'a>) {
        Loaded => "ocmini.skill.loaded" { name: &'a str, path: &'a Value, #[serde(rename="sourceFingerprint")] source_fingerprint: &'a Value }
    },
    File(FileKind<'a>) {
        Edited => "file.edited" { file: &'a std::path::Path, tool: &'a str }
    },
    Mcp(McpKind<'a>) {
        Status => "ocmini.mcp.status" { server: &'a str, status: crate::mcp::ConnectionState }
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum SessionStatus {
    Idle,
    Busy,
    Retry {
        attempt: u32,
        message: String,
        next: i64,
    },
}

#[derive(Serialize)]
#[serde(tag = "state", rename_all = "lowercase")]
pub enum Title<'a> {
    Start,
    Done { title: &'a str },
}
#[derive(Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DeltaField {
    Text,
}
#[derive(Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RuntimeMode {
    Local,
}
#[derive(Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PromptMode {
    Context,
    Execution,
}
#[derive(Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CompactionMode {
    Manual,
    Automatic,
}
#[derive(Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskStatus {
    Completed,
    Cancelled,
    Failed,
}
#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PermissionReply {
    Once,
    Always,
    Reject,
}
impl PermissionReply {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "once" => Some(Self::Once),
            "always" => Some(Self::Always),
            "reject" => Some(Self::Reject),
            _ => None,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Once => "once",
            Self::Always => "always",
            Self::Reject => "reject",
        }
    }
}
#[derive(Clone, Copy)]
pub enum HookTarget {
    Handler(crate::hooks::HookName),
    Observer,
}
impl Serialize for HookTarget {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(match self {
            Self::Handler(hook) => hook.as_str(),
            Self::Observer => "event",
        })
    }
}
pub enum HookPhase {
    Started,
    Completed,
    Failed,
}
impl HookPhase {
    pub fn event(self, data: Diagnostic<'_>) -> Hook<'_> {
        match self {
            Self::Started => Hook::Started { data },
            Self::Completed => Hook::Completed { data },
            Self::Failed => Hook::Failed { data },
        }
    }
}

#[derive(Serialize)]
pub struct Source<'a> {
    pub id: &'a str,
    pub source: &'a str,
    pub trusted: bool,
    pub enabled: bool,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Diagnostic<'a> {
    #[serde(rename = "invocationID")]
    pub invocation_id: &'a str,
    #[serde(rename = "handlerID")]
    pub handler_id: &'a str,
    pub hook: HookTarget,
    pub source: &'a str,
    pub trusted: bool,
    pub duration_ms: u64,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub observer: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timing: Option<&'a Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_kind: Option<&'a Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stderr_artifact: Option<&'a Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diagnostic: Option<&'a str>,
}
#[derive(Serialize)]
#[serde(tag = "delivery")]
pub enum Overflow<'a> {
    #[serde(rename = "durable")]
    Durable { diagnostic: &'a str },
    #[serde(untagged)]
    BestEffort {
        #[serde(rename = "subscriptionID")]
        subscription_id: &'a str,
        dropped: u64,
    },
}

/// Serialized once at the producer/delivery boundary. Consumers share the
/// immutable payload, and wire names can only come from the event tree.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Envelope {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: EventKind,
    pub properties: Value,
}
impl Envelope {
    pub fn new<'a>(event: impl Into<Event<'a>>, context: Value) -> Self {
        let event = event.into();
        let kind = event.kind();
        let mut wire = serde_json::to_value(event).expect("event payload is serializable");
        let mut properties = wire["properties"].take();
        if properties.is_null() {
            properties = json!({});
        }
        if let Some(context) = context.as_object() {
            let properties = properties.as_object_mut().expect("object event payload");
            for (key, value) in context {
                properties.entry(key).or_insert_with(|| value.clone());
            }
        }
        Self {
            id: crate::types::hex_id("evt_"),
            kind,
            properties,
        }
    }
    pub fn scoped<'a>(event: impl Into<Event<'a>>, directory: &str, context: &Value) -> Self {
        let mut scope = json!({});
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
                scope[key] = value.clone();
            }
        }
        let mut event = Self::new(event, scope);
        // Scope owns these envelope fields even when a payload has a time field.
        event.properties["version"] = json!(1);
        event.properties["directory"] = json!(directory);
        event.properties["time"] = json!(crate::types::now_ms());
        event
    }
}

impl Envelope {
    pub fn is_nonterminal_part_update(&self) -> bool {
        if self.kind != EventKind::Part(PartKind::Updated) {
            return false;
        }
        let part = &self.properties["part"];
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

    pub fn coalesce_key(&self) -> Option<String> {
        if self.kind == EventKind::Session(SessionKind::Status) {
            let sid = self.properties["sessionID"]
                .as_str()
                .or_else(|| self.properties["info"]["id"].as_str())?;
            Some(format!("session.status:{sid}"))
        } else if self.is_nonterminal_part_update() {
            let part = &self.properties["part"];
            let sid = part["sessionID"].as_str().unwrap_or("");
            let mid = part["messageID"].as_str().unwrap_or("");
            let pid = part["id"].as_str().unwrap_or("");
            Some(format!("part.updated:{sid}:{mid}:{pid}"))
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_names_round_trip_through_the_existing_wire_format() {
        for name in NAMES {
            let kind: EventKind = serde_json::from_value(json!(name)).unwrap();
            assert_eq!(kind.as_str(), *name);
            assert_eq!(serde_json::to_value(kind).unwrap(), json!(name));
        }
        assert!(serde_json::from_value::<EventKind>(json!("session.typo")).is_err());
    }

    #[test]
    fn scoped_events_freeze_public_metadata_without_forwarding_private_context() {
        let session = SessionInfo::new("Typed event", "/project");
        let event = Envelope::scoped(
            Session::Created { info: &session },
            "/project",
            &json!({
                "sessionID":session.id,"directory":"/wrong","parentID":null,
                "headers":{"authorization":"private"},"env":{"TOKEN":"private"},"time":0
            }),
        );
        let wire = serde_json::to_value(&event).unwrap();
        assert_eq!(wire["type"], "session.created");
        assert_eq!(wire["properties"]["info"]["id"], session.id);
        assert_eq!(wire["properties"]["sessionID"], session.id);
        assert_eq!(wire["properties"]["directory"], "/project");
        assert_eq!(wire["properties"]["parentID"], Value::Null);
        assert!(wire["properties"]["time"].as_i64().unwrap() > 0);
        assert!(wire["properties"].get("headers").is_none());
        assert!(wire["properties"].get("env").is_none());
        let recovered: Envelope = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(serde_json::to_value(recovered).unwrap(), wire);
    }
}
