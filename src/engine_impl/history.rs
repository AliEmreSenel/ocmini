use super::*;

pub(super) async fn load_messages(
    store: Arc<Store>,
    sid: &str,
    through: &str,
    chat: bool,
) -> Result<(Vec<Value>, bool), String> {
    let (sid, through) = (sid.to_owned(), through.to_owned());
    tokio::task::spawn_blocking(move || build_messages(&store, &sid, &through, chat))
        .await
        .map_err(|error| error.to_string())?
}

/// One turn of assistant output, merged into the single assistant message
/// OpenCode's Chat Completions converter sends: every step's chain of thought
/// in order, every step's tool calls, then the tool results.
#[derive(Default)]
struct ChatRun {
    texts: Vec<String>,
    calls: Vec<Value>,
    reasoning: Vec<(String, String)>,
    details: Vec<Value>,
    tail: Vec<Value>,
}

fn flush_chat_run(out: &mut Vec<Value>, run: Option<ChatRun>) {
    let Some(run) = run else { return };
    let mut message = json!({"role":"assistant"});
    let content = run.texts.join("\n");
    if !content.is_empty() {
        message["content"] = json!(content);
    }
    if !run.calls.is_empty() {
        message["tool_calls"] = json!(run.calls);
    }
    if !run.reasoning.is_empty() || !run.details.is_empty() {
        let mut meta_message = json!({"role":"assistant"});
        let mut keys: Vec<String> = Vec::new();
        let mut joined: HashMap<String, Vec<String>> = HashMap::new();
        for (key, text) in run.reasoning {
            if !joined.contains_key(&key) {
                keys.push(key.clone());
            }
            joined.entry(key).or_default().push(text);
        }
        for key in keys {
            meta_message[&key] = json!(joined[&key].join("\n"));
        }
        if !run.details.is_empty() {
            meta_message["reasoning_details"] = json!(run.details);
        }
        message["providerMetadata"] = json!({"provider":"openai-chat","message":meta_message});
    }
    if message.get("content").is_none()
        && message.get("tool_calls").is_none()
        && message.get("providerMetadata").is_none()
    {
        return;
    }
    out.push(message);
    out.extend(run.tail);
}

pub(super) fn build_messages(
    store: &Store,
    sid: &str,
    through: &str,
    chat: bool,
) -> Result<(Vec<Value>, bool), String> {
    let mut history = store.context_messages(sid, through)?;
    let summary = history.iter().rposition(|m| {
        m.info.summary == Some(true)
            && m.info.time.completed.is_some()
            && m.info.error.is_none()
            && m.parts.iter().any(|p| {
                p["type"] == "text" && p["text"].as_str().is_some_and(|s| !s.trim().is_empty())
            })
    });
    let mut out = Vec::new();
    let mut run: Option<ChatRun> = None;
    let start = if let Some(index) = summary {
        let text = crate::types::text_parts(&history[index].parts)
            .collect::<Vec<_>>()
            .join("\n");
        out.push(json!({"role":"user","content":format!("Continuation summary of the earlier conversation:\n{text}")}));
        for part in &mut history[index].parts {
            if let Some(value) = part.get_mut("compactionRecent") {
                if let Value::Array(recent) = value.take() {
                    out.extend(recent);
                }
            }
        }
        index + 1
    } else {
        0
    };
    for message in history.into_iter().skip(start) {
        if message.info.role == "user" {
            flush_chat_run(&mut out, run.take());
            let mut content = Vec::new();
            for part in &message.parts {
                if part["ignored"] == true {
                    continue;
                }
                match part["type"].as_str() {
                    Some("text")=>content.push(json!({"type":"text","text":part["text"]})),
                    Some("file")=>content.push(crate::attachments::content(part)?),
                    Some("agent")=>content.push(json!({"type":"text","text":format!("Requested agent: {}",part["name"].as_str().unwrap_or_default())})),
                    _=>{},
                }
            }
            if content.is_empty() {
                continue;
            }
            let content = if content.iter().all(|p| p["type"] == "text") {
                json!(content
                    .iter()
                    .filter_map(|p| p["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("\n"))
            } else {
                json!(content)
            };
            out.push(json!({"role":"user","content":content}));
            continue;
        }
        if message.info.role != "assistant" {
            continue;
        }
        let text = crate::types::text_parts(&message.parts)
            .collect::<Vec<_>>()
            .join("");
        let calls: Vec<_> = message
            .parts
            .iter()
            .filter(|p| p["type"] == "tool")
            .collect();
        let metadata = message
            .parts
            .iter()
            .rev()
            .find_map(|p| p.get("providerMetadata"))
            .or_else(|| message.info.extra.get("providerMetadata"))
            .filter(|value| !value.is_null())
            .cloned();
        let mut attachments = Vec::new();
        let mut tail: Vec<Value> = Vec::new();
        for part in &calls {
            let content = match part["state"]["status"].as_str() {
                Some("completed") if part["state"]["time"]["compacted"].is_number() => {
                    "[Old tool result content cleared]".into()
                }
                Some("completed") => part["state"]["output"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
                Some("error") => format!(
                    "Tool failed: {}",
                    part["state"]["error"]
                        .as_str()
                        .unwrap_or("Unknown tool error")
                ),
                _ => "Tool execution was interrupted before a result was recorded.".into(),
            };
            tail.push(json!({"role":"tool","tool_call_id":part["callID"],"content":content,"is_error":part["state"]["status"]!="completed"}));
            if let Some(blocks) = part["state"]["metadata"]["attachments"]
                .as_array()
                .filter(|_| !part["state"]["time"]["compacted"].is_number())
            {
                for block in blocks {
                    match block["type"].as_str() {
                        Some("image") => {
                            let mime = block["mimeType"].as_str().unwrap_or("image/png");
                            let data = block["data"].as_str().ok_or("MCP image has no data")?;
                            attachments.push(json!({"type":"image_url","image_url":{"url":format!("data:{mime};base64,{data}")}}));
                        }
                        Some("audio") => {
                            let format=match block["mimeType"].as_str().unwrap_or_default() { "audio/wav"|"audio/x-wav"=>"wav","audio/mpeg"|"audio/mp3"=>"mp3",mime=>return Err(format!("MCP audio format '{mime}' cannot be sent to the configured chat adapters")) };
                            attachments.push(json!({"type":"input_audio","input_audio":{"data":block["data"],"format":format}}));
                        }
                        Some("resource") => {
                            let resource = &block["resource"];
                            let mime = resource["mimeType"]
                                .as_str()
                                .unwrap_or("application/octet-stream");
                            if let Some(blob) = resource["blob"].as_str() {
                                attachments.push(crate::attachments::content(&json!({"mime":mime,"filename":resource["uri"],"url":format!("data:{mime};base64,{blob}")}))?);
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        if !attachments.is_empty() {
            tail.push(json!({"role":"user","content":attachments}));
        }
        if chat {
            let chat_run = run.get_or_insert_with(ChatRun::default);
            if !text.is_empty() {
                chat_run.texts.push(text);
            }
            for part in &calls {
                chat_run.calls.push(json!({"id":part["callID"],"type":"function","function":{"name":part["tool"],"arguments":part["state"]["input"].to_string()}}));
            }
            if let Some(meta) = metadata
                .as_ref()
                .filter(|m| m["provider"] == "openai-chat")
                .and_then(|m| m.get("message"))
            {
                for key in ["reasoning_content", "reasoning", "reasoning_text"] {
                    if let Some(value) = meta[key].as_str() {
                        if !value.is_empty() {
                            chat_run
                                .reasoning
                                .push((key.to_string(), value.to_string()));
                        }
                    }
                }
                if let Some(list) = meta["reasoning_details"].as_array() {
                    chat_run.details.extend(list.iter().cloned());
                }
            }
            chat_run.tail.extend(tail);
        } else {
            if !text.is_empty() || !calls.is_empty() || metadata.is_some() {
                let mut value = json!({"role":"assistant","content":if text.is_empty(){Value::Null}else{json!(text)}});
                if !calls.is_empty() {
                    value["tool_calls"]=json!(calls.iter().map(|p|json!({"id":p["callID"],"type":"function","function":{"name":p["tool"],"arguments":p["state"]["input"].to_string()}})).collect::<Vec<_>>());
                }
                if let Some(metadata) = metadata {
                    value["providerMetadata"] = metadata;
                }
                if message
                    .parts
                    .iter()
                    .any(|part| part[provider::TEXT_TRANSFORMED] == true)
                {
                    value[provider::TEXT_TRANSFORMED] = json!(true);
                    provider::reconcile_text(&mut value)?;
                }
                out.push(value);
                out.extend(tail);
            }
        }
        if let Some(feedback) = message
            .info
            .extra
            .get("ocminiHookFeedback")
            .and_then(Value::as_array)
        {
            flush_chat_run(&mut out, run.take());
            out.extend(
                feedback
                    .iter()
                    .filter_map(Value::as_str)
                    .map(|text| json!({"role":"user","content":text})),
            );
        }
    }
    flush_chat_run(&mut out, run.take());
    Ok((out, summary.is_some()))
}
