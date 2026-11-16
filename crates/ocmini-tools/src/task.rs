use crate::{
    decode, failure, hook_context,
    host::{Host, Model, TaskRequest, TaskResult, TaskSession},
    types::{OperationError, ToolOutput},
    ToolCtx,
};
use serde_json::{json, Value};
#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct Input {
    description: String,
    prompt: String,
    #[serde(alias = "agent")]
    pub subagent_type: String,
    #[serde(alias = "sessionID")]
    task_id: Option<String>,
    model: Option<String>,
    #[serde(default)]
    background: bool,
}
pub struct Plan {
    pub input: Input,
    parent: TaskSession,
    model: Option<Model>,
}
fn options<A: Host>(
    ctx: &ToolCtx<A>,
    input: &Input,
    selected: Option<Model>,
) -> Result<(TaskSession, Option<Model>), OperationError> {
    if input.prompt.trim().is_empty() {
        return Err(failure("prompt must not be empty"));
    }
    let agent = &input.subagent_type;
    let config = ctx.app.agent_config(agent);
    if !matches!(agent.as_str(), "general" | "explore") && config.is_none() {
        return Err(failure(format!("Unknown subagent {agent:?}")));
    }
    if config
        .as_ref()
        .is_some_and(|c| c["disable"] == true || c["mode"] == "primary")
    {
        return Err(failure(format!(
            "Agent {agent:?} is unavailable as a subagent"
        )));
    }
    let parent = ctx
        .app
        .session(&ctx.session_id)?
        .ok_or_else(|| failure("Parent session does not exist"))?;
    let mut ancestor = parent.parent_id.clone();
    let mut ancestors = std::collections::HashSet::from([parent.id.clone()]);
    let mut depth = 0;
    while let Some(id) = ancestor {
        if !ancestors.insert(id.clone()) {
            return Err(failure("Subagent session ancestry contains a cycle"));
        }
        depth += 1;
        ancestor = ctx
            .app
            .session(&id)?
            .ok_or_else(|| failure(format!("Ancestor session {id} does not exist")))?
            .parent_id;
    }
    let max = ctx.app.subagent_depth();
    if depth >= max {
        return Err(failure(format!(
            "Subagent nesting limit ({max}) reached. Increase subagent_depth to allow more levels."
        )));
    }
    let mut model = ctx.app.last_model(&ctx.session_id)?;
    if let Some(s) = config.as_ref().and_then(|c| c["model"].as_str()) {
        model = Some(Model::parse(s)?);
    }
    if let Some(s) = &input.model {
        model = Some(Model::parse(s.split('#').next().unwrap_or(s))?);
    }
    if selected.is_some() {
        model = selected;
    }
    if model.as_ref().is_some_and(|m| !ctx.app.model_available(m)) {
        return Err(failure("Subagent provider or model is disabled"));
    }
    if let Some(id) = &input.task_id {
        let child = ctx
            .app
            .session(id)?
            .ok_or_else(|| failure("Task session not found"))?;
        if child.parent_id.as_deref() != Some(ctx.session_id.as_str()) {
            return Err(failure(
                "task_id must identify a child of the current session",
            ));
        }
    }
    Ok((parent, model))
}
pub async fn prepare<A: Host>(
    ctx: &ToolCtx<A>,
    mut args: Value,
) -> Result<(Value, Plan), OperationError> {
    let input: Input = decode(&args)?;
    let (parent, model) = options(ctx, &input, None)?;
    let mut plan = Plan {
        input,
        parent,
        model,
    };
    if ctx.app.has_hook(crate::hooks::TASK_SPAWN_BEFORE) {
        let mut payload = args.clone();
        payload["model"] = json!(plan.model);
        let mut validator = |before: &Value, set: &mut serde_json::Map<String, Value>| {
            let mut candidate = before.clone();
            candidate.as_object_mut().unwrap().extend(set.clone());
            let model = if !candidate["model"].is_null()
                && (set.contains_key("model")
                    || candidate["subagent_type"] == before["subagent_type"])
            {
                Some(
                    serde_json::from_value(candidate["model"].clone())
                        .map_err(|e| e.to_string())?,
                )
            } else {
                None
            };
            let input: Input = decode(&candidate).map_err(String::from)?;
            let (parent, model) = options(ctx, &input, model).map_err(String::from)?;
            set.insert("model".into(), json!(model));
            plan = Plan {
                input,
                parent,
                model,
            };
            Ok(())
        };
        let result = ctx
            .app
            .hook(
                crate::hooks::TASK_SPAWN_BEFORE,
                &hook_context(ctx, "task"),
                payload,
                &ctx.cancel,
                Some(Box::new(&mut validator)),
            )
            .await?;
        for key in ["prompt", "description", "subagent_type"] {
            args[key] = result[key].clone();
        }
    }
    Ok((args, plan))
}
struct Guard<A: Host> {
    app: A,
    engine: A::Engine,
    id: String,
    armed: bool,
}
impl<A: Host> Drop for Guard<A> {
    fn drop(&mut self) {
        if self.armed {
            self.app.abort(&self.engine, &self.id);
        }
    }
}
pub async fn run<A: Host>(ctx: &ToolCtx<A>, plan: &Plan) -> Result<ToolOutput, OperationError> {
    let input = &plan.input;
    let child = if let Some(id) = &input.task_id {
        let c = ctx.app.session(id)?.ok_or("Task session not found")?;
        if c.parent_id.as_deref() != Some(ctx.session_id.as_str()) {
            return Err("task_id must identify a child of the current session".into());
        }
        c
    } else {
        ctx.app
            .create_child(&plan.parent, &input.description)
            .await?
    };
    ctx.update_metadata(
        json!({"sessionId":child.id,"model":plan.model,"agent":input.subagent_type}),
    )?;
    let mut guard = Guard {
        app: ctx.app.clone(),
        engine: ctx.engine.clone(),
        id: child.id.clone(),
        armed: true,
    };
    let request = TaskRequest {
        model: plan.model.clone(),
        prompt: input.prompt.clone(),
        agent: input.subagent_type.clone(),
        variant: input
            .model
            .as_deref()
            .and_then(|s| s.split_once('#'))
            .map(|(_, v)| v.into()),
    };
    let done = tokio::select! {r=ctx.app.admit(&ctx.engine,&child.id,request)=>r?,_=ctx.cancel.cancelled()=>return Err(failure("Subagent cancelled"))};
    let mut context = hook_context(ctx, "task");
    context["childSessionID"] = json!(child.id);
    ctx.app.emit("task.started",&context,json!({"parentSessionID":ctx.session_id,"childSessionID":child.id,"agent":input.subagent_type,"model":plan.model}));
    if input.background {
        guard.armed = false;
        let (app, engine, parent, parent_message, child_id, description) = (
            ctx.app.clone(),
            ctx.engine.clone(),
            ctx.session_id.clone(),
            ctx.message_id.clone(),
            child.id.clone(),
            input.description.clone(),
        );
        tokio::spawn(async move {
            if let Ok(result) = done.await {
                app.deliver(
                    engine,
                    parent,
                    parent_message,
                    child_id,
                    description,
                    result,
                )
                .await;
            }
        });
        return Ok(ToolOutput{output:format!("Task session: {}\n\nThe task is working in the background. You will be notified automatically when it finishes.\nDO NOT sleep, poll for progress, ask the task for status, or duplicate this task's work — avoid working with the same files or topics it is using.\nWork on non-overlapping tasks, or briefly tell the user what you launched and end your response.",child.id),title:input.description.clone(),metadata:json!({"sessionId":child.id,"model":plan.model,"agent":input.subagent_type,"background":true})});
    }
    let result = tokio::select! {r=done=>r.map_err(|_|failure("Subagent ended without a result")).and_then(|r|r),_=ctx.cancel.cancelled()=>Err(failure("Subagent cancelled"))};
    let status = if result.is_ok() {
        "completed"
    } else if ctx.cancel.is_cancelled() {
        "cancelled"
    } else {
        "failed"
    };
    ctx.app.emit("task.ended",&context,json!({"parentSessionID":ctx.session_id,"childSessionID":child.id,"status":status,"messageID":result.as_ref().ok().map(|r:&TaskResult|&r.message_id)}));
    let result = result?;
    guard.armed = false;
    Ok(ToolOutput {
        output: format!("Task session: {}\n\n{}", child.id, result.text),
        title: input.description.clone(),
        metadata: json!({"sessionId":child.id,"model":plan.model,"agent":input.subagent_type}),
    })
}
