use serde::{Deserialize, Serialize};
use serde_json::Value;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailureKind {
    Error,
    Invalid,
    SandboxRefused,
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
            Self::Error
            | Self::Invalid
            | Self::SandboxRefused
            | Self::NotFound
            | Self::Busy
            | Self::HookReentry => "error",
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
    pub fn sandbox_refused(message: impl Into<String>) -> Self {
        Self::new(FailureKind::SandboxRefused, message)
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
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ToolOutput {
    pub output: String,
    pub title: String,
    pub metadata: Value,
}

pub fn hex_id(prefix: &str) -> String {
    const CH: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let mut s = String::with_capacity(prefix.len() + 26);
    s.push_str(prefix);
    use rand::Rng;
    s.extend((0..26).map(|_| CH[rand::rng().random_range(0..CH.len())] as char));
    s
}
