use super::*;
use crate::events as event;
use tokio_util::sync::CancellationToken;

pub(super) const COMPACTION_PROMPT: &str = "Summarize the conversation for another coding agent to continue. Preserve the user's goal, constraints, decisions, changed files, executed commands and their results, unresolved errors, and next actions. Quote relevant identifiers and paths. Do not perform the task or call tools. Produce a substantive continuation summary.";

/// Transient provider failures permit five retries per request. OpenCode
/// retries every model request, the compaction summary included, through the
/// same processor policy.
pub(super) const MAX_RETRIES: u32 = 5;

/// OpenCode caps each tool result in the summarizer input at 2000 characters:
/// the summary needs the gist of a tool call, not its full output. Carrying
/// whole tool outputs makes the summary request the largest of the turn.
pub(super) const TOOL_OUTPUT_MAX_CHARS: usize = 2_000;

/// Truncate one tool result for the summarizer input, matching OpenCode's
/// `truncate()`. Counts Unicode scalar characters, as ocmini's other caps do.
pub(super) fn truncate_tool_output(text: &str) -> String {
    if text.chars().count() <= TOOL_OUTPUT_MAX_CHARS {
        return text.to_string();
    }
    let mut kept: String = text.chars().take(TOOL_OUTPUT_MAX_CHARS).collect();
    kept.push_str("\n[truncated]");
    kept
}

#[derive(Default)]
pub(super) struct CompactionPolicy {
    pub(super) settings: CompactionCfg,
    pub(super) context: Option<u64>,
    pub(super) input: Option<u64>,
    pub(super) output: u64,
}

impl CompactionPolicy {
    pub(super) fn for_model(cfg: &Config, model: &ModelRef) -> Self {
        let provider = cfg.provider(&model.provider_id);
        let configured = provider.and_then(|provider| provider.models.get(&model.model_id));
        let limit = configured.and_then(|model| model.limit.as_ref());
        let output = limit
            .and_then(|limit| limit["output"].as_u64())
            .filter(|output| *output > 0)
            .unwrap_or(32_000)
            .min(32_000);
        Self {
            settings: cfg.compaction.clone(),
            context: limit.and_then(|limit| limit["context"].as_u64()),
            input: limit.and_then(|limit| limit["input"].as_u64()),
            output,
        }
    }

    pub(super) fn usable(&self) -> Option<u64> {
        self.settings.usable(self.context, self.input, self.output)
    }
}

/// Keep roughly 15k tokens verbatim (less for small windows). Budget whole
/// messages, including tool arguments/results; never start with an orphan result.
/// The latest user message takes priority even when it exceeds the soft budget.
pub(super) fn compaction_messages(
    mut messages: Vec<Value>,
    has_summary: bool,
    policy: &CompactionPolicy,
) -> (Vec<Value>, Vec<Value>) {
    let first = usize::from(has_summary);
    let user = messages
        .iter()
        .enumerate()
        .skip(first)
        .rfind(|(_, message)| message["role"] == "user")
        .map(|(index, _)| index);
    let budget = policy.settings.recent_bytes(policy.usable());
    let retain = budget > 0 && policy.settings.tail_turns != Some(0);
    let user = user.filter(|_| retain);
    let recent_first = policy
        .settings
        .tail_turns
        .and_then(|turns| {
            messages
                .iter()
                .enumerate()
                .skip(first)
                .rev()
                .filter(|(_, message)| message["role"] == "user")
                .nth(usize::try_from(turns.saturating_sub(1)).unwrap_or(usize::MAX))
                .map(|(index, _)| index)
        })
        .unwrap_or(first);
    let mut bytes = user.map_or(0, |index| messages[index].to_string().len());
    let mut start = messages.len();
    for index in (recent_first..messages.len()).rev().filter(|_| retain) {
        let size = if Some(index) == user {
            0
        } else {
            messages[index].to_string().len()
        };
        if bytes.saturating_add(size) > budget {
            break;
        }
        bytes += size;
        if messages[index]["role"] == "user" || messages[index]["role"] == "assistant" {
            start = index;
        }
    }
    let mut recent = messages.split_off(start);
    if let Some(index) = user.filter(|index| *index < start) {
        recent.insert(0, messages[index].clone());
    }
    if messages.is_empty() {
        // Short conversations can fit entirely in the verbatim tail. Still
        // produce a useful checkpoint when compaction is explicitly requested.
        messages = recent.clone();
    }

    // Leave room for the summarizer's reply, preserving the previous checkpoint
    // and whole assistant/tool groups when older context needs to be shortened.
    let budget = usize::try_from(
        policy
            .usable()
            .unwrap_or(64_000)
            .saturating_mul(2)
            .max(8_000),
    )
    .unwrap_or(usize::MAX);
    let mut total = 0usize;
    let mut start = messages.len();
    while start > first {
        let size = messages[start - 1].to_string().len();
        if start < messages.len() && total.saturating_add(size) > budget {
            break;
        }
        total = total.saturating_add(size);
        start -= 1;
    }
    while start > first && messages[start]["role"] == "tool" {
        start -= 1;
    }
    let older = messages
        .into_iter()
        .enumerate()
        .filter(|(index, _)| *index >= start || has_summary && *index == 0)
        .map(|(_, message)| message)
        .collect();
    (older, recent)
}

pub(super) fn prepare_summary_messages(messages: &mut Vec<Value>) {
    for message in messages.iter_mut().filter(|m| m["role"] == "tool") {
        if let Some(content) = message["content"].as_str() {
            message["content"] = json!(truncate_tool_output(content));
        }
    }
    messages.push(json!({"role":"user","content":"Produce the continuation summary now."}));
}

/// Compact history mid-turn: ask the model for a continuation summary and
/// persist it as a summary message. build_messages restarts history from the
/// newest summary, so the next request carries the summary instead of the
/// transcript that overflowed the provider's context window.
pub(super) async fn compaction_prompt(
    app: &App,
    context: &Value,
    default: &str,
    cancel: &CancellationToken,
) -> Result<String, OperationError> {
    #[derive(serde::Serialize, serde::Deserialize)]
    struct Input {
        context: Vec<String>,
        prompt: Option<String>,
    }
    let input = app
        .hooks
        .transform(
            hooks::COMPACTION_BEFORE,
            context,
            Input {
                context: Vec::new(),
                prompt: None,
            },
            Some(cancel),
        )
        .await?;
    let mut prompt = input.prompt.unwrap_or_else(|| default.to_owned());
    for text in input.context {
        prompt.push_str("\n\n");
        prompt.push_str(&text);
    }
    if prompt.trim().is_empty() {
        return Err("Compaction hook prompt must not be empty".into());
    }
    Ok(prompt)
}

pub(super) async fn compaction_summary(
    app: &App,
    context: &Value,
    summary: String,
    source_bytes: usize,
    cancel: &CancellationToken,
) -> Result<String, OperationError> {
    let text = if app.hooks.has(crate::engine_impl::hooks::COMPACTION_AFTER) {
        let result = app
            .hooks
            .run(
                crate::engine_impl::hooks::COMPACTION_AFTER,
                context,
                json!({"summary":summary}),
                Some(cancel),
            )
            .await?;
        let text = result.input["summary"]
            .as_str()
            .ok_or("Compaction summary must be text")?
            .to_string();
        if context["turnKind"] != "compaction" && text.len() >= source_bytes {
            return Err("Transformed compaction summary did not shrink the context".into());
        }
        text
    } else {
        summary
    };
    if text.trim().is_empty() {
        return Err("Provider returned an empty compaction summary".into());
    }
    Ok(text)
}

pub(super) async fn compact_history(
    app: &App,
    adapter: &dyn provider::ProviderAdapter,
    current: &MessageInfo,
    messages: Vec<Value>,
    has_summary: bool,
    policy: &CompactionPolicy,
    cancel: &CancellationToken,
) -> Result<(), OperationError> {
    let context = app.session_context(&current.session_id, json!({"turnID":current.parent_id,"messageID":current.id,"agent":current.agent,"model":{"providerID":current.provider_id,"modelID":current.model_id},"turnKind":"primary","requestKind":"compaction","requestID":hex_id("req_")}));
    let result = summarize_history(
        app,
        adapter,
        current,
        messages,
        has_summary,
        policy,
        cancel,
        &context,
        None,
    )
    .await;
    if let Err(error) = &result {
        app.hooks.emit(
            event::Compaction::Failed {
                error: &error.message,
            },
            &context,
        );
    }
    result.map(|_| ())
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn summarize_history(
    app: &App,
    adapter: &dyn provider::ProviderAdapter,
    current: &MessageInfo,
    messages: Vec<Value>,
    has_summary: bool,
    policy: &CompactionPolicy,
    cancel: &CancellationToken,
    context: &Value,
    system: Option<&str>,
) -> Result<MessageWithParts, OperationError> {
    let sid = &current.session_id;
    let model = match current.model.clone() {
        Some(model) => model,
        None => ModelRef {
            provider_id: current
                .provider_id
                .clone()
                .ok_or("Missing compaction provider")?,
            model_id: current.model_id.clone().ok_or("Missing compaction model")?,
        },
    };
    let parent = if current.role == "user" {
        Some(current.id.clone())
    } else {
        current.parent_id.clone()
    };
    let mut info = MessageInfo::assistant(sid, parent, &model, "compaction");
    let mut context = context.clone();
    context["messageID"] = json!(info.id);
    app.hooks.emit(
        event::Compaction::Started {
            mode: if current.role == "user" {
                event::CompactionMode::Manual
            } else {
                event::CompactionMode::Automatic
            },
        },
        &context,
    );
    let start =
        json!({"id":hex_id("prt_"),"sessionID":sid,"messageID":info.id,"type":"step-start"});
    crate::messages::save(
        app,
        &MessageWithParts {
            info: info.clone(),
            parts: vec![start.clone()],
        },
    )?;
    let result: Result<MessageWithParts, OperationError> = async {
        let (mut request_messages, recent) = compaction_messages(messages, has_summary, policy);
        if request_messages.is_empty() {
            return Err("Nothing to compact".into());
        }
        prepare_summary_messages(&mut request_messages);
        let source_bytes = request_messages.iter().map(|v| v.to_string().len()).sum();
        let mut request = ChatRequest {
            model: model.model_id.clone(),
            system: {
                let prompt = compaction_prompt(app, &context, COMPACTION_PROMPT, cancel).await?;
                system.map_or(prompt.clone(), |system| format!("{system}\n\n{prompt}"))
            },
            messages: request_messages,
            session_id: Some(sid.into()),
            ..Default::default()
        };
        provider::prepare_hooks(app, &context, &mut request, Some(cancel)).await?;
        // The summary request carries most of the window, so it is the largest and
        // slowest request of the turn. OpenCode sends it through the same retry
        // policy as a model step; the transport deaths the main loop retries must
        // not end recovery here, or the overflow error surfaces verbatim and the
        // turn is lost.
        let mut attempt = 0u32;
        let completion = loop {
            context["attempt"] = json!(attempt);
            match provider::request_text(app, adapter, &context, &request, cancel).await {
                Ok(completion) => break completion,
                Err(error) if error.retryable && attempt < MAX_RETRIES => {
                    attempt += 1;
                    wait_retry(
                        app,
                        sid,
                        &error.message,
                        error.retry_after,
                        attempt,
                        cancel,
                        &context,
                    )
                    .await?;
                }
                Err(error) => return Err(error.into()),
            }
        };
        record_step(
            app,
            &app.config,
            &model,
            &mut info,
            &completion.finish,
            &completion.usage,
        )?;
        let summary = provider::complete_text(app, &context, completion.text, Some(cancel)).await?;
        let text = compaction_summary(app, &context, summary, source_bytes, cancel).await?;
        let now = now_ms();
        let mid = &info.id;
        let mut part = json!({"id": hex_id("prt_"), "sessionID": sid, "messageID": mid, "type": "text", "text": text, "time": {"start": now, "end": now}});
        part["compactionRecent"] = Value::Array(recent);
        if current.role == "assistant" && current.time.completed.is_none() {
            // Overflow recovery finishes this already-created response after the
            // checkpoint. Its final text/results must remain visible on later turns.
            part["compactionPending"] = json!(current.id);
        }
        info.summary = Some(true);
        info.time.completed = Some(now_ms());
        crate::messages::save(app, &MessageWithParts { info: info.clone(), parts: vec![part.clone()] })?;
        app.hooks.emit(event::Session::Compacted { session_id: sid }, &app.session_context(sid, json!({})));
        app.store.message(sid, &info.id)?.ok_or_else(|| "Persisted summary not found".into())
    }.await;
    if let Err(error) = &result {
        fail_message(app, &mut info, error, cancel.is_cancelled())?;
    }
    result
}
