use crate::events as event;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

pub fn stable_hash(value: &str) -> u64 {
    value.bytes().fold(0xcbf29ce484222325, |hash, byte| {
        (hash ^ byte as u64).wrapping_mul(0x100000001b3)
    })
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(i64::MAX as u128) as i64)
        .unwrap_or(0)
}

pub fn hex_id(prefix: &str) -> String {
    const CH: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let mut s = String::with_capacity(prefix.len() + 26);
    s.push_str(prefix);
    use rand::Rng;
    s.extend((0..26).map(|_| CH[rand::rng().random_range(0..CH.len())] as char));
    s
}

pub fn slug() -> String {
    const A: &[&str] = &[
        "clever", "silent", "brave", "lucky", "calm", "eager", "gentle", "proud",
    ];
    const B: &[&str] = &[
        "eagle", "falcon", "otter", "wolf", "comet", "river", "maple", "ember",
    ];
    use rand::Rng;
    format!(
        "{}-{}",
        A[rand::rng().random_range(0..A.len())],
        B[rand::rng().random_range(0..B.len())]
    )
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct Time {
    pub created: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated: Option<i64>,
    #[serde(default, flatten)]
    pub extra: serde_json::Map<String, Value>,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum PermissionAction {
    Allow,
    Ask,
    Deny,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct PermissionRule {
    pub permission: String,
    #[serde(default = "star")]
    pub pattern: String,
    pub action: PermissionAction,
}

fn star() -> String {
    "*".into()
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug)]
#[serde(rename_all = "lowercase")]
pub enum TitleSource {
    Explicit,
    Automatic,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Session {
    pub id: String,
    pub slug: String,
    #[serde(rename = "projectID")]
    pub project_id: String,
    pub directory: String,
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub cost: f64,
    #[serde(default)]
    pub tokens: Tokens,
    pub title: String,
    #[serde(
        default,
        rename = "ocminiTitleSource",
        skip_serializing_if = "Option::is_none"
    )]
    pub title_source: Option<TitleSource>,
    pub version: String,
    pub time: Time,
    #[serde(rename = "parentID", skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permission: Option<Vec<PermissionRule>>,
    #[serde(default, flatten)]
    pub extra: serde_json::Map<String, Value>,
}

impl Session {
    pub fn new(title: &str, directory: &str) -> Self {
        let now = now_ms();
        Self {
            id: hex_id("ses_"),
            slug: slug(),
            project_id: "global".into(),
            directory: directory.into(),
            path: directory.into(),
            title: title.into(),
            title_source: None,
            version: crate::OPENCODE_COMPAT_VERSION.into(),
            cost: 0.0,
            tokens: Tokens::default(),
            time: Time {
                created: now,
                updated: Some(now),
                ..Default::default()
            },
            parent_id: None,
            permission: None,
            extra: Default::default(),
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct Tokens {
    pub input: i64,
    pub output: i64,
    pub reasoning: i64,
    pub cache: Cache,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct Cache {
    pub read: i64,
    pub write: i64,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct MessageInfo {
    pub id: String,
    #[serde(rename = "sessionID")]
    pub session_id: String,
    pub role: String,
    pub time: MessageTime,
    #[serde(rename = "modelID", skip_serializing_if = "Option::is_none")]
    pub model_id: Option<String>,
    #[serde(rename = "providerID", skip_serializing_if = "Option::is_none")]
    pub provider_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens: Option<Tokens>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finish: Option<String>,
    #[serde(rename = "parentID", default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
    #[serde(default, flatten)]
    pub extra: serde_json::Map<String, Value>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct MessageTime {
    pub created: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed: Option<i64>,
}

impl MessageInfo {
    pub fn assistant(sid: &str, parent: Option<String>, model: &ModelRef, agent: &str) -> Self {
        Self {
            id: hex_id("msg_"),
            session_id: sid.into(),
            role: "assistant".into(),
            time: MessageTime {
                created: now_ms(),
                completed: None,
            },
            parent_id: parent,
            model_id: Some(model.model_id.clone()),
            provider_id: Some(model.provider_id.clone()),
            agent: Some(agent.into()),
            mode: Some(agent.into()),
            cost: Some(0.0),
            tokens: Some(Tokens::default()),
            ..Default::default()
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailureKind {
    Error,
    Invalid,
    NotFound,
    Busy,
    Cancelled,
    HookRejected,
    HookReentry,
    HookLimit,
    StepLimit,
}
impl FailureKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Error | Self::Invalid | Self::NotFound | Self::Busy | Self::HookReentry => {
                "error"
            }
            Self::Cancelled => "cancelled",
            Self::HookRejected => "hook_rejected",
            Self::HookLimit => "hook_limit",
            Self::StepLimit => "step_limit",
        }
    }
}
#[derive(Clone, Debug, Serialize)]
#[serde(transparent)]
pub struct OperationError {
    #[serde(skip)]
    pub kind: FailureKind,
    pub message: String,
}
impl OperationError {
    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new(FailureKind::Invalid, message)
    }
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(FailureKind::NotFound, message)
    }
    pub fn busy(message: impl Into<String>) -> Self {
        Self::new(FailureKind::Busy, message)
    }
    pub fn new(kind: FailureKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}
impl From<String> for OperationError {
    fn from(message: String) -> Self {
        Self::new(FailureKind::Error, message)
    }
}
impl From<&str> for OperationError {
    fn from(message: &str) -> Self {
        message.to_owned().into()
    }
}
impl From<OperationError> for String {
    fn from(error: OperationError) -> Self {
        error.message
    }
}
impl std::ops::Deref for OperationError {
    type Target = str;
    fn deref(&self) -> &str {
        &self.message
    }
}
impl std::fmt::Display for OperationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for OperationError {}
pub type TurnResult = Result<MessageWithParts, OperationError>;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PromptPartScope {
    User,
    Expanded,
    Compaction,
}

impl PromptRequest {
    pub fn part_scope(&self) -> PromptPartScope {
        if self.command.as_deref() == Some("__ocmini_compact") {
            PromptPartScope::Compaction
        } else {
            PromptPartScope::Expanded
        }
    }
}

pub fn validate_parts(parts: &[Value], scope: PromptPartScope) -> Result<(), OperationError> {
    if parts.is_empty() {
        return Err(OperationError::invalid(
            "Prompt requires at least one text or file part",
        ));
    }
    for part in parts {
        let valid = match part["type"].as_str() {
            Some("text") => part["text"].is_string(),
            Some("file") => part["url"].is_string() && part["mime"].is_string(),
            Some("agent") => scope != PromptPartScope::User && part["name"].is_string(),
            Some("compaction") => scope == PromptPartScope::Compaction,
            _ => false,
        };
        if !valid {
            return Err(OperationError::invalid(
                "Invalid or unsupported prompt part",
            ));
        }
    }
    Ok(())
}

pub fn text_parts(parts: &[Value]) -> impl Iterator<Item = &str> {
    parts
        .iter()
        .filter(|part| part["type"] == "text")
        .filter_map(|part| part["text"].as_str())
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct MessageWithParts {
    pub info: MessageInfo,
    pub parts: Vec<Value>,
}

pub use event::Envelope as BusEvent;

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct ModelRef {
    #[serde(rename = "providerID")]
    pub provider_id: String,
    #[serde(rename = "modelID")]
    pub model_id: String,
}

impl ModelRef {
    pub fn validate(&self) -> Result<(), String> {
        if self.provider_id.trim().is_empty() || self.model_id.trim().is_empty() {
            return Err("model requires nonempty providerID and modelID".into());
        }
        Ok(())
    }

    /// Parse a `provider/model` reference, the shape config strings and CLI
    /// flags use everywhere.
    pub fn parse(value: &str) -> Result<Self, String> {
        let (provider_id, model_id) = value
            .split_once('/')
            .ok_or("model must be provider/model")?;
        let model = Self {
            provider_id: provider_id.into(),
            model_id: model_id.into(),
        };
        model.validate()?;
        Ok(model)
    }
}

impl std::fmt::Display for ModelRef {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}/{}", self.provider_id, self.model_id)
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(rename_all = "camelCase")]
#[serde(default)]
pub struct PromptRequest {
    pub model: Option<ModelRef>,
    pub parts: Vec<Value>,
    #[serde(rename = "messageID")]
    pub message_id: Option<String>,
    pub system: Option<String>,
    pub agent: Option<String>,
    pub variant: Option<String>,
    pub no_reply: bool,
    pub tools: Option<HashMap<String, bool>>,
    pub command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command_arguments: Option<String>,
    /// Runtime-only registrations: HTTP clients cannot install executable hooks.
    #[serde(skip)]
    pub hook_snapshot: Option<Value>,
    #[serde(skip)]
    pub admission_token: Option<String>,
}

#[derive(Clone, Debug)]
pub enum ProviderEvent {
    TextDelta(String),
    ReasoningDelta(String),
    ToolCallStart {
        call_id: String,
        name: String,
        args: String,
    },
    Finished {
        finish: String,
        usage: Tokens,
    },
    Metadata(Value),
    Error(String),
}

#[derive(Deserialize, Debug)]
pub struct QuestionReply {
    pub answers: Vec<Vec<String>>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionRequest {
    #[serde(skip)]
    pub context: Value,
    pub id: String,
    #[serde(rename = "sessionID")]
    pub session_id: String,
    pub permission: String,
    pub patterns: Vec<String>,
    pub metadata: Value,
    pub always: Vec<String>,
    pub time: Time,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool: Option<Value>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QuestionRequest {
    #[serde(skip)]
    pub context: Value,
    pub id: String,
    #[serde(rename = "sessionID")]
    pub session_id: String,
    pub questions: Vec<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool: Option<Value>,
}

impl QuestionRequest {
    pub fn validate_answers(&self, answers: &[Vec<String>]) -> Result<(), String> {
        if answers.len() != self.questions.len() {
            return Err("answers must have one array for each question".into());
        }
        for (answers, question) in answers.iter().zip(&self.questions) {
            validate_answer(question, answers)?;
        }
        Ok(())
    }
}

pub fn validate_answer(question: &Value, answers: &[String]) -> Result<(), String> {
    if question["multiple"] != true && answers.len() > 1 {
        return Err("A question without multiple=true accepts at most one answer".into());
    }
    Ok(())
}

#[derive(Clone, Debug, Deserialize)]
pub struct ToolOutput {
    pub output: String,
    pub title: String,
    pub metadata: Value,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operation_errors_keep_their_kind_and_existing_wire_shape() {
        let error = OperationError::new(FailureKind::HookRejected, "blocked by handler");
        assert_eq!(error.kind, FailureKind::HookRejected);
        assert_eq!(error.to_string(), "blocked by handler");
        assert_eq!(serde_json::to_value(&error).unwrap(), "blocked by handler");
        let message: String = error.into();
        assert_eq!(message, "blocked by handler");
    }
}
