//! Child-session preparation and execution, including cancellation ownership.
use super::{decode, failure, hook_context, ToolCtx};
use crate::engine::EngineApi;
use crate::events as event;
use crate::types::{ModelRef, OperationError, PromptRequest, ToolOutput};
use serde_json::{json, Value};
use std::sync::Arc;

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub(super) struct Input {
    description: String,
    prompt: String,
    pub subagent_type: String,
    task_id: Option<String>,
}

pub(super) struct Plan {
    pub input: Input,
    parent: crate::types::Session,
    model: Option<ModelRef>,
}

struct ChildGuard {
    engine: Arc<dyn EngineApi>,
    session_id: String,
    armed: bool,
}
impl Drop for ChildGuard {
    fn drop(&mut self) {
        if self.armed {
            self.engine.abort(&self.session_id);
        }
    }
}

fn options(
    ctx: &ToolCtx,
    input: &Input,
    selected_model: Option<&ModelRef>,
) -> Result<(crate::types::Session, Option<ModelRef>), OperationError> {
    let prompt = &input.prompt;
    let agent = input.subagent_type.as_str();
    if prompt.trim().is_empty() {
        return Err(failure("prompt must not be empty"));
    }
    {
        let config = ctx.app.config.as_ref();
        if !matches!(agent, "general" | "explore") && !config.agent.contains_key(agent) {
            return Err(failure(format!("Unknown subagent {agent:?}")));
        }
        if let Some(config) = config.agent.get(agent) {
            if config["disable"] == true || config["mode"] == "primary" {
                return Err(failure(format!(
                    "Agent {agent:?} is unavailable as a subagent"
                )));
            }
        }
    }
    let parent = ctx
        .app
        .store
        .get_session(&ctx.session_id)?
        .ok_or_else(|| failure("Parent session does not exist"))?;
    let mut ancestor = parent.parent_id.clone();
    let max_depth = ctx.app.config.as_ref().subagent_depth.unwrap_or(8);
    let mut depth = 0u64;
    let mut ancestors = std::collections::HashSet::from([parent.id.clone()]);
    while let Some(id) = ancestor {
        if !ancestors.insert(id.clone()) {
            return Err(failure("Subagent session ancestry contains a cycle"));
        }
        depth += 1;
        ancestor = ctx
            .app
            .store
            .get_session(&id)?
            .ok_or_else(|| failure(format!("Ancestor session {id} does not exist")))?
            .parent_id;
    }
    if depth >= max_depth {
        return Err(failure(format!("Subagent nesting limit ({max_depth}) reached. Increase subagent_depth to allow more levels.")));
    }
    let mut model = ctx.app.store.last_model(&ctx.session_id)?;
    if let Some(selected) = ctx
        .app
        .config
        .agent
        .get(agent)
        .and_then(|value| value["model"].as_str())
    {
        model = Some(ModelRef::parse(selected)?);
    }
    if let Some(selected) = selected_model {
        model = Some(selected.clone());
    }
    if let Some(selected) = &model {
        selected.validate()?;
        if !ctx
            .app
            .config
            .model_available(&selected.provider_id, &selected.model_id)
        {
            return Err(failure("Subagent provider or model is disabled"));
        }
    }
    if let Some(id) = &input.task_id {
        let child = ctx
            .app
            .store
            .get_session(id)?
            .ok_or_else(|| failure("Task session not found"))?;
        if child.parent_id.as_deref() != Some(ctx.session_id.as_str()) {
            return Err(failure(
                "task_id must identify a child of the current session",
            ));
        }
    }
    Ok((parent, model))
}

pub(super) async fn prepare(
    ctx: &ToolCtx,
    mut input: Value,
) -> Result<(Value, Plan), OperationError> {
    let typed: Input = decode(&input)?;
    let (parent, model) = options(ctx, &typed, None)?;
    if !ctx.app.hooks.has(crate::tools::hooks::TASK_SPAWN_BEFORE) {
        return Ok((
            input,
            Plan {
                input: typed,
                parent,
                model,
            },
        ));
    }
    let mut payload = input.clone();
    payload["model"] = json!(model);
    let mut task = Plan {
        input: typed,
        parent,
        model,
    };
    let result = ctx
        .app
        .hooks
        .run_with(
            crate::tools::hooks::TASK_SPAWN_BEFORE,
            &hook_context(ctx, "task"),
            payload,
            Some(&ctx.cancel),
            |before, set| {
                let mut candidate = before.clone();
                candidate.as_object_mut().unwrap().extend(set.clone());
                let selected = if !candidate["model"].is_null()
                    && (set.contains_key("model")
                        || candidate["subagent_type"] == before["subagent_type"])
                {
                    Some(
                        serde_json::from_value::<ModelRef>(candidate["model"].clone())
                            .map_err(|e| e.to_string())?,
                    )
                } else {
                    None
                };
                let typed: Input = decode(&candidate).map_err(|e| e.message)?;
                let (parent, model) =
                    options(ctx, &typed, selected.as_ref()).map_err(|e| e.message)?;
                set.insert("model".into(), json!(model));
                task = Plan {
                    input: typed,
                    parent,
                    model,
                };
                Ok(())
            },
        )
        .await?
        .input;
    for key in ["prompt", "description", "subagent_type"] {
        input[key] = result[key].clone();
    }
    Ok((input, task))
}

pub(super) async fn run(ctx: &ToolCtx, plan: &Plan) -> Result<ToolOutput, OperationError> {
    let input = &plan.input;
    let prompt = &input.prompt;
    let description = input.description.as_str();
    let agent = input.subagent_type.as_str();
    let (parent, model) = (&plan.parent, &plan.model);
    let child = if let Some(id) = input.task_id.as_deref() {
        let child = ctx
            .app
            .store
            .get_session(id)?
            .ok_or_else(|| failure(format!("Task session {id:?} does not exist")))?;
        if child.parent_id.as_deref() != Some(ctx.session_id.as_str()) {
            return Err(failure(
                "task_id must identify a child of the current session",
            ));
        }
        child
    } else {
        let mut child = crate::types::Session::new(
            &description.chars().take(160).collect::<String>(),
            &parent.directory,
        );
        child.parent_id = Some(parent.id.clone());
        child.permission = parent.permission.clone();
        crate::sessions::create(&ctx.app, child, None).await?
    };
    ctx.update_metadata(json!({"sessionId":child.id,"model":model,"agent":agent}))?;
    let request = PromptRequest {
        model: model.clone(),
        parts: vec![json!({"type":"text","text":prompt})],
        agent: Some(agent.into()),
        ..Default::default()
    };
    let mut guard = ChildGuard {
        engine: ctx.engine.clone(),
        session_id: child.id.clone(),
        armed: true,
    };
    let mut context = hook_context(ctx, "task");
    context["childSessionID"] = json!(child.id);
    let done = tokio::select! {
        result = ctx.engine.admit(ctx.app.clone(), child.id.clone(), request) => result?,
        _ = ctx.cancel.cancelled() => return Err(failure("Subagent cancelled")),
    };
    ctx.app.hooks.emit(
        event::Task::Started {
            parent_session_id: &ctx.session_id,
            child_session_id: &child.id,
            agent,
            model: model.as_ref(),
        },
        &context,
    );
    let result = tokio::select! {
        result = done => result.map_err(|_| "Subagent ended without a result".to_owned()).and_then(|result| result.map_err(String::from)),
        _ = ctx.cancel.cancelled() => Err("Subagent cancelled".into()),
    };
    let status = match &result {
        Ok(_) => event::TaskStatus::Completed,
        Err(_) if ctx.cancel.is_cancelled() => event::TaskStatus::Cancelled,
        Err(_) => event::TaskStatus::Failed,
    };
    ctx.app.hooks.emit(
        event::Task::Ended {
            parent_session_id: &ctx.session_id,
            child_session_id: &child.id,
            status,
            message_id: result.as_ref().ok().map(|message| message.info.id.as_str()),
        },
        &context,
    );
    let result = result?;
    guard.armed = false;
    let text = crate::types::text_parts(&result.parts)
        .collect::<Vec<_>>()
        .join("\n");
    Ok(ToolOutput {
        output: format!("Task session: {}\n\n{text}", child.id),
        title: description.into(),
        metadata: json!({"sessionId":child.id,"model":model,"agent":agent}),
    })
}
