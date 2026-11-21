//! Application services used by tools. The implementations live in core;
//! no tool needs the application's database, engine, HTTP server or sandbox types.
use crate::{
    hook_types::Hook,
    types::{OperationError, ToolOutput},
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
pub type Validator<'a> =
    Box<dyn FnMut(&Value, &mut Map<String, Value>) -> Result<(), String> + Send + 'a>;
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct TaskSession {
    pub id: String,
    pub directory: String,
    pub parent_id: Option<String>,
    pub permission: Value,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Model {
    #[serde(rename = "providerID")]
    pub provider_id: String,
    #[serde(rename = "modelID")]
    pub model_id: String,
}
impl Model {
    pub fn parse(s: &str) -> Result<Self, OperationError> {
        let (provider, model) = s.split_once('/').ok_or("Model must use provider/model")?;
        if provider.is_empty() || model.is_empty() {
            return Err("Invalid model".into());
        }
        Ok(Self {
            provider_id: provider.into(),
            model_id: model.into(),
        })
    }
}
#[derive(Clone)]
pub struct TaskRequest {
    pub model: Option<Model>,
    pub prompt: String,
    pub agent: String,
    pub variant: Option<String>,
}
#[derive(Clone)]
pub struct TaskResult {
    pub message_id: String,
    pub text: String,
}
pub type Completion = tokio::sync::oneshot::Receiver<Result<TaskResult, OperationError>>;
#[async_trait::async_trait]
pub trait Host: Clone + Send + Sync + 'static {
    type Engine: Clone + Send + Sync + 'static;
    fn update_metadata(
        &self,
        ctx: &crate::ToolCtx<Self>,
        metadata: Value,
    ) -> Result<(), OperationError>;
    fn ownership(&self, session: &str) -> Result<(), OperationError>;
    fn has_hook(&self, hook: Hook) -> bool;
    fn matches_hook(&self, hook: Hook, context: &Value) -> bool;
    async fn hook(
        &self,
        hook: Hook,
        context: &Value,
        input: Value,
        cancel: &CancellationToken,
        validate: Option<Validator<'_>>,
    ) -> Result<Value, OperationError>;
    fn emit(&self, name: &str, context: &Value, data: Value);
    fn session_context(&self, session: &str) -> Value;
    fn session(&self, id: &str) -> Result<Option<TaskSession>, OperationError>;
    fn agent_config(&self, name: &str) -> Option<Value>;
    fn subagent_depth(&self) -> u64;
    fn last_model(&self, session: &str) -> Result<Option<Model>, OperationError>;
    fn model_available(&self, model: &Model) -> bool;
    async fn create_child(
        &self,
        parent: &TaskSession,
        description: &str,
    ) -> Result<TaskSession, OperationError>;
    async fn admit(
        &self,
        engine: &Self::Engine,
        child: &str,
        request: TaskRequest,
    ) -> Result<Completion, OperationError>;
    fn abort(&self, engine: &Self::Engine, session: &str);
    async fn deliver(
        &self,
        engine: Self::Engine,
        parent: String,
        parent_message: String,
        child: String,
        description: String,
        result: Result<TaskResult, OperationError>,
    );
    async fn ask(
        &self,
        ctx: &crate::ToolCtx<Self>,
        questions: Value,
    ) -> Result<Vec<Vec<String>>, OperationError>;
    fn todos(&self, session: &str) -> Result<Value, OperationError>;
    fn set_todos(&self, session: &str, todos: &Value) -> Result<(), OperationError>;
    fn skill_paths(&self) -> Vec<String>;
    fn mcp_route(&self, name: &str) -> Option<(String, String)>;
    async fn mcp_call(
        &self,
        server: &str,
        tool: &str,
        args: Value,
        cancel: CancellationToken,
    ) -> Result<ToolOutput, OperationError>;
    fn remote(&self, session: &str) -> bool;
    fn output_directory(&self, _session: &str) -> std::path::PathBuf {
        crate::output::directory()
    }
    fn release_file(&self, session: &str, handle: u64);
    async fn effect(
        &self,
        ctx: &crate::ToolCtx<Self>,
        method: &str,
        args: Value,
    ) -> Result<crate::effects::EffectResult, OperationError>;
}
#[derive(Clone)]
pub struct ToolCtx<A: Host> {
    pub dir: String,
    pub app: A,
    pub session_id: String,
    pub message_id: String,
    pub call_id: String,
    pub part_id: String,
    pub engine: A::Engine,
    pub cancel: CancellationToken,
    pub hook_context: Value,
}
impl<A: Host> ToolCtx<A> {
    pub fn update_metadata(&self, metadata: Value) -> Result<(), OperationError> {
        self.app.update_metadata(self, metadata)
    }
}
pub fn context<A: Host>(ctx: &ToolCtx<A>, tool: &str) -> Value {
    let mut c = ctx.hook_context.clone();
    if !c.is_object() {
        c = serde_json::json!({});
    }
    c["directory"] = ctx.dir.clone().into();
    c["sessionID"] = ctx.session_id.clone().into();
    c["messageID"] = ctx.message_id.clone().into();
    c["callID"] = ctx.call_id.clone().into();
    c["tool"] = tool.into();
    c["requestKind"] = "primary".into();
    c
}
pub type SharedValidator = Arc<dyn Fn(&Value) -> Result<(), OperationError> + Send + Sync>;
