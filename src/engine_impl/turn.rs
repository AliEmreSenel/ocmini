use super::*;
use crate::events as event;
use tokio_util::sync::CancellationToken;

struct PreparedTurn {
    config: Config,
    adapter: Box<dyn provider::ProviderAdapter>,
    request: ChatRequest,
    chat: bool,
}
async fn prepare_turn(
    app: &App,
    engine: Arc<dyn EngineApi>,
    session: &Session,
    request: &PromptRequest,
    model: &ModelRef,
    hook_context: &Value,
    cancel: &CancellationToken,
) -> Result<PreparedTurn, OperationError> {
    let sid = session.id.as_str();
    let compact = request.command.as_deref() == Some("__ocmini_compact");
    let extra_context = if app.hooks.has(crate::engine_impl::hooks::TURN_START_BEFORE) {
        let result = app
            .hooks
            .run(
                crate::engine_impl::hooks::TURN_START_BEFORE,
                hook_context,
                json!({"context":""}),
                Some(cancel),
            )
            .await?;
        result.input["context"]
            .as_str()
            .ok_or("Turn context must be a string")?
            .to_string()
    } else {
        String::new()
    };
    let mut cfg = app.config.as_ref().clone();
    if cfg
        .provider(&model.provider_id)
        .is_none_or(|provider| provider.models.is_empty())
    {
        let (discovered, errors) = tokio::select! {
            result = provider::discover_models(&cfg, &app.auth) => result,
            _ = cancel.cancelled() => return Err("Aborted".into()),
        };
        for error in errors {
            eprintln!("ocmini: {error}");
        }
        cfg = discovered;
    }
    let agent = request.agent.as_deref().unwrap_or("build");

    let read_only = ["plan", "explore"].contains(&agent);
    tokio::select! {
        _ = app.wait_mcp_ready() => {},
        _ = cancel.cancelled() => return Err("Aborted".into()),
    }
    let mut system = tokio::select! {
        result=system_prompt(&cfg,&session.directory,agent,request.system.as_deref())=>result?,
        _=cancel.cancelled()=>return Err("Aborted".into()),
    };
    if !extra_context.is_empty() {
        system.push_str("\n\n");
        system.push_str(&extra_context);
    }
    if read_only {
        system.push_str("\n\nThis is a read-only analysis session. Use the supplied reading and search tools. Do not modify files or request execution tools.");
    }
    if request.command.is_some() && !compact {
        expand_shell_templates(app, engine.clone(), session, request, &cfg, cancel).await?;
    }
    let adapter = provider::build(
        &model.provider_id,
        cfg.provider(&model.provider_id),
        &app.auth,
    );
    // Chat adapters replay chain of thought once per turn, so history is
    // merged per turn there. Responses and Anthropic rebuild each step from
    // their own opaque payloads and need the per-step shape kept.
    let mut base = ChatRequest {
        model: model.model_id.clone(),
        system,
        variant: request.variant.clone(),
        session_id: Some(sid.into()),
        ..Default::default()
    };
    let chat = matches!(adapter.protocol_of(&base)?, provider::Protocol::Chat);
    if compact {
        return Ok(PreparedTurn {
            config: cfg,
            adapter,
            request: base,
            chat,
        });
    }
    let mut defs = tools::tool_defs();
    defs.extend(tokio::select! {
        value = app.mcp().tool_defs_async() => value,
        _ = cancel.cancelled() => return Err("Aborted".into()),
    });
    defs.retain(|def| {
        let name = def["function"]["name"].as_str().unwrap_or_default();
        tool_enabled(name, agent, &cfg, request)
            && !(read_only && (execution_tool(name) || app.mcp().route(name).is_some()))
    });
    if app.hooks.has(crate::tools::hooks::TOOL_CATALOG_TRANSFORM) {
        let result = app
            .hooks
            .run(
                crate::tools::hooks::TOOL_CATALOG_TRANSFORM,
                hook_context,
                json!({"tools":defs}),
                Some(cancel),
            )
            .await?;
        defs = result.input["tools"]
            .as_array()
            .ok_or("Tool catalog hook must return an array")?
            .clone();
    }
    base.tools = defs;
    Ok(PreparedTurn {
        config: cfg,
        adapter,
        request: base,
        chat,
    })
}

async fn end_feedback(
    app: &App,
    engine: &Arc<dyn EngineApi>,
    hook_context: &Value,
    response: &MessageWithParts,
    cancel: &CancellationToken,
) -> Result<Option<String>, OperationError> {
    let sid = hook_context["sessionID"]
        .as_str()
        .ok_or("Missing turn session")?;
    let amid = hook_context["messageID"]
        .as_str()
        .ok_or("Missing turn message")?;
    let end_continuations = hook_context["endAttempt"].as_u64().unwrap_or(1) - 1;
    app.hooks.emit(
        event::Turn::EndRequested {
            end_attempt: end_continuations + 1,
            message_id: amid,
        },
        hook_context,
    );
    if !app.hooks.has(crate::engine_impl::hooks::TURN_END_BEFORE) {
        return Ok(None);
    }
    let Some(gate_cancel) = engine.begin_end_gate(app, sid)? else {
        if cancel.is_cancelled() {
            return Err("Aborted".into());
        }
        return Ok(None);
    };
    let gate_guard = EndGateGuard {
        engine: engine.clone(),
        session_id: sid.to_owned(),
        token: gate_cancel.clone(),
    };
    let text = crate::types::text_parts(&response.parts)
        .collect::<Vec<_>>()
        .join("\n");
    let gate = app
        .hooks
        .run(
            crate::engine_impl::hooks::TURN_END_BEFORE,
            hook_context,
            json!({"reason":"completed","response":{"text":text,"messageID":amid}}),
            Some(&gate_cancel),
        )
        .await;
    drop(gate_guard);
    let feedback = match gate {
        Ok(value) => value.feedback,
        Err(ref error)
            if gate_cancel.is_cancelled()
                && !cancel.is_cancelled()
                && error.kind == crate::hooks::FailureKind::Cancelled
                && app.store.has_waiting(sid)? =>
        {
            None
        }
        Err(error) => return Err(error),
    };
    Ok(feedback)
}

pub(super) async fn run_turn(
    app: App,
    engine: Arc<dyn EngineApi>,
    sid: &str,
    request: PromptRequest,
    cancel: CancellationToken,
) -> TurnResult {
    let session = app.store.get_session(sid)?.ok_or("Session not found")?;
    let mut scoped = app.for_directory(&session.directory)?;
    scoped.hooks = app.hooks.clone();
    let app = scoped;
    let uid = request
        .message_id
        .as_deref()
        .ok_or("Admitted prompt has no messageID")?;
    let model = request
        .model
        .as_ref()
        .ok_or("Admitted prompt has no model")?;
    let mut hook_context = turn_context(&app, sid, &request);
    let compact = request.command.as_deref() == Some("__ocmini_compact");
    hook_context["requestKind"] = json!(if compact { "compaction" } else { "primary" });
    app.hooks
        .emit(event::Turn::Started { message_id: uid }, &hook_context);
    let PreparedTurn {
        config: cfg,
        adapter,
        request: base,
        chat,
    } = prepare_turn(
        &app,
        engine.clone(),
        &session,
        &request,
        model,
        &hook_context,
        &cancel,
    )
    .await?;
    if compact {
        let current = app
            .store
            .message(sid, uid)?
            .ok_or("Compaction prompt not found")?;
        let (messages, summary) = history::load_messages(app.store.clone(), sid, uid, chat).await?;
        return summarize_history(
            &app,
            adapter.as_ref(),
            &current.info,
            messages,
            summary,
            &CompactionPolicy::for_model(&cfg, model),
            &cancel,
            &hook_context,
            Some(&base.system),
        )
        .await;
    }
    let agent = request.agent.as_deref().unwrap_or("build");
    let allowed: HashSet<String> = base
        .tools
        .iter()
        .filter_map(|d| d["function"]["name"].as_str().map(str::to_string))
        .collect();
    let max_steps = cfg
        .agent
        .get(agent)
        .and_then(|a| a["steps"].as_u64().or_else(|| a["maxSteps"].as_u64()))
        .map(|steps| steps.max(1));
    let mut step = 0u64;
    let mut end_continuations = 0u32;
    let mut recent_calls = VecDeque::new();
    let mut compaction = CompactionPolicy::for_model(&cfg, model);
    // A turn runs until completion, steering or cancellation unless the user
    // explicitly configured an agent step budget.
    loop {
        step = step.saturating_add(1);
        if cancel.is_cancelled() {
            return Err("Aborted".into());
        }
        let (messages, has_summary) =
            history::load_messages(app.store.clone(), sid, uid, chat).await?;
        let mut info = MessageInfo::assistant(sid, Some(uid.into()), model, agent);
        let amid = info.id.clone();
        hook_context["step"] = json!(step);
        hook_context["messageID"] = json!(amid);
        hook_context["requestID"] = json!(hex_id("req_"));
        info.path = Some(json!({"cwd":session.directory,"root":session.directory}));
        let start_part =
            json!({"id":hex_id("prt_"),"sessionID":sid,"messageID":amid,"type":"step-start"});
        crate::messages::save(
            &app,
            &MessageWithParts {
                info: info.clone(),
                parts: vec![start_part.clone()],
            },
        )?;
        let result = async {
            let req = ChatRequest {
                messages,
                ..base.clone()
            };
            let (mut parts, (finish, usage)) = request_step(
                &app,
                adapter.as_ref(),
                req,
                &info,
                chat,
                has_summary,
                &mut compaction,
                &mut hook_context,
                &cancel,
            )
            .await?;
            if !parts.metadata.is_null() {
                info.extra
                    .insert("providerMetadata".into(), parts.metadata.take());
            }
            // Charge each completed provider request once, before any fallible
            // text/result processing or tool execution can end the turn.
            record_step(&app, &cfg, model, &mut info, &finish, &usage)?;
            if let Some(part) = &mut parts.text {
                if app
                    .hooks
                    .matches(crate::provider::hooks::MESSAGE_TEXT_COMPLETE, &hook_context)
                {
                    let before = part.part["text"].as_str().unwrap_or_default().to_string();
                    let text =
                        provider::complete_text(&app, &hook_context, before.clone(), Some(&cancel))
                            .await?;
                    if text != before {
                        part.part["text"] = json!(text);
                        part.part[provider::TEXT_TRANSFORMED] = json!(true);
                        crate::messages::update_part(&app, &part.part)?;
                    }
                }
            }
            let tools_context = ToolCtx {
                hook_context: hook_context.clone(),
                dir: session.directory.clone(),
                app: app.clone(),
                session_id: sid.into(),
                engine: engine.clone(),
                cancel: cancel.clone(),
                message_id: amid.clone(),
                call_id: String::new(),
                part_id: String::new(),
            };
            run_tools(
                &tools_context,
                &cfg,
                &request,
                &allowed,
                &mut parts.tools,
                &mut recent_calls,
            )
            .await?;
            if cancel.is_cancelled() {
                return Err("Aborted".into());
            }
            info.time.completed = Some(now_ms().max(info.time.created));
            crate::messages::update_info(&app, &info)?;
            let response = app
                .store
                .message(sid, &amid)?
                .ok_or("Persisted response not found")?;
            Ok::<_, OperationError>((response, parts.tools, usage))
        }
        .await;
        let (response, tool_parts, usage) = match result {
            Ok(completed) => completed,
            Err(error) => {
                fail_message(&app, &mut info, &error, cancel.is_cancelled())?;
                return Err(error);
            }
        };
        // Compacting only after the window is full burns a failed request;
        // when the window is known, compact as the step approaches it.
        let used =
            usage.input + usage.output + usage.reasoning + usage.cache.read + usage.cache.write;
        if compaction.settings.automatic()
            && compaction
                .usable()
                .is_some_and(|limit| u64::try_from(used).unwrap_or(0) >= limit)
        {
            match history::load_messages(app.store.clone(), sid, uid, chat).await {
                Ok((history, has_summary)) => {
                    if let Err(error) = compact_history(
                        &app,
                        adapter.as_ref(),
                        &info,
                        history,
                        has_summary,
                        &compaction,
                        &cancel,
                    )
                    .await
                    {
                        eprintln!("ocmini: proactive compaction failed: {error}");
                    }
                }
                Err(error) => eprintln!("ocmini: proactive compaction rebuild failed: {error}"),
            }
        }
        if tool_parts.is_empty() || app.store.has_waiting(sid)? {
            if tool_parts.is_empty() && !app.store.has_waiting(sid)? && !cancel.is_cancelled() {
                hook_context["messageID"] = json!(amid);
                hook_context["endAttempt"] = json!(end_continuations + 1);
                let feedback =
                    match end_feedback(&app, &engine, &hook_context, &response, &cancel).await {
                        Ok(feedback) => feedback,
                        Err(error) => {
                            fail_message(&app, &mut info, &error, cancel.is_cancelled())?;
                            return Err(error);
                        }
                    };
                if let Some(feedback) = feedback {
                    if cancel.is_cancelled() {
                        return Err("Aborted".into());
                    }
                    if engine.allow_hook_continuation(&app, sid)? {
                        if app
                            .hooks
                            .max_end_continuations()
                            .is_some_and(|limit| end_continuations >= limit)
                        {
                            let error = "Turn hook continuation limit reached";
                            fail_message(&app, &mut info, error, false)?;
                            return Err(OperationError::new(FailureKind::HookLimit, error));
                        }
                        if max_steps.is_some_and(|limit| step >= limit) {
                            let error = format!(
                                "Agent step limit ({step}) reached before hook continuation"
                            );
                            fail_message(&app, &mut info, &error, false)?;
                            return Err(OperationError::new(FailureKind::StepLimit, error));
                        }
                        end_continuations =
                            app.store.append_hook_feedback(uid, &amid, &feedback)?;
                        app.hooks.emit(
                            event::Turn::Continued {
                                end_attempt: u64::from(end_continuations),
                                message_id: &amid,
                            },
                            &hook_context,
                        );
                        continue;
                    }
                }
            }
            if compaction.settings.prune == Some(true) {
                let (store, sid_text, uid_text) =
                    (app.store.clone(), sid.to_string(), uid.to_string());
                let pruned = tokio::select! {
                    result = tokio::task::spawn_blocking(move || store.prune_tool_outputs(&sid_text, &uid_text)) => result,
                    _ = cancel.cancelled() => return Err("Aborted".into()),
                };
                match pruned {
                    Ok(Ok(parts)) => {
                        for part in parts {
                            crate::messages::announce_part(&app, &part);
                        }
                    }
                    Ok(Err(error)) => eprintln!("ocmini: tool-output pruning failed: {error}"),
                    Err(error) => eprintln!("ocmini: tool-output pruning failed: {error}"),
                }
            }
            return Ok(response);
        }
        if max_steps.is_some_and(|limit| step >= limit) {
            let message=format!("Agent step limit ({step}) reached. Send another message to continue from the saved tool results.");
            emit_error(&app, sid, &message, false);
            return Err(OperationError::new(FailureKind::StepLimit, message));
        }
    }
}
