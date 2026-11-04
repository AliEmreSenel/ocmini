use super::*;
use crate::events as event;
use tokio_util::sync::CancellationToken;

/// Tool calls admitted in one model step are independent. Argument hooks and
/// doom-loop signatures run in part order, then preparation and execution run
/// concurrently. A same-file edit can wait for its predecessor's transaction
/// without blocking tools on unrelated paths. Results retain provider order.
pub(super) async fn run_tools(
    base: &ToolCtx,
    cfg: &Config,
    request: &PromptRequest,
    allowed: &HashSet<String>,
    tool_parts: &mut [Value],
    recent_calls: &mut VecDeque<String>,
) -> Result<(), String> {
    let agent = request.agent.as_deref().unwrap_or("build");
    let mut running = Vec::new();
    for (index, part) in tool_parts.iter_mut().enumerate() {
        if part["state"]["status"] == "error" {
            continue;
        }
        let name = part["tool"].as_str().unwrap_or_default().to_string();
        let original_input = part["state"]["input"].clone();
        let start = now_ms();
        let mut ctx = base.clone();
        ctx.call_id = part["callID"].as_str().unwrap_or_default().into();
        ctx.part_id = part["id"].as_str().unwrap_or_default().into();
        let prepared =
            prepare_effective_args(&ctx, &name, original_input.clone(), cfg, request).await;
        let visible_input = match &prepared {
            Ok(input) => input.clone(),
            Err(_) => original_input.clone(),
        };
        let signature = format!("{name}:{visible_input}");
        recent_calls.push_back(signature.clone());
        if recent_calls.len() > 3 {
            recent_calls.pop_front();
        }
        let repeated = recent_calls.len() == 3 && recent_calls.iter().all(|s| s == &signature);
        running.push(execute_part(
            ctx,
            index,
            name,
            prepared,
            visible_input,
            part.clone(),
            start,
            repeated,
            agent,
            cfg,
            request,
            allowed,
        ));
    }
    let mut first_error = None;
    for (index, part, error) in futures::future::join_all(running).await {
        tool_parts[index] = part;
        if let Some(error) = error.filter(|_| first_error.is_none()) {
            first_error = Some(error);
        }
    }
    match first_error {
        None => Ok(()),
        Some(error) => Err(error),
    }
}

/// Run one prepared tool call to its terminal part state. The part is owned
/// while it runs, so concurrent calls never share a mutable part slot.
#[allow(clippy::too_many_arguments)]
async fn execute_part(
    ctx: ToolCtx,
    index: usize,
    name: String,
    prepared: Result<Value, OperationError>,
    mut visible_input: Value,
    mut part: Value,
    start: i64,
    repeated: bool,
    agent: &str,
    cfg: &Config,
    request: &PromptRequest,
    allowed: &HashSet<String>,
) -> (usize, Value, Option<String>) {
    let prepared = match prepared {
        Ok(_)
            if !allowed.contains(&name) && !allowed.contains(crate::v2::wire::tool_name(&name)) =>
        {
            Err(OperationError::from(format!(
                "Tool '{name}' is not enabled for agent '{agent}'"
            )))
        }
        Ok(args) => prepare_tool_input(&ctx, &name, args, cfg, request).await,
        Err(error) => Err(error),
    };
    if let Ok(input) = &prepared {
        visible_input = input.args().clone();
    }
    // File hooks run under the edit transaction lock. Persist the complete
    // effective input once, before permission checks and effects.
    if visible_input != part["state"]["input"] {
        part["ocminiOriginalInput"] = part["state"]["input"].take();
    }
    part["state"] = json!({"status":"running","input":&visible_input,"time":{"start":start}});
    if let Err(error) = crate::messages::update_part(&ctx.app, &part) {
        return (index, part, Some(error.message));
    }
    let output = match &prepared {
        Err(error) => Err(error.clone()),
        Ok(input) => execute_tool(&ctx, &name, input, cfg, request, repeated).await,
    };
    // Task/question tools may publish metadata while blocked. Preserve it.
    let existing = ctx
        .app
        .store
        .message_part(&ctx.session_id, &ctx.message_id, &ctx.part_id)
        .ok()
        .flatten();
    let running_metadata = existing
        .as_ref()
        .map(|p| p["state"]["metadata"].clone())
        .unwrap_or_else(|| json!({}));
    part["state"] = match output {
        Ok(out) => {
            json!({"status":"completed","input":visible_input,"output":out.output,"title":out.title,"metadata":out.metadata,"time":{"start":start,"end":now_ms()}})
        }
        Err(error) => {
            json!({"status":"error","input":visible_input,"error":error.message,"metadata":running_metadata,"time":{"start":start,"end":now_ms()}})
        }
    };
    let update = crate::messages::update_part(&ctx.app, &part);
    let completed = part["state"]["status"] == "completed";
    let error_value = part["state"]["error"].clone();
    let event = if completed {
        event::Tool::Completed {
            call_id: &ctx.call_id,
            message_id: &ctx.message_id,
            part_id: &ctx.part_id,
            error: &error_value,
        }
    } else {
        event::Tool::Failed {
            call_id: &ctx.call_id,
            message_id: &ctx.message_id,
            part_id: &ctx.part_id,
            error: &error_value,
        }
    };
    ctx.app.hooks.emit(event, &tools::hook_context(&ctx, &name));
    let failure = match update {
        Ok(()) => None,
        Err(error) => Some(error.message),
    };
    (index, part, failure)
}

pub(super) fn execution_tool(name: &str) -> bool {
    tools::Builtin::from_name(name).is_some_and(tools::Builtin::is_execution)
        || name.starts_with("mcp_")
}
pub(super) fn unattended_tool_denied(name: &str, app: &App, request: &PromptRequest) -> bool {
    request.deny_unattended_tools
        && (execution_tool(name) || name == "webfetch" || app.mcp().route(name).is_some())
}

pub(super) fn tool_enabled(name: &str, agent: &str, cfg: &Config, request: &PromptRequest) -> bool {
    let legacy = match name {
        "shell" => "bash",
        "subagent" => "task",
        other => other,
    };
    [name, legacy, crate::v2::wire::tool_name(name)]
        .into_iter()
        .all(|alias| {
            request.tools.as_ref().and_then(|t| t.get(alias)) != Some(&false)
                && cfg
                    .agent
                    .get(agent)
                    .and_then(|a| a["tools"].get(alias))
                    .and_then(Value::as_bool)
                    != Some(false)
        })
}

fn permission_rules(
    cfg: &Config,
    session: &Session,
    request: &PromptRequest,
) -> Vec<PermissionRule> {
    let mut rules = cfg.agent_permission_rules(request.agent.as_deref().unwrap_or("build"));
    rules.extend(session.permission.clone().unwrap_or_default());
    rules
}

pub(super) async fn prepare_effective_input(
    ctx: &ToolCtx,
    name: &str,
    input: Value,
    cfg: &Config,
    request: &PromptRequest,
) -> Result<PreparedTool, OperationError> {
    let input = prepare_effective_args(ctx, name, input, cfg, request).await?;
    prepare_tool_input(ctx, name, input, cfg, request).await
}

async fn prepare_tool_input(
    ctx: &ToolCtx,
    name: &str,
    args: Value,
    cfg: &Config,
    request: &PromptRequest,
) -> Result<PreparedTool, OperationError> {
    use tools::Builtin;
    let builtin = Builtin::from_name(name);
    if !matches!(
        builtin,
        Some(Builtin::Write | Builtin::Edit | Builtin::ApplyPatch)
    ) {
        return tools::prepare_input(ctx, name, args, |_, _| async { Ok(()) }).await;
    }
    let root = tools::canonical_target(Path::new(&ctx.dir))?;
    let approval_args = args.clone();
    let (root, approval_args) = (&root, &approval_args);
    tools::prepare_input(ctx, name, args, move |path, canonical| async move {
        // An earlier edit may have saved an approval while this one waited.
        let rules = current_permission_rules(ctx, cfg, request)?;
        if !canonical.starts_with(root) {
            check_permission(
                ctx,
                &rules,
                "external_directory",
                &canonical.to_string_lossy(),
                name,
                approval_args,
            )
            .await?;
        }
        for path in std::iter::once(&path).chain((canonical != path).then_some(&canonical)) {
            let pattern = path.to_string_lossy();
            check_permission(ctx, &rules, "edit", &pattern, name, approval_args).await?;
            // Edits and patches can surface existing content in diagnostics or
            // transformed tool inputs. Apply the read policy before loading it.
            if builtin != Some(Builtin::Write) {
                check_permission(ctx, &rules, "read", &pattern, name, approval_args).await?;
            }
        }
        ctx.app.store.check_ownership(&ctx.session_id)?;
        Ok(())
    })
    .await
}

fn current_permission_rules(
    ctx: &ToolCtx,
    cfg: &Config,
    request: &PromptRequest,
) -> Result<Vec<PermissionRule>, OperationError> {
    ctx.app.store.check_ownership(&ctx.session_id)?;
    let session = ctx
        .app
        .store
        .get_session(&ctx.session_id)?
        .ok_or_else(|| OperationError::from("Session not found"))?;
    Ok(permission_rules(cfg, &session, request))
}

async fn prepare_effective_args(
    ctx: &ToolCtx,
    name: &str,
    mut input: Value,
    cfg: &Config,
    request: &PromptRequest,
) -> Result<Value, OperationError> {
    let agent = request.agent.as_deref().unwrap_or("build");
    if !input.is_object() {
        return Err("Tool input must be an object".into());
    }
    if unattended_tool_denied(name, &ctx.app, request) {
        return Err(format!("Tool '{name}' requires --auto in a non-interactive run").into());
    }
    if !tool_enabled(name, agent, cfg, request)
        || (["plan", "explore"].contains(&agent)
            && (execution_tool(name) || ctx.app.mcp().route(name).is_some()))
    {
        return Err(format!("Tool '{name}' is disabled for this request").into());
    }
    if ctx.app.hooks.matches(
        crate::tools::hooks::TOOL_EXECUTE_BEFORE,
        &tools::hook_context(ctx, name),
    ) {
        let result = ctx
            .app
            .hooks
            .run(
                crate::tools::hooks::TOOL_EXECUTE_BEFORE,
                &tools::hook_context(ctx, name),
                json!({"args":input}),
                Some(&ctx.cancel),
            )
            .await?;
        input = result.input["args"].clone();
    }
    Ok(input)
}

pub(super) async fn execute_tool(
    ctx: &ToolCtx,
    name: &str,
    input: &PreparedTool,
    cfg: &Config,
    request: &PromptRequest,
    repeated: bool,
) -> Result<ToolOutput, OperationError> {
    let result = execute_authorized_tool(ctx, name, input, cfg, request, repeated).await;
    let mut context = tools::hook_context(ctx, name);
    context["outcome"] = json!(if result.is_ok() { "completed" } else { "error" });
    if !ctx
        .app
        .hooks
        .matches(crate::tools::hooks::TOOL_EXECUTE_AFTER, &context)
    {
        return result;
    }
    match result {
        Ok(out) => {
            let protected = out.metadata.clone();
            let transformed=ctx.app.hooks.run(crate::tools::hooks::TOOL_EXECUTE_AFTER,&context,json!({"status":"completed","args":input.args(),"output":out.output,"title":out.title,"metadata":out.metadata}),Some(&ctx.cancel)).await;
            let convert = async {
                let result = transformed?;
                let output_changed = result.input["output"].as_str() != Some(out.output.as_str());
                let out: ToolOutput = serde_json::from_value(result.input)
                    .map_err(|error| OperationError::from(error.to_string()))?;
                if output_changed {
                    tools::limit_output(out).await
                } else {
                    Ok(out)
                }
            }
            .await;
            match convert {
                Ok(out) => Ok(out),
                Err(error) => {
                    let mut metadata = protected;
                    if !metadata.is_object() {
                        metadata = json!({});
                    }
                    metadata["executionOccurred"] = json!(true);
                    metadata["resultHookError"] = json!(error.message);
                    let _ = ctx.update_metadata(metadata);
                    Err(OperationError::from(format!("Tool effects already completed; result processing failed: {}. Do not repeat the operation solely to recover its output.",error.message)))
                }
            }
        }
        Err(error) => {
            let _ = ctx
                .app
                .hooks
                .run(
                    crate::tools::hooks::TOOL_EXECUTE_AFTER,
                    &context,
                    json!({"status":"error","args":input.args(),"error":error.message}),
                    Some(&ctx.cancel),
                )
                .await;
            Err(error)
        }
    }
}

async fn execute_authorized_tool(
    ctx: &ToolCtx,
    name: &str,
    input: &PreparedTool,
    cfg: &Config,
    request: &PromptRequest,
    repeated: bool,
) -> Result<ToolOutput, OperationError> {
    if repeated || input.file_paths().is_none() {
        let rules = current_permission_rules(ctx, cfg, request)?;
        if repeated {
            check_permission(ctx, &rules, "doom_loop", name, name, input.args()).await?;
        }
        if input.file_paths().is_none() {
            let paths = input.paths(&ctx.dir);
            let mut permission_paths = paths.clone();
            let root = tools::canonical_target(Path::new(&ctx.dir))?;
            for path in &paths {
                let canonical = tools::canonical_target(path)?;
                if !permission_paths.contains(&canonical) {
                    permission_paths.push(canonical.clone());
                }
                if !canonical.starts_with(&root) {
                    check_permission(
                        ctx,
                        &rules,
                        "external_directory",
                        &canonical.to_string_lossy(),
                        name,
                        input.args(),
                    )
                    .await?;
                }
            }
            let (permission, patterns) = input.permission(name, &permission_paths);
            for pattern in patterns {
                check_permission(ctx, &rules, permission, &pattern, name, input.args()).await?;
            }
        }
    }
    // Approval hooks and user input can outlive the lease that authorized them.
    ctx.app.store.check_ownership(&ctx.session_id)?;
    ctx.app.hooks.emit(
        event::Tool::Started {
            call_id: &ctx.call_id,
            message_id: &ctx.message_id,
            part_id: &ctx.part_id,
            name,
        },
        &tools::hook_context(ctx, name),
    );
    tools::run_tool(ctx, name, input).await
}

async fn check_permission(
    ctx: &ToolCtx,
    rules: &[PermissionRule],
    permission: &str,
    pattern: &str,
    tool: &str,
    input: &Value,
) -> Result<(), OperationError> {
    let denied = || OperationError::from(format!("Permission '{permission}' denied for {pattern}"));
    let original = crate::permission::decide(rules, permission, pattern);
    let mut decision = original;
    let mut context = ctx.hook_context.clone();
    context["tool"] = json!(tool);
    context["messageID"] = json!(ctx.message_id);
    context["callID"] = json!(ctx.call_id);
    context["requestKind"] = json!("primary");
    if ctx.app.hooks.has(crate::permission::PERMISSION_DECIDE) {
        let result = ctx
            .app
            .hooks
            .run(
                crate::permission::PERMISSION_DECIDE,
                &context,
                json!({"permission":permission,"pattern":pattern,"decision":decision}),
                Some(&ctx.cancel),
            )
            .await?;
        decision = serde_json::from_value(result.input["decision"].clone())
            .map_err(|e| OperationError::from(format!("Invalid permission decision: {e}")))?;
    }
    ctx.app.hooks.emit(
        event::Permission::Decided {
            permission,
            pattern,
            decision,
        },
        &context,
    );
    match decision {
        PermissionAction::Allow => return Ok(()),
        PermissionAction::Deny => return Err(denied()),
        PermissionAction::Ask => {}
    }
    let (req, rx) =
        ctx.app
            .perms
            .ask_for_tool(tools::hook_context(ctx, tool), permission, pattern, input);
    ctx.app.hooks.emit(
        event::Permission::Asked { request: &req },
        &ctx.app.session_context(&ctx.session_id, json!({})),
    );
    let rejected = || crate::permission::Response {
        decision: "reject".into(),
        message: None,
    };
    let reply = tokio::select! { reply=rx=>reply.unwrap_or_else(|_|rejected()), _=ctx.cancel.cancelled()=>rejected() };
    if reply.decision == "reject" {
        ctx.app.perms.reply(&req.id, "reject");
        return Err(reply
            .message
            .map(|message| OperationError::from(format!("{}: {message}", denied().message)))
            .unwrap_or_else(denied));
    }
    if reply.decision == "always" {
        let rule = PermissionRule {
            permission: permission.into(),
            pattern: pattern.into(),
            action: crate::types::PermissionAction::Allow,
        };
        ctx.app.store.mutate_session(&ctx.session_id, |session| {
            session.permission.get_or_insert_with(Vec::new).push(rule);
            Ok(true)
        })?;
    }
    Ok(())
}

pub(super) async fn expand_shell_templates(
    app: &App,
    engine: Arc<dyn EngineApi>,
    session: &Session,
    request: &PromptRequest,
    cfg: &Config,
    cancel: &CancellationToken,
) -> Result<(), String> {
    let mid = request
        .message_id
        .as_deref()
        .ok_or("Command prompt has no messageID")?;
    let mut user = app
        .store
        .message(&session.id, mid)?
        .ok_or("Command prompt not found")?;
    static SHELL_TEMPLATE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re =
        SHELL_TEMPLATE.get_or_init(|| regex::Regex::new(r"!`([^`]+)`").expect("constant regex"));
    for part in &mut user.parts {
        if part["type"] != "text" {
            continue;
        }
        let original = part["text"].as_str().unwrap_or_default().to_string();
        let mut replaced = String::new();
        let mut offset = 0;
        for capture in re.captures_iter(&original) {
            let matched = capture.get(0).ok_or("Invalid shell template")?;
            let command = capture.get(1).ok_or("Empty shell template")?.as_str();
            if ["plan", "explore"].contains(&request.agent.as_deref().unwrap_or("build")) {
                return Err("Shell interpolation is disabled in read-only agents".into());
            }
            let ctx = ToolCtx {
                hook_context: {
                    let mut c = turn_context(app, &session.id, request);
                    c["requestKind"] = json!("primary");
                    c
                },
                dir: session.directory.clone(),
                app: app.clone(),
                session_id: session.id.clone(),
                engine: engine.clone(),
                cancel: cancel.clone(),
                message_id: mid.into(),
                call_id: hex_id("call_"),
                part_id: part["id"].as_str().unwrap_or_default().into(),
            };
            let input = json!({"command":command,"timeout":120_000});
            let input = prepare_effective_input(&ctx, "bash", input, cfg, request)
                .await
                .map_err(|e| e.message)?;
            let output = execute_tool(&ctx, "bash", &input, cfg, request, false)
                .await
                .map_err(|e| e.message)?;
            replaced.push_str(&original[offset..matched.start()]);
            replaced.push_str(&output.output);
            offset = matched.end();
        }
        replaced.push_str(&original[offset..]);
        part["text"] = json!(replaced);
    }
    crate::messages::update_parts(app, &user)?;
    Ok(())
}
