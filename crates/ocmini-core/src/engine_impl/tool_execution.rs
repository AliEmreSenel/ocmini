use super::*;
use crate::events as event;
use tokio_util::sync::CancellationToken;

pub(super) type DeferredMcpTool = (usize, Value, crate::mcp::McpToolRoute);
type McpActivation<'a> = (&'a [DeferredMcpTool], &'a mut Vec<String>);

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
    deferred: &[DeferredMcpTool],
) -> Result<Vec<String>, String> {
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
        let stale = deferred
            .iter()
            .find(|(_, def, _)| def["function"]["name"] == name)
            .is_some_and(|(_, _, route)| !base.app.mcp().route_is_current(&name, route));
        let prepared = if stale {
            Err(OperationError::from(
                "MCP tool catalog changed. Run discover_mcp_tools again.",
            ))
        } else {
            prepare_effective_args(&ctx, &name, original_input.clone(), cfg, request).await
        };
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
            deferred,
        ));
    }
    let mut first_error = None;
    let mut activated = Vec::new();
    for (index, part, error, selected) in futures::future::join_all(running).await {
        tool_parts[index] = part;
        activated.extend(selected);
        if let Some(error) = error.filter(|_| first_error.is_none()) {
            first_error = Some(error);
        }
    }
    if first_error.is_none()
        && tool_parts.iter().any(|part| {
            part["tool"] == "read"
                && part["state"]["status"] == "completed"
                && part["state"]["input"]["filePath"].as_str().is_some()
        })
    {
        discover_read_instructions(base, cfg, request, tool_parts).await?;
    }
    match first_error {
        None => Ok(activated),
        Some(error) => Err(error),
    }
}

async fn discover_read_instructions(
    base: &ToolCtx,
    cfg: &Config,
    request: &PromptRequest,
    parts: &[Value],
) -> Result<(), String> {
    // Resolve paths in the effect domain, including when a sandbox is active
    // or the workspace directory is accessed through a symlink.
    let root = tools::resolve_path(&base.dir, ".");
    let reads: Vec<_> = parts
        .iter()
        .filter_map(|part| {
            if part["tool"] != "read" || part["state"]["status"] != "completed" {
                return None;
            }
            let path = tools::resolve_path(&base.dir, part["state"]["input"]["filePath"].as_str()?);
            if path == root {
                return None;
            }
            Some((part, path))
        })
        .collect();
    if reads.is_empty() || base.cancel.is_cancelled() {
        return Ok(());
    }
    let mut pending = Vec::new();
    for (part, path) in reads {
        if base.cancel.is_cancelled() {
            return Ok(());
        }
        // The successful read's input may refer to a directory; probing it is
        // delegated to the same effect domain as the read itself.
        let Ok(resolved) = tools::host::Host::effect(
            &base.app,
            base,
            "instruction_resolve_read",
            json!({"path":path}),
        )
        .await
        else {
            continue;
        };
        let Ok(resolved) = resolved.json() else {
            continue;
        };
        let Some(target) = resolved["target"].as_str() else {
            continue;
        };
        let Some(canonical_root) = resolved["root"].as_str() else {
            continue;
        };
        let root_instruction = resolved["rootInstruction"].as_str().map(PathBuf::from);
        let target = Path::new(target);
        let canonical_root = Path::new(canonical_root);
        if !target.starts_with(canonical_root) {
            continue;
        }
        let mut dir = if resolved["directory"] == true {
            Some(target)
        } else {
            target.parent()
        };
        let mut candidates = Vec::new();
        while let Some(current) = dir {
            if current == canonical_root || !current.starts_with(canonical_root) {
                break;
            }
            candidates.push(current.join("AGENTS.md"));
            dir = current.parent();
        }
        candidates.reverse();
        if !candidates.is_empty() {
            pending.push((
                part,
                target.to_path_buf(),
                canonical_root.to_path_buf(),
                root_instruction,
                candidates,
            ));
        }
    }
    if pending.is_empty() || base.cancel.is_cancelled() {
        return Ok(());
    }
    let store = base.app.store.clone();
    let sid = base.session_id.clone();
    let mid = base.message_id.clone();
    let history = tokio::task::spawn_blocking(move || store.context_messages(&sid, &mid))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.message)?;
    let start = history
        .iter()
        .rposition(|m| {
            m.info.summary == Some(true)
                && m.info.time.completed.is_some()
                && m.info.error.is_none()
                && m.parts.iter().any(|p| {
                    p["type"] == "text" && p["text"].as_str().is_some_and(|s| !s.trim().is_empty())
                })
        })
        .map_or(0, |i| i + 1);
    let mut seen = HashSet::new();
    for message in history.into_iter().skip(start) {
        for part in message.parts {
            if let Some(path) = part["ocminiInstructionPath"].as_str() {
                seen.insert(path.to_owned());
            }
            // Recognize provenance written by the previous implementation until it is pruned.
            if !part["state"]["time"]["compacted"].is_number() {
                if let Some(paths) = part["state"]["metadata"]["instructionPaths"].as_array() {
                    seen.extend(paths.iter().filter_map(Value::as_str).map(str::to_owned));
                }
            }
        }
    }
    for (part, target, canonical_root, root_instruction, candidates) in pending {
        if base.cancel.is_cancelled() {
            return Ok(());
        }
        let mut ctx = base.clone();
        ctx.call_id = part["callID"].as_str().unwrap_or_default().into();
        ctx.part_id = part["id"].as_str().unwrap_or_default().into();
        let root_agents = canonical_root.join("AGENTS.md");
        for candidate in candidates {
            if ctx.cancel.is_cancelled() {
                return Ok(());
            }
            if seen.contains(candidate.to_string_lossy().as_ref()) {
                continue;
            }
            let Ok(result) = tools::host::Host::effect(
                &ctx.app,
                &ctx,
                "instruction_resolve",
                json!({"path":candidate}),
            )
            .await
            else {
                continue;
            };
            let Ok(value) = result.json() else {
                continue;
            };
            let Some(canonical) = value.as_str() else {
                continue;
            };
            let canonical = Path::new(canonical);
            if !canonical.starts_with(&canonical_root)
                || canonical == target.as_path()
                || canonical == root_agents.as_path()
                || root_instruction
                    .as_deref()
                    .is_some_and(|root_file| canonical == root_file)
            {
                continue;
            }
            let key = canonical.to_string_lossy().into_owned();
            if seen.contains(&key) {
                continue;
            }
            let rules = current_permission_rules(&ctx, cfg, request).map_err(|e| e.message)?;
            let candidate_str = candidate.to_string_lossy();
            let permission_input = json!({"filePath": candidate_str});
            if check_permission(
                &ctx,
                &rules,
                "read",
                &candidate_str,
                "read",
                &permission_input,
            )
            .await
            .is_err()
            {
                if ctx.cancel.is_cancelled() {
                    return Ok(());
                }
                continue;
            }
            if canonical != candidate
                && check_permission(&ctx, &rules, "read", &key, "read", &permission_input)
                    .await
                    .is_err()
            {
                if ctx.cancel.is_cancelled() {
                    return Ok(());
                }
                continue;
            }
            ctx.app
                .store
                .check_ownership(&ctx.session_id)
                .map_err(|e| e.message)?;
            let Ok(result) = tools::host::Host::effect(
                &ctx.app,
                &ctx,
                "instruction",
                json!({"path":candidate, "expected":canonical, "display":candidate_str}),
            )
            .await
            else {
                continue;
            };
            let Ok(value) = result.json() else {
                continue;
            };
            let Some(content) = value.as_str() else {
                continue;
            };
            // Store under a dedicated internal part type so v2/TUI transcript views
            // never render injected AGENTS.md content as visible assistant text.
            let context = json!({
                "id": hex_id("prt_"),
                "sessionID": base.session_id,
                "messageID": base.message_id,
                "type": "ocmini_instruction",
                "synthetic": true,
                "ocminiInstructionPath": key,
                "text": content,
            });
            crate::messages::update_part(&base.app, &context).map_err(|e| e.message)?;
            seen.insert(key);
        }
    }
    Ok(())
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
    deferred: &[DeferredMcpTool],
) -> (usize, Value, Option<String>, Vec<String>) {
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
        return (index, part, Some(error.message), Vec::new());
    }
    let mut selected = Vec::new();
    let output: Result<_, OperationError> = match &prepared {
        Err(error) => Err(error.clone()),
        Ok(input) => {
            execute_tool(
                &ctx,
                &name,
                input,
                cfg,
                request,
                repeated,
                Some((deferred, &mut selected)),
            )
            .await
        }
    };
    part["state"] = match output {
        Ok(out) => {
            json!({"status":"completed","input":visible_input,"output":out.output,"title":out.title,"metadata":out.metadata,"time":{"start":start,"end":now_ms()}})
        }
        Err(error) => {
            // Task/question tools may publish metadata while blocked. Preserve it.
            let running_metadata = ctx
                .app
                .store
                .message_part(&ctx.session_id, &ctx.message_id, &ctx.part_id)
                .ok()
                .flatten()
                .map(|p| p["state"]["metadata"].clone())
                .unwrap_or_else(|| json!({}));
            json!({"status":"error","input":visible_input,"error":error.message,"metadata":running_metadata,"time":{"start":start,"end":now_ms()}})
        }
    };
    let update = crate::messages::update_part(&ctx.app, &part);
    let completed = part["state"]["status"] == "completed";
    if !completed {
        selected.clear();
    }
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
    (index, part, failure, selected)
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
    if name == "background_read" && !tool_enabled("bash", agent, cfg, request) {
        return false;
    }
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
        return tools::prepare_input(ctx, name, args, |_, _| async { Ok(()) })
            .await
            .map_err(Into::into);
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
    .map_err(Into::into)
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
    selection: Option<McpActivation<'_>>,
) -> Result<ToolOutput, OperationError> {
    let result = execute_authorized_tool(ctx, name, input, cfg, request, repeated, selection).await;
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
                    tools::limit_output(out, ctx.app.tool_output_directory(&ctx.session_id)).await
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
    selection: Option<McpActivation<'_>>,
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
    let route = selection.as_ref().and_then(|(deferred, _)| {
        deferred
            .iter()
            .find(|(_, def, _)| def["function"]["name"] == name)
            .map(|(_, _, route)| route)
    });
    if route.is_some_and(|route| !ctx.app.mcp().route_is_current(name, route)) {
        return Err("MCP tool catalog changed. Run discover_mcp_tools again.".into());
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
    if name == "discover_mcp_tools" {
        let (deferred, selected) = selection.ok_or("Discovery unavailable")?;
        let query = input.args()["query"]
            .as_str()
            .ok_or("query must be a string")?
            .to_lowercase();
        for (_, def, route) in deferred {
            let name = def["function"]["name"].as_str().unwrap_or_default();
            let description = def["function"]["description"].as_str().unwrap_or_default();
            if ctx.app.mcp().route_is_current(name, route)
                && format!("{name} {description}")
                    .to_lowercase()
                    .contains(&query)
            {
                selected.push(name.to_owned());
            }
        }
        return tools::limit_output(
            ToolOutput {
                output: format!("Activated MCP tools: {}", selected.join(", ")),
                title: "Discover MCP tools".into(),
                metadata: json!({"activated":selected}),
            },
            ctx.app.tool_output_directory(&ctx.session_id),
        )
        .await
        .map_err(Into::into);
    }
    if let Some(route) = route {
        let output = ctx
            .app
            .mcp()
            .call_discovered(route, input.args().clone(), ctx.cancel.clone())
            .await?;
        return ocmini_tools::output::limit(output, ctx.app.tool_output_directory(&ctx.session_id))
            .await
            .map_err(Into::into);
    }
    tools::run_tool(ctx, name, input, |next, hop_args| async move {
        let rules = current_permission_rules(ctx, cfg, request)?;
        check_permission(ctx, &rules, "webfetch", &next, "webfetch", &hop_args).await?;
        ctx.app.store.check_ownership(&ctx.session_id)?;
        Ok(())
    })
    .await
    .map_err(Into::into)
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
    let mut changed = false;
    for part in &mut user.parts {
        if part["type"] != "text" {
            continue;
        }
        let Some(original) = part["text"].as_str() else {
            continue;
        };
        if !original.contains("!`") {
            continue;
        }
        let original = original.to_owned();
        let mut replaced = String::new();
        let mut offset = 0;
        let mut part_changed = false;
        for capture in re.captures_iter(&original) {
            part_changed = true;
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
            let output = execute_tool(&ctx, "bash", &input, cfg, request, false, None)
                .await
                .map_err(|e| e.message)?;
            replaced.push_str(&original[offset..matched.start()]);
            replaced.push_str(&output.output);
            offset = matched.end();
        }
        if part_changed {
            replaced.push_str(&original[offset..]);
            part["text"] = json!(replaced);
            changed = true;
        }
    }
    if changed {
        crate::messages::update_parts(app, &user)?;
    }
    Ok(())
}
