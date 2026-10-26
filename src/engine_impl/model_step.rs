use super::*;
use crate::events as event;
use tokio_util::sync::CancellationToken;

#[derive(Default)]
pub(super) struct ModelStep {
    pub(super) text: Option<StreamedPart>,
    pub(super) reasoning: Option<StreamedPart>,
    pub(super) tools: Vec<Value>,
    pub(super) metadata: Value,
}

pub(super) async fn read_step(
    app: &App,
    sid: &str,
    amid: &str,
    adapter: &dyn provider::ProviderAdapter,
    req: &ChatRequest,
    context: &Value,
    cancel: &CancellationToken,
) -> Result<(ModelStep, Result<(String, Tokens), provider::ProviderError>), OperationError> {
    let mut step = ModelStep::default();
    let result = provider::request_completion(app, adapter, context, req, cancel, |event| {
        match event {
            ProviderEvent::TextDelta(delta) => {
                append_delta(app, sid, amid, "text", &mut step.text, &delta)?
            }
            ProviderEvent::ReasoningDelta(delta) => {
                append_delta(app, sid, amid, "reasoning", &mut step.reasoning, &delta)?
            }
            ProviderEvent::Metadata(value) => step.metadata = value,
            ProviderEvent::ToolCallStart {
                call_id,
                name,
                args,
            } => {
                if call_id.is_empty()
                    || name.is_empty()
                    || step.tools.iter().any(|p| p["callID"] == call_id)
                {
                    return Err("Provider returned a missing or duplicate tool call identifier".into());
                }
                let input = serde_json::from_str::<Value>(&args);
                let state = match input {
                    Ok(input) if input.is_object() => {
                        json!({"status":"pending","input":input,"raw":args})
                    }
                    _ => {
                        json!({"status":"error","input":{},"error":"Provider returned invalid JSON tool arguments","time":{"start":now_ms(),"end":now_ms()}})
                    }
                };
                let part = json!({"id":hex_id("prt_"),"sessionID":sid,"messageID":amid,"type":"tool","tool":name,"callID":call_id,"state":state});
                crate::messages::update_part(app, &part)?;
                step.tools.push(part);
            }
            _ => {}
        }
        Ok(())
    }).await;
    for part in [&mut step.text, &mut step.reasoning].into_iter().flatten() {
        part.part["time"]["end"] = json!(now_ms());
        crate::messages::update_part(app, &part.part)?;
    }
    Ok((step, result))
}

/// Streaming parts persist to SQLite at most every flush interval; deltas are
/// announced on the bus immediately. Crash recovery marks the interrupted
/// message, so the bounded persistence lag never presents partial text as done.
pub(super) struct StreamedPart {
    pub(super) part: Value,
    last_flush: std::time::Instant,
}

const STREAM_FLUSH: std::time::Duration = std::time::Duration::from_millis(250);

pub(super) fn append_delta(
    app: &App,
    sid: &str,
    mid: &str,
    kind: &str,
    current: &mut Option<StreamedPart>,
    delta: &str,
) -> Result<(), OperationError> {
    if delta.is_empty() {
        return Ok(());
    }
    if current.is_none() {
        let part = json!({"id":hex_id("prt_"),"sessionID":sid,"messageID":mid,"type":kind,"text":"","time":{"start":now_ms()}});
        crate::messages::update_part(app, &part)?;
        *current = Some(StreamedPart {
            part,
            last_flush: std::time::Instant::now(),
        });
    }
    let entry = current.as_mut().ok_or("Missing streamed part")?;
    if let Value::String(text) = &mut entry.part["text"] {
        text.push_str(delta);
    }
    app.hooks.emit(
        event::Part::Delta {
            session_id: sid,
            message_id: mid,
            part_id: entry.part["id"].as_str().unwrap_or_default(),
            field: event::DeltaField::Text,
            delta,
        },
        &app.session_context(sid, json!({})),
    );
    if entry.last_flush.elapsed() >= STREAM_FLUSH {
        app.store.update_part(sid, &entry.part)?;
        entry.last_flush = std::time::Instant::now();
    }
    Ok(())
}

pub(super) async fn wait_retry(
    app: &App,
    sid: &str,
    message: &str,
    retry_after: Option<std::time::Duration>,
    attempt: u32,
    cancel: &CancellationToken,
    context: &Value,
) -> Result<(), OperationError> {
    let mut delay = retry_delay(retry_after, attempt).min(std::time::Duration::from_secs(120));
    if app.hooks.has(crate::provider::hooks::MODEL_RETRY_BEFORE) {
        let result = app
            .hooks
            .run(
                crate::provider::hooks::MODEL_RETRY_BEFORE,
                context,
                json!({"delayMs":delay.as_millis() as u64,"attempt":attempt,"error":message}),
                Some(cancel),
            )
            .await?;
        let millis = result.input["delayMs"]
            .as_u64()
            .filter(|ms| *ms <= 120_000)
            .ok_or("Retry hook delay must be 0–120000 milliseconds")?;
        delay = std::time::Duration::from_millis(millis);
    }
    app.hooks.emit(
        event::Model::Retrying {
            attempt,
            delay_ms: delay.as_millis() as u64,
        },
        context,
    );
    crate::sessions::set_status(
        app,
        sid,
        event::SessionStatus::Retry {
            attempt,
            message: message.to_owned(),
            next: now_ms() + delay.as_millis() as i64,
        },
    );
    tokio::select! {
        _ = tokio::time::sleep(delay) => {},
        _ = cancel.cancelled() => return Err(OperationError::new(FailureKind::Cancelled, "Aborted")),
    }
    crate::sessions::set_status(app, sid, event::SessionStatus::Busy);
    Ok(())
}

fn retry_delay(retry_after: Option<std::time::Duration>, attempt: u32) -> std::time::Duration {
    let base = std::env::var("OCMINI_RETRY_BASE_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(1000);
    let base = std::time::Duration::from_millis(base);
    let backoff = base * (1u32 << attempt.min(6));
    let delay = retry_after
        .unwrap_or(backoff)
        .min(std::time::Duration::from_secs(120));
    let jitter =
        std::time::Duration::from_millis(rand::random::<u64>() % (base.as_millis() as u64 / 4 + 1));
    delay + jitter
}

pub(super) fn record_step(
    app: &App,
    cfg: &Config,
    model: &ModelRef,
    info: &mut MessageInfo,
    finish: &str,
    usage: &Tokens,
) -> Result<(), OperationError> {
    let cost = crate::usage_db::record(
        app,
        cfg,
        model,
        &info.session_id,
        &info.id,
        usage,
        info.time.created,
    )?;
    let finish = match finish {
        "tool_calls" | "tool_use" => "tool-calls",
        "end_turn" => "stop",
        other => other,
    };
    info.tokens = Some(usage.clone());
    info.cost = Some(cost);
    info.finish = Some(finish.into());
    app.store.add_message(&MessageWithParts {
        info: info.clone(),
        parts: Vec::new(),
    })?;
    let mut step = json!({"id":hex_id("prt_"),"sessionID":info.session_id,"messageID":info.id,"type":"step-finish","reason":finish,"cost":cost,"tokens":usage});
    if let Some(metadata) = info.extra.get("providerMetadata") {
        step["providerMetadata"] = metadata.clone();
    }
    crate::messages::update_part(app, &step)?;
    Ok(())
}

pub(super) fn fail_message(
    app: &App,
    info: &mut MessageInfo,
    error: &str,
    aborted: bool,
) -> Result<(), OperationError> {
    if let Some(message) = app.store.message(&info.session_id, &info.id)? {
        for mut part in message.parts.into_iter().filter(|part| {
            part["type"] == "tool"
                && matches!(
                    part["state"]["status"].as_str(),
                    Some("pending" | "running")
                )
        }) {
            part["state"] = json!({"status":"error","input":part["state"]["input"],"error":error,
                "metadata":part["state"]["metadata"],"time":{"start":part["state"]["time"]["start"].as_i64().unwrap_or_else(now_ms),"end":now_ms()}});
            crate::messages::update_part(app, &part)?;
        }
    }
    info.time.completed = Some(now_ms().max(info.time.created));
    info.error = Some(error_value(error, aborted));
    crate::messages::update_info(app, info)?;
    Ok(())
}

/// A model step owns its retries and overflow recovery. The turn owns the
/// accepted result, tool effects and the decision to continue.
#[allow(clippy::too_many_arguments)]
pub(super) async fn request_step(
    app: &App,
    adapter: &dyn provider::ProviderAdapter,
    mut req: ChatRequest,
    info: &MessageInfo,
    chat: bool,
    mut has_summary: bool,
    compaction: &mut CompactionPolicy,
    context: &mut Value,
    cancel: &CancellationToken,
) -> Result<(ModelStep, (String, Tokens)), OperationError> {
    let sid = info.session_id.as_str();
    let amid = info.id.as_str();
    let uid = info.parent_id.as_deref().ok_or("Missing step parent")?;
    let (system, variant) = (req.system.clone(), req.variant.clone());
    let mut attempt: u32 = 0;
    // Compaction may repeat while it reduces an overflowing request. Stop
    // when a checkpoint makes no progress, rather than after a fixed count.
    let mut previous_overflow_bytes = None;
    provider::prepare_hooks(app, context, &mut req, Some(cancel)).await?;
    loop {
        context["attempt"] = json!(attempt);
        let (mut result, completion) =
            read_step(app, sid, amid, adapter, &req, context, cancel).await?;
        let error = match completion {
            Ok(completion) => return Ok((result, completion)),
            Err(error) => error,
        };
        if error.retryable
            && result.tools.is_empty()
            && attempt < MAX_RETRIES
            && !cancel.is_cancelled()
        {
            for part in [&mut result.text, &mut result.reasoning]
                .into_iter()
                .flatten()
            {
                part.part["text"] = json!("");
                crate::messages::update_part(app, &part.part)?;
            }
            attempt += 1;
            wait_retry(
                app,
                sid,
                &error.message,
                error.retry_after,
                attempt,
                cancel,
                context,
            )
            .await?;
            continue;
        }
        // A context overflow can never succeed verbatim: compact
        // the history to the provider's stated window and replay
        // this step against the summary.
        let request_bytes = req
            .messages
            .iter()
            .map(|message| message.to_string().len())
            .sum::<usize>();
        let kind = error.kind;
        let mut message = error.message;
        if error.overflow
            && compaction.settings.automatic()
            && previous_overflow_bytes.is_none_or(|previous| request_bytes < previous)
            && !cancel.is_cancelled()
        {
            previous_overflow_bytes = Some(request_bytes);
            if let Some(parsed) = provider::parse_context_limit(&message) {
                compaction.context = Some(parsed);
            }
            match compact_history(
                app,
                adapter,
                info,
                std::mem::take(&mut req.messages),
                has_summary,
                compaction,
                cancel,
            )
            .await
            {
                Ok(()) => {
                    (req.messages, has_summary) =
                        history::load_messages(app.store.clone(), sid, uid, chat).await?;
                    req.system = system.clone();
                    req.options = Value::Null;
                    req.headers.clear();
                    req.variant = variant.clone();
                    context["requestID"] = json!(hex_id("req_"));
                    provider::prepare_hooks(app, context, &mut req, Some(cancel)).await?;
                    continue;
                }
                Err(compact_error) => {
                    // The server's stderr is invisible to the TUI, so
                    // the reason recovery gave up must reach the user.
                    eprintln!("ocmini: auto-compaction failed: {compact_error}");
                    message = format!("{} (auto-compaction failed: {compact_error})", message);
                }
            }
        }
        return Err(OperationError::new(kind, message));
    }
}
