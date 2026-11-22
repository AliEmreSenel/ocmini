//! Adapters for tools' application services. Tool behavior stays in ocmini-tools.
use crate::{
    engine::{App, EngineApi},
    events as event,
    types::{ModelRef, PromptRequest, Session, ToolOutput},
};
use ocmini_tools::types::OperationError;
use ocmini_tools::{
    hook_types::Hook,
    host::{Completion, Host, Model, TaskRequest, TaskSession, Validator},
    ToolCtx,
};
use serde_json::{json, Value};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
fn task_session(s: Session) -> TaskSession {
    TaskSession {
        id: s.id,
        directory: s.directory,
        parent_id: s.parent_id,
        permission: json!(s.permission),
    }
}
fn model(m: Model) -> ModelRef {
    ModelRef {
        provider_id: m.provider_id,
        model_id: m.model_id,
    }
}
#[async_trait::async_trait]
impl Host for App {
    type Engine = Arc<dyn EngineApi>;
    fn update_metadata(&self, ctx: &ToolCtx<Self>, metadata: Value) -> Result<(), OperationError> {
        let mut part = self
            .store
            .message_part(&ctx.session_id, &ctx.message_id, &ctx.part_id)?
            .ok_or("Tool part no longer exists")?;
        part["state"]["metadata"] = metadata;
        crate::messages::update_part(self, &part)?;
        Ok(())
    }
    fn ownership(&self, session: &str) -> Result<(), OperationError> {
        self.store.check_ownership(session).map_err(Into::into)
    }
    fn has_hook(&self, h: Hook) -> bool {
        self.hooks.has(h)
    }
    fn matches_hook(&self, h: Hook, c: &Value) -> bool {
        self.hooks.matches(h, c)
    }
    async fn hook(
        &self,
        h: Hook,
        c: &Value,
        input: Value,
        cancel: &CancellationToken,
        validator: Option<Validator<'_>>,
    ) -> Result<Value, OperationError> {
        match validator {
            Some(mut validate) => Ok(self
                .hooks
                .run_with(h, c, input, Some(cancel), move |before, set| {
                    validate(before, set)
                })
                .await?
                .input),
            None => Ok(self.hooks.run(h, c, input, Some(cancel)).await?.input),
        }
    }
    fn emit(&self, name: &str, c: &Value, data: Value) {
        let s = |k: &str| data[k].as_str().unwrap_or_default();
        match name {
            "file.edited" => self.hooks.emit(
                event::File::Edited {
                    file: std::path::Path::new(s("file")),
                    tool: s("tool"),
                },
                c,
            ),
            "todo.updated" => self.hooks.emit(
                event::Todo::Updated {
                    session_id: s("sessionID"),
                    todos: &data["todos"],
                },
                c,
            ),
            "skill.loaded" => self.hooks.emit(
                event::Skill::Loaded {
                    name: s("name"),
                    path: &data["path"],
                    source_fingerprint: &data["sourceFingerprint"],
                },
                c,
            ),
            "task.started" => {
                let selected: Option<ModelRef> =
                    serde_json::from_value(data["model"].clone()).ok().flatten();
                self.hooks.emit(
                    event::Task::Started {
                        parent_session_id: s("parentSessionID"),
                        child_session_id: s("childSessionID"),
                        agent: s("agent"),
                        model: selected.as_ref(),
                    },
                    c,
                );
            }
            "task.ended" => self.hooks.emit(
                event::Task::Ended {
                    parent_session_id: s("parentSessionID"),
                    child_session_id: s("childSessionID"),
                    status: match s("status") {
                        "completed" => event::TaskStatus::Completed,
                        "cancelled" => event::TaskStatus::Cancelled,
                        _ => event::TaskStatus::Failed,
                    },
                    message_id: data["messageID"].as_str(),
                },
                c,
            ),
            _ => {}
        }
    }
    fn session_context(&self, id: &str) -> Value {
        App::session_context(self, id, json!({}))
    }
    fn session(&self, id: &str) -> Result<Option<TaskSession>, OperationError> {
        self.store
            .get_session(id)
            .map(|s| s.map(task_session))
            .map_err(Into::into)
    }
    fn agent_config(&self, name: &str) -> Option<Value> {
        self.config.agent.get(name).cloned()
    }
    fn subagent_depth(&self) -> u64 {
        self.config.subagent_depth.unwrap_or(8)
    }
    fn last_model(&self, id: &str) -> Result<Option<Model>, OperationError> {
        Ok(self.store.last_model(id)?.map(|m| Model {
            provider_id: m.provider_id,
            model_id: m.model_id,
        }))
    }
    fn model_available(&self, m: &Model) -> bool {
        self.config.model_available(&m.provider_id, &m.model_id)
    }
    async fn create_child(
        &self,
        parent: &TaskSession,
        description: &str,
    ) -> Result<TaskSession, OperationError> {
        let mut child = Session::new(
            &description.chars().take(160).collect::<String>(),
            &parent.directory,
        );
        child.parent_id = Some(parent.id.clone());
        child.permission = serde_json::from_value(parent.permission.clone())
            .map_err(|e| OperationError::from(e.to_string()))?;
        Ok(task_session(
            crate::sessions::create(self, child, None).await?,
        ))
    }
    async fn admit(
        &self,
        engine: &Self::Engine,
        child: &str,
        request: TaskRequest,
    ) -> Result<Completion, OperationError> {
        let done = engine
            .admit(
                self.clone(),
                child.into(),
                PromptRequest {
                    model: request.model.map(model),
                    parts: vec![json!({"type":"text","text":request.prompt})],
                    agent: Some(request.agent),
                    variant: request.variant,
                    ..Default::default()
                },
            )
            .await?;
        let (tx, rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            if let Ok(result) = done.await {
                let _ = tx.send(
                    result
                        .map(|m| ocmini_tools::host::TaskResult {
                            message_id: m.info.id,
                            text: crate::types::text_parts(&m.parts)
                                .collect::<Vec<_>>()
                                .join("\n"),
                        })
                        .map_err(Into::into),
                );
            }
        });
        Ok(rx)
    }
    fn abort(&self, engine: &Self::Engine, id: &str) {
        engine.abort(id)
    }
    async fn deliver(
        &self,
        engine: Self::Engine,
        parent: String,
        parent_message: String,
        child: String,
        description: String,
        result: Result<ocmini_tools::host::TaskResult, OperationError>,
    ) {
        let (state, body, message_id) = match result {
            Ok(r) => ("completed", r.text, Some(r.message_id)),
            Err(e) if e.message == "Aborted" => {
                self.hooks.emit(
                    event::Task::Ended {
                        parent_session_id: &parent,
                        child_session_id: &child,
                        status: event::TaskStatus::Cancelled,
                        message_id: None,
                    },
                    &self.session_context(&parent, json!({})),
                );
                return;
            }
            Err(e) => ("error", e.message, None),
        };
        self.hooks.emit(
            event::Task::Ended {
                parent_session_id: &parent,
                child_session_id: &child,
                status: if state == "completed" {
                    event::TaskStatus::Completed
                } else {
                    event::TaskStatus::Failed
                },
                message_id: message_id.as_deref(),
            },
            &self.session_context(&parent, json!({})),
        );
        let tag = if state == "completed" {
            "task_result"
        } else {
            "task_error"
        };
        let summary = if state == "completed" {
            "Background task completed"
        } else {
            "Background task failed"
        };
        let text=format!("<task id=\"{child}\" state=\"{state}\">\n<summary>{summary}: {description}</summary>\n<{tag}>\n{body}\n</{tag}>\n</task>");
        let v2_enabled = self.v2.enabled(&parent);
        let agent = if v2_enabled {
            None
        } else {
            self.store
                .message(&parent, &parent_message)
                .ok()
                .flatten()
                .and_then(|m| m.info.agent)
        };
        let model = if v2_enabled {
            None
        } else {
            self.store.last_model(&parent).ok().flatten()
        };
        let v2 = v2_enabled.then(|| crate::v2::Input {
            kind: "synthetic".into(),
            payload: json!({"text":text,"metadata":{"source":"subagent","childID":child,"state":state,"description":description}}),
            ..Default::default()
        });
        if let Ok(done) = engine
            .admit(
                self.clone(),
                parent,
                PromptRequest {
                    model,
                    v2,
                    parts: vec![json!({"type":"text","text":text,"synthetic":true})],
                    agent,
                    ..Default::default()
                },
            )
            .await
        {
            drop(done);
        }
    }
    async fn ask(
        &self,
        ctx: &ToolCtx<Self>,
        questions: Value,
    ) -> Result<Vec<Vec<String>>, OperationError> {
        struct Guard {
            state: crate::engine::question::QuestionState,
            id: String,
        }
        impl Drop for Guard {
            fn drop(&mut self) {
                self.state.reject(&self.id);
            }
        }
        let (request, rx) = self.questions.ask_for_tool(
            &ctx.session_id,
            questions,
            Some(json!({"messageID":ctx.message_id,"callID":ctx.call_id})),
            ocmini_tools::hook_context(ctx, "question"),
        );
        let _guard = Guard {
            state: self.questions.clone(),
            id: request.id.clone(),
        };
        self.hooks.emit(
            event::Question::Asked { request: &request },
            &self.session_context(&ctx.session_id, json!({})),
        );
        tokio::select! {reply=rx=>reply.map_err(|_|OperationError::from("Question cancelled"))?.ok_or_else(||OperationError::from("User dismissed the question")),_=ctx.cancel.cancelled()=>Err("Question cancelled".into())}
    }
    fn todos(&self, id: &str) -> Result<Value, OperationError> {
        self.store.todos(id).map_err(Into::into)
    }
    fn set_todos(&self, id: &str, todos: &Value) -> Result<(), OperationError> {
        self.store.set_todos(id, todos).map_err(Into::into)
    }
    fn skill_paths(&self) -> Vec<String> {
        self.config.skill_paths()
    }
    fn mcp_route(&self, name: &str) -> Option<(String, String)> {
        self.mcp().route(name)
    }
    async fn mcp_call(
        &self,
        server: &str,
        tool: &str,
        args: Value,
        cancel: CancellationToken,
    ) -> Result<ToolOutput, OperationError> {
        let result = self
            .mcp()
            .call_result_async(server, tool, args, cancel)
            .await?;
        Ok(crate::mcp::tool_output(server, tool, &result))
    }
    fn release_file(&self, session: &str, handle: u64) {
        self.effects.release_remote(session, handle);
        #[cfg(feature = "sandbox")]
        if self.sandbox.enabled(session) {
            let registry = self.sandbox.clone();
            let session = session.to_owned();
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                runtime.spawn(async move {
                    let _ = registry
                        .request(
                            &session,
                            "discard",
                            json!({"handle":handle}),
                            CancellationToken::new(),
                        )
                        .await;
                });
            }
            return;
        }
        self.effects.discard(session, handle);
    }
    fn remote(&self, session: &str) -> bool {
        #[cfg(feature = "sandbox")]
        {
            self.sandbox.enabled(session)
        }
        #[cfg(not(feature = "sandbox"))]
        {
            let _ = session;
            false
        }
    }
    fn output_directory(&self, session: &str) -> std::path::PathBuf {
        self.tool_output_directory(session)
    }
    async fn effect(
        &self,
        ctx: &ToolCtx<Self>,
        method: &str,
        args: Value,
    ) -> Result<ocmini_tools::effects::EffectResult, OperationError> {
        #[cfg(feature = "sandbox")]
        if self.sandbox.enabled(&ctx.session_id) {
            let lease = self
                .effects
                .before_remote(&ctx.session_id, method, &args, &ctx.cancel)
                .await?;
            let coordinate_args = json!({"name":args["name"],"handle":args["handle"]});
            let value = self
                .sandbox
                .request_retained(
                    &ctx.session_id,
                    method,
                    args,
                    ctx.cancel.clone(),
                    lease,
                    Some(&ctx.dir),
                )
                .await
                .map_err(OperationError::from)?;
            if let Err(error) = self
                .effects
                .after_remote(
                    &ctx.session_id,
                    method,
                    &coordinate_args,
                    &value,
                    &ctx.cancel,
                )
                .await
            {
                if let Some(handle) = value["handle"]
                    .as_u64()
                    .or(coordinate_args["handle"].as_u64())
                {
                    self.release_file(&ctx.session_id, handle);
                }
                return Err(error);
            }
            return ocmini_tools::effects::EffectResult::from_wire(method, value);
        }
        self.effects
            .dispatch(&ctx.dir, &ctx.session_id, method, args, ctx.cancel.clone())
            .await
    }
}
