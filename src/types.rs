use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

pub fn hex_id(prefix: &str) -> String {
    const CH: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let mut s = String::with_capacity(prefix.len() + 26);
    s.push_str(prefix);
    use rand::Rng;
    let bytes: Vec<u8> = (0..26).map(|_| CH[rand::rng().random_range(0..CH.len())]).collect();
    s.push_str(&String::from_utf8(bytes).unwrap());
    s
}

pub fn slug() -> String {
    const A: &[&str] = &["clever", "silent", "brave", "lucky", "calm", "eager", "gentle", "proud"];
    const B: &[&str] = &["eagle", "falcon", "otter", "wolf", "comet", "river", "maple", "ember"];
    use rand::Rng;
    format!("{}-{}", A[rand::rng().random_range(0..A.len())], B[rand::rng().random_range(0..B.len())])
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Time {
    pub created: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated: Option<i64>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct PermissionRule {
    pub permission: String,
    #[serde(default = "star")]
    pub pattern: String,
    pub action: String,
}

fn star() -> String {
    "*".into()
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
    pub version: String,
    pub time: Time,
    #[serde(rename = "parentID", skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permission: Option<Vec<PermissionRule>>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Tokens {
    #[serde(default)]
    pub input: i64,
    #[serde(default)]
    pub output: i64,
    #[serde(default)]
    pub reasoning: i64,
    #[serde(default)]
    pub cache: Cache,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Cache {
    #[serde(default)]
    pub read: i64,
    #[serde(default)]
    pub write: i64,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
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
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct MessageTime {
    pub created: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed: Option<i64>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct MessageWithParts {
    pub info: MessageInfo,
    pub parts: Vec<Value>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct BusEvent {
    pub id: String,
    pub r#type: String,
    pub properties: Value,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct PromptPart {
    pub r#type: String,
    #[serde(default)]
    pub text: String,
}

#[derive(Deserialize, Debug)]
pub struct ModelRef {
    #[serde(rename = "providerID")]
    pub provider_id: String,
    #[serde(rename = "modelID")]
    pub model_id: String,
}

#[derive(Deserialize, Debug)]
pub struct PromptRequest {
    pub model: ModelRef,
    pub parts: Vec<PromptPart>,
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub variant: Option<String>,
}

#[derive(Clone, Debug)]
pub enum ProviderEvent {
    TextDelta(String),
    ReasoningDelta(String),
    ToolCallStart { call_id: String, name: String, args: String },
    Finished { finish: String, usage: Tokens },
    Error(String),
}

#[derive(Deserialize, Debug)]
pub struct PermissionReply {
    pub response: String,
}

#[derive(Deserialize, Debug)]
pub struct QuestionReply {
    pub answers: Vec<Vec<String>>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionRequest {
    pub id: String,
    #[serde(rename = "sessionID")]
    pub session_id: String,
    pub permission: String,
    pub patterns: Vec<String>,
    pub metadata: Value,
    pub always: Vec<String>,
    pub time: Time,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QuestionRequest {
    pub id: String,
    #[serde(rename = "sessionID")]
    pub session_id: String,
    pub questions: Vec<Value>,
}

pub struct ToolOutput {
    pub output: String,
    pub title: String,
}

pub struct ToolError {
    pub message: String,
    pub denied: bool,
}

#[derive(Default, Debug)]
pub struct Usage {
    pub per_model: HashMap<String, (i64, i64)>,
}
