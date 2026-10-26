use super::*;
use crate::events as event;
use tokio_util::sync::CancellationToken;

#[async_trait::async_trait]
impl EngineApi for Engine {
    fn begin_end_gate(
        &self,
        app: &App,
        sid: &str,
    ) -> Result<Option<CancellationToken>, OperationError> {
        crate::blocking::run(|| {
            let mut running = self.running.lock().unwrap();
            let Some(state) = running.get_mut(sid) else {
                return Ok(None);
            };
            if state.cancel.is_cancelled() || app.store.has_waiting(sid)? {
                return Ok(None);
            }
            let token = CancellationToken::new();
            state.end_gate = Some(token.clone());
            Ok(Some(token))
        })
    }
    fn finish_end_gate(&self, sid: &str, token: &CancellationToken) {
        crate::blocking::run(|| {
            if let Some(state) = self.running.lock().unwrap().get_mut(sid) {
                if state
                    .end_gate
                    .as_ref()
                    .is_some_and(|active| active == token)
                {
                    state.end_gate = None;
                }
            }
        });
    }
    fn allow_hook_continuation(&self, app: &App, sid: &str) -> Result<bool, OperationError> {
        crate::blocking::run(|| {
            let running = self.running.lock().unwrap();
            Ok(running.get(sid).is_some_and(|s| !s.cancel.is_cancelled())
                && !app.store.has_waiting(sid)?)
        })
    }
    async fn admit(
        &self,
        app: App,
        sid: String,
        mut request: PromptRequest,
    ) -> Result<oneshot::Receiver<TurnResult>, OperationError> {
        let session = app
            .store
            .get_session(&sid)?
            .ok_or_else(|| OperationError::not_found("Session not found"))?;
        let app = app.for_directory(&session.directory)?;
        let mid = request
            .message_id
            .get_or_insert_with(|| hex_id("msg_"))
            .clone();
        validate_message_id(&mid)?;
        if !app.hooks.is_empty() {
            request.hook_snapshot = Some(app.hooks.snapshot());
        }
        let input_hooks = app
            .hooks
            .has(crate::engine_impl::hooks::PROMPT_ACCEPT_BEFORE)
            || (request.command.is_some()
                && (app
                    .hooks
                    .has(crate::engine_impl::hooks::COMMAND_EXECUTE_BEFORE)
                    || app
                        .hooks
                        .has(crate::engine_impl::hooks::COMMAND_EXPAND_AFTER)));
        if !input_hooks || app.store.message_owner(&mid)?.is_some() {
            return self.start_turn(app, sid, request);
        }
        let cfg = app.config.as_ref().clone();
        normalize_request(&cfg, &app, &sid, &mut request)?;
        let deadline = now_ms() + app.hooks.admission_timeout_ms() as i64;
        let cancel = CancellationToken::new();
        let token = loop {
            let reservation = crate::blocking::run(|| {
                let running = self.running.lock().unwrap();
                if running
                    .get(&sid)
                    .is_some_and(|state| state.cancel.is_cancelled())
                {
                    return Err(OperationError::busy(
                        "Session is busy aborting pending turns",
                    ));
                }
                let reservation = app.store.reserve_hook_admission(&mid, &sid, deadline)?;
                if matches!(reservation, crate::store::AdmissionReservation::Acquired(_)) {
                    self.admissions.lock().unwrap().insert(
                        mid.clone(),
                        Admitting {
                            app: app.clone(),
                            session_id: sid.clone(),
                            cancel: cancel.clone(),
                        },
                    );
                }
                Ok::<_, OperationError>(reservation)
            })?;
            match reservation {
                crate::store::AdmissionReservation::Acquired(token) => break token,
                crate::store::AdmissionReservation::Existing => {
                    return self.start_turn(app, sid, request)
                }
                crate::store::AdmissionReservation::Rejected(error) => return Err(error.into()),
                crate::store::AdmissionReservation::Waiting => {
                    if now_ms() >= deadline {
                        return Err("Prompt admission deadline exceeded".into());
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            }
        };
        let _reservation_guard = AdmissionGuard {
            admissions: self.admissions.clone(),
            app: app.clone(),
            mid: mid.clone(),
            token: token.clone(),
            cancel: cancel.clone(),
        };
        request.admission_token = Some(token.clone());
        let prepare = async {
            while !app.store.hook_admission_ready(&mid, &token)? {
                if cancel.is_cancelled() {
                    return Err("Aborted".into());
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            let cfg = app.config.as_ref().clone();
            let context = turn_context(&app, &sid, &request);
            if let (Some(command), Some(arguments)) =
                (request.command.clone(), request.command_arguments.clone())
            {
                if app
                    .hooks
                    .has(crate::engine_impl::hooks::COMMAND_EXECUTE_BEFORE)
                {
                    let out = app
                        .hooks
                        .run(
                            crate::engine_impl::hooks::COMMAND_EXECUTE_BEFORE,
                            &context,
                            json!({"arguments":arguments,"command":command}),
                            Some(&cancel),
                        )
                        .await?;
                    let arguments = out.input["arguments"]
                        .as_str()
                        .ok_or("Command hook arguments must be a string")?;
                    if request.command_arguments.as_deref() != Some(arguments) {
                        request =
                            crate::commands::prepare(&cfg, &command, arguments, request, None)?;
                    }
                    request.admission_token = Some(token.clone());
                }
                transform_parts(
                    &app,
                    &mut request,
                    hooks::COMMAND_EXPAND_AFTER,
                    &context,
                    &cancel,
                )
                .await?;
            }
            transform_parts(
                &app,
                &mut request,
                hooks::PROMPT_ACCEPT_BEFORE,
                &context,
                &cancel,
            )
            .await?;
            if cancel.is_cancelled() {
                return Err("Aborted".into());
            }
            self.start_turn(app.clone(), sid.clone(), request)
        };
        let remaining = std::time::Duration::from_millis(
            u64::try_from((deadline - now_ms()).max(0)).unwrap_or(0),
        );
        let result = match tokio::time::timeout(remaining, prepare).await {
            Ok(result) => result,
            Err(_) => Err("Prompt admission deadline exceeded".into()),
        };
        self.admissions.lock().unwrap().remove(&mid);
        if let Err(error) = &result {
            let _ = app.store.reject_hook_admission(&mid, &token, error);
            let context = json!({"directory":session.directory,"sessionID":sid,"turnID":mid});
            app.hooks.emit(
                event::Prompt::Rejected {
                    reason: &error.message,
                },
                &context,
            );
        }
        result
    }
    fn start_turn(
        &self,
        app: App,
        sid: String,
        mut request: PromptRequest,
    ) -> Result<oneshot::Receiver<TurnResult>, OperationError> {
        crate::blocking::run(|| {
            let session = app
                .store
                .get_session(&sid)?
                .ok_or_else(|| OperationError::not_found("Session not found"))?;
            let mut guard = self.running.lock().unwrap();
            let mid = request.message_id.clone().unwrap_or_else(|| hex_id("msg_"));
            validate_message_id(&mid)?;
            let (tx, rx) = oneshot::channel();
            if let Some(owner) = app.store.message_owner(&mid)? {
                if owner != sid {
                    return Err(OperationError::invalid(
                        "messageID already belongs to another session",
                    ));
                }
                let existing = app
                    .store
                    .message(&sid, &mid)?
                    .ok_or_else(|| OperationError::not_found("Message not found"))?;
                if existing.info.role != "user" {
                    return Err(OperationError::invalid(
                        "messageID already belongs to an assistant message",
                    ));
                }
                if let Some((value, finalizing)) = app.store.turn_result(&mid)? {
                    if finalizing {
                        if let Some(state) = guard.get_mut(&sid) {
                            state.waiters.entry(mid).or_default().push(tx);
                            return Ok(rx);
                        }
                        // A sibling owns the finalizer. Its prepared result is
                        // durable, but synchronous completion must still wait
                        // for the finalizer's terminal commit.
                        drop(guard);
                        tokio::spawn(wait_for_finalization(app.store.clone(), mid, tx));
                        return Ok(rx);
                    }
                    let _ = tx.send(value);
                    return Ok(rx);
                }
                if app.store.is_pending_message(&mid)? {
                    if let Some(state) = guard.get_mut(&sid) {
                        state.waiters.entry(mid).or_default().push(tx);
                        return Ok(rx);
                    }
                    let scoped = app.for_directory(&session.directory)?;
                    let cancel = self.claim(&mut guard, &scoped, &sid, Some((mid, tx)))?;
                    drop(guard);
                    let engine = self.clone();
                    tokio::spawn(async move {
                        engine.drain(scoped, sid, cancel).await;
                    });
                    return Ok(rx);
                }
                let result = app.store.reply_to(&sid, &mid)?.unwrap_or(existing);
                let _ = tx.send(Ok(result));
                return Ok(rx);
            }
            if guard
                .get(&sid)
                .is_some_and(|state| state.cancel.is_cancelled())
            {
                return Err(OperationError::busy(
                    "Session is busy aborting pending turns",
                ));
            }
            let app = app.for_directory(&session.directory)?;
            if request.hook_snapshot.is_none() && !app.hooks.is_empty() {
                request.hook_snapshot = Some(app.hooks.snapshot());
            }
            let cfg = app.config.as_ref().clone();
            request.message_id = Some(mid.clone());
            let agent = normalize_request(&cfg, &app, &sid, &mut request)?;
            let mut seen_parts = HashSet::new();
            for part in &mut request.parts {
                if part["type"] == "file" {
                    crate::attachments::snapshot(part)?;
                }
                let id = part["id"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .unwrap_or_else(|| hex_id("prt_"));
                if !seen_parts.insert(id.clone()) {
                    return Err(OperationError::invalid("Duplicate part id in prompt"));
                }
                part["id"] = json!(id);
                part["messageID"] = json!(mid);
                part["sessionID"] = json!(sid);
            }
            // rowid is the stable tie breaker for messages created in one millisecond.
            let created = now_ms();
            let mut info = MessageInfo {
                id: mid.clone(),
                session_id: sid.clone(),
                role: "user".into(),
                time: MessageTime {
                    created,
                    completed: None,
                },
                model: request.model.clone(),
                agent: Some(agent),
                system: request.system.clone(),
                variant: request.variant.clone(),
                ..Default::default()
            };
            if let Some(tools) = &request.tools {
                info.extra.insert("tools".into(), json!(tools));
            }
            if let Some(command) = &request.command {
                info.extra.insert("command".into(), json!(command));
            }
            let message = MessageWithParts {
                info,
                parts: request.parts.clone(),
            };
            // Reserve an idle session before persisting acknowledged work.
            // Otherwise another process could claim it between admission and
            // starting the drain, leaving work queued after a rejected request.
            let claimed = !request.no_reply && !guard.contains_key(&sid);
            if claimed {
                app.store.claim(&sid)?;
            }
            if let Err(error) = app.store.admit(&request, &message).and_then(|inserted| {
                if inserted {
                    Ok(())
                } else {
                    Err("Duplicate prompt admission race; retry the same messageID".into())
                }
            }) {
                if claimed {
                    app.store.release(&sid);
                }
                return Err(error);
            }
            crate::messages::announce(&app, &message);
            app.hooks.emit(
                event::Prompt::Accepted {
                    mode: if request.no_reply {
                        event::PromptMode::Context
                    } else {
                        event::PromptMode::Execution
                    },
                    message_id: &mid,
                },
                &turn_context(&app, &sid, &request),
            );
            // A launcher title ("tui session", "run") says nothing about the chat.
            // Name the session after its first prompt, with the model writing the
            // title in the background when `title_model` is configured.
            if request.no_reply {
                let _ = tx.send(Ok(message));
                return Ok(rx);
            }
            if let Some(model) = &request.model {
                title::launch(app.clone(), sid.clone(), &message.parts, model);
            }
            if let Some(state) = guard.get_mut(&sid) {
                if let Some(gate) = &state.end_gate {
                    gate.cancel();
                }
                state.waiters.entry(mid).or_default().push(tx);
                return Ok(rx);
            }
            let cancel = self.claim(&mut guard, &app, &sid, Some((mid, tx)))?;
            drop(guard);
            let engine = self.clone();
            tokio::spawn(async move {
                engine.drain(app, sid, cancel).await;
            });
            Ok(rx)
        })
    }

    fn abort(&self, sid: &str) {
        crate::blocking::run(|| {
            let mut queue = vec![sid.to_string()];
            let running = self.running.lock().unwrap();
            let mut visited = HashSet::new();
            while let Some(id) = queue.pop() {
                if !visited.insert(id.clone()) {
                    continue;
                }
                for admission in self
                    .admissions
                    .lock()
                    .unwrap()
                    .values()
                    .filter(|a| a.session_id == id)
                {
                    admission.cancel.cancel();
                    let _ = admission.app.store.cancel_hook_admissions(&id);
                }
                crate::tools::cancel_background_jobs(&id);
                if let Some(state) = running.get(&id) {
                    state.cancel.cancel();
                    // This also rejects reservations held by sibling
                    // instances, even when there are no local input hooks.
                    let _ = state.app.store.cancel_hook_admissions(&id);
                    if let Some(gate) = &state.end_gate {
                        gate.cancel();
                    }
                    state.app.perms.reject_all(&id);
                    state.app.questions.reject_all(&id);
                    queue.extend(
                        state
                            .app
                            .store
                            .children(&id)
                            .unwrap_or_default()
                            .into_iter()
                            .map(|s| s.id),
                    );
                }
            }
        })
    }

    fn summarize(
        &self,
        app: App,
        sid: String,
        model: Option<ModelRef>,
    ) -> Result<oneshot::Receiver<TurnResult>, OperationError> {
        self.start_turn(
            app,
            sid,
            PromptRequest {
                model,
                parts: vec![json!({"type":"compaction","auto":false})],
                command: Some("__ocmini_compact".into()),
                ..Default::default()
            },
        )
    }

    fn recover(&self, app: App) {
        self.recover_once(&app);
        let running = Arc::downgrade(&self.running);
        let admissions = Arc::downgrade(&self.admissions);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(20)).await;
                let (Some(running), Some(admissions)) = (running.upgrade(), admissions.upgrade())
                else {
                    break;
                };
                Engine {
                    running,
                    admissions,
                }
                .recover_once(&app);
            }
        });
    }
}

async fn transform_parts(
    app: &App,
    request: &mut PromptRequest,
    hook: crate::hooks::Hook,
    context: &Value,
    cancel: &CancellationToken,
) -> Result<(), OperationError> {
    if !app.hooks.has(hook) {
        return Ok(());
    }
    let mut input = json!({});
    input["parts"] = Value::Array(std::mem::take(&mut request.parts));
    let mut out = app
        .hooks
        .run_with(hook, context, input, Some(cancel), |_, set| {
            if let Some(parts) = set.get("parts").and_then(Value::as_array) {
                validate_parts(parts, request.part_scope())?;
            }
            Ok(())
        })
        .await?;
    request.parts = match out.input["parts"].take() {
        Value::Array(parts) => parts,
        _ => return Err("Hook parts must be an array".into()),
    };
    Ok(())
}

fn normalize_request(
    cfg: &Config,
    app: &App,
    sid: &str,
    request: &mut PromptRequest,
) -> Result<String, OperationError> {
    let agent = request
        .agent
        .get_or_insert_with(|| {
            cfg.extra
                .get("default_agent")
                .and_then(Value::as_str)
                .unwrap_or("build")
                .to_owned()
        })
        .clone();
    if !["build", "plan", "general", "explore", "subagent"].contains(&agent.as_str())
        && !cfg.agent.contains_key(&agent)
    {
        return Err(OperationError::invalid(format!("Unknown agent '{agent}'")));
    }
    if cfg.agent.get(&agent).is_some_and(|a| a["disable"] == true) {
        return Err(OperationError::invalid(format!(
            "Agent '{agent}' is disabled"
        )));
    }
    let model = resolve_model(cfg, app, sid, request.model.as_ref(), Some(&agent))?;
    if !cfg.model_available(&model.provider_id, &model.model_id) {
        return Err(OperationError::invalid(format!(
            "Provider or model is disabled: {model}"
        )));
    }
    request.model = Some(model);
    validate_parts(&request.parts, request.part_scope())?;
    Ok(agent)
}

fn resolve_model(
    cfg: &Config,
    app: &App,
    sid: &str,
    selected: Option<&ModelRef>,
    agent: Option<&str>,
) -> Result<ModelRef, OperationError> {
    if let Some(model) = selected {
        model.validate().map_err(OperationError::invalid)?;
        return Ok(model.clone());
    }
    let agent_model = agent
        .and_then(|a| cfg.agent.get(a))
        .and_then(|a| a["model"].as_str());
    for value in agent_model
        .into_iter()
        .chain(std::iter::once(cfg.model.as_str()))
    {
        if let Ok(model) = ModelRef::parse(value) {
            return Ok(model);
        }
    }
    if let Some(model) = app.store.last_model(sid)? {
        return Ok(model);
    }
    let catalog = cfg.catalog();
    if catalog.len() == 1 {
        return Ok(ModelRef {
            provider_id: catalog[0]["providerID"].as_str().unwrap_or_default().into(),
            model_id: catalog[0]["modelID"].as_str().unwrap_or_default().into(),
        });
    }
    Err("No model selected. Set model to 'provider/model' in opencode.json or supply model.providerID and model.modelID.".into())
}
