use super::*;
use crate::events as event;
use tokio_util::sync::CancellationToken;

pub(super) async fn run_tools(
    base: &ToolCtx,
    cfg: &Config,
    request: &PromptRequest,
    allowed: &HashSet<String>,
    tool_parts: &mut [Value],
    recent_calls: &mut VecDeque<String>,
) -> Result<(), String> {
    let app = &base.app;
    let sid = base.session_id.as_str();
    let amid = base.message_id.as_str();
    let cancel = &base.cancel;
    let agent = request.agent.as_deref().unwrap_or("build");
    for part in tool_parts {
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
            prepare_effective_input(&ctx, &name, original_input.clone(), cfg, request).await;
        let visible_input = prepared
            .as_ref()
            .map_or(&original_input, |input| input.args());
        if visible_input != &part["state"]["input"] {
            part["ocminiOriginalInput"] = part["state"]["input"].take();
        }
        part["state"] = json!({"status":"running","input":visible_input,"time":{"start":start}});
        crate::messages::update_part(app, part)?;
        let signature = format!("{name}:{}", visible_input);
        recent_calls.push_back(signature.clone());
        if recent_calls.len() > 3 {
            recent_calls.pop_front();
        }
        let repeated = recent_calls.len() == 3 && recent_calls.iter().all(|s| s == &signature);
        let output = match &prepared {
            Err(error) => Err(error.clone()),
            Ok(_) if !allowed.contains(&name) => Err(OperationError::from(format!(
                "Tool '{name}' is not enabled for agent '{agent}'"
            ))),
            Ok(input) => execute_tool(&ctx, &name, input, cfg, request, repeated).await,
        };
        // Task/question tools may publish metadata while blocked. Preserve it.
        let existing = app
            .store
            .message(sid, amid)
            .ok()
            .flatten()
            .and_then(|m| m.parts.into_iter().find(|p| p["id"] == part["id"]));
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
        crate::messages::update_part(app, part)?;
        let event = if part["state"]["status"] == "completed" {
            event::Tool::Completed {
                call_id: &ctx.call_id,
                message_id: amid,
                part_id: &ctx.part_id,
                error: &part["state"]["error"],
            }
        } else {
            event::Tool::Failed {
                call_id: &ctx.call_id,
                message_id: amid,
                part_id: &ctx.part_id,
                error: &part["state"]["error"],
            }
        };
        app.hooks.emit(event, &tools::hook_context(&ctx, &name));

        if cancel.is_cancelled() {
            break;
        }
    }
    Ok(())
}

pub(super) fn execution_tool(name: &str) -> bool {
    tools::Builtin::from_name(name).is_some_and(tools::Builtin::is_execution)
        || name.starts_with("mcp_")
}
pub(super) fn tool_enabled(name: &str, agent: &str, cfg: &Config, request: &PromptRequest) -> bool {
    if request.tools.as_ref().and_then(|t| t.get(name)) == Some(&false) {
        return false;
    }
    cfg.agent
        .get(agent)
        .and_then(|a| a["tools"].get(name))
        .and_then(Value::as_bool)
        != Some(false)
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
    mut input: Value,
    cfg: &Config,
    request: &PromptRequest,
) -> Result<PreparedTool, OperationError> {
    let agent = request.agent.as_deref().unwrap_or("build");
    if !input.is_object() {
        return Err("Tool input must be an object".into());
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
    tools::prepare_input(ctx, name, input).await
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
    ctx.app.store.check_ownership(&ctx.session_id)?;
    let session = ctx
        .app
        .store
        .get_session(&ctx.session_id)?
        .ok_or_else(|| OperationError::from("Session not found"))?;
    let rules = permission_rules(cfg, &session, request);
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
    if repeated {
        check_permission(ctx, &rules, "doom_loop", name, name, input.args()).await?;
    }
    let (permission, patterns) = input.permission(name, &permission_paths);
    for pattern in patterns {
        check_permission(ctx, &rules, permission, &pattern, name, input.args()).await?;
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
    let reply = tokio::select! { reply=rx=>reply.unwrap_or_else(|_|"reject".into()), _=ctx.cancel.cancelled()=>"reject".into() };
    if reply == "reject" {
        ctx.app.perms.reply(&req.id, "reject");
        return Err(denied());
    }
    if reply == "always" {
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
