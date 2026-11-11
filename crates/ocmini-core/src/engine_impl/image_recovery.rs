//! A provider decoder failure must not poison every subsequent turn. Keep the
//! original media, but quarantine images actually present in the rejected request.
use super::*;

pub(super) const REJECTED: &str = "ocminiImageRejected";

pub(super) fn placeholder() -> Value {
    json!({"type":"text","text":"[Image omitted after a provider image-decoding error. Its contents were not read. Ask the user to reattach it if needed.]"})
}

pub(super) fn announce(app: &App, info: &MessageInfo) -> Result<(), OperationError> {
    let mut notice = None;
    append_delta(
        app,
        &info.session_id,
        &info.id,
        "text",
        &mut notice,
        "The provider could not decode an image. Continuing without the images in this request; please reattach any images needed.\n\n",
    )?;
    if let Some(mut notice) = notice {
        notice.part["time"]["end"] = json!(now_ms());
        crate::messages::update_part(app, &notice.part)?;
    }
    Ok(())
}

fn image_url(block: &Value) -> Option<&str> {
    match block["type"].as_str()? {
        "image_url" => block["image_url"]["url"]
            .as_str()
            .or_else(|| block["image_url"].as_str()),
        "input_image" => block["image_url"].as_str(),
        _ => None,
    }
}

fn replace_images(messages: &mut [Value], matches: impl Fn(&str) -> bool) -> bool {
    let mut changed = false;
    for message in messages {
        if let Some(blocks) = message.get_mut("content").and_then(Value::as_array_mut) {
            for block in blocks {
                if image_url(block).is_some_and(&matches) {
                    *block = placeholder();
                    changed = true;
                }
            }
        }
    }
    changed
}

pub(super) async fn recover(
    app: &App,
    info: &MessageInfo,
    messages: &mut Vec<Value>,
    mut retained: Option<&mut Vec<Value>>,
) -> Result<bool, OperationError> {
    let app = app.clone();
    let sid = info.session_id.clone();
    let through = info.parent_id.clone().ok_or("Missing step parent")?;
    let mut request = std::mem::take(messages);
    let mut recent = retained
        .as_deref_mut()
        .map(std::mem::take)
        .unwrap_or_default();
    let (request, recent, recovered) = tokio::task::spawn_blocking(move || {
        let urls: HashSet<_> = request
            .iter()
            .filter_map(|message| message["content"].as_array())
            .flatten()
            .filter_map(image_url)
            .collect();
        if urls.is_empty() {
            return Ok::<_, OperationError>((request, recent, false));
        }
        replace_images(&mut recent, |url| urls.contains(url));
        // The provider does not identify which image failed. Quarantine all
        // candidate images, preserving other files, text and tool call/result pairs.
        for message in app.store.context_messages(&sid, &through)? {
            for mut part in message.parts {
                let mut changed = false;
                if part["type"] == "file"
                    && part["mime"]
                        .as_str()
                        .is_some_and(|m| m.starts_with("image/"))
                {
                    let content = crate::attachments::content(&part)?;
                    if image_url(&content).is_some_and(|url| urls.contains(url)) {
                        part[REJECTED] = json!(true);
                        changed = true;
                    }
                }
                if let Some(blocks) = part
                    .get_mut("state")
                    .and_then(|s| s.get_mut("metadata"))
                    .and_then(|m| m.get_mut("attachments"))
                    .and_then(Value::as_array_mut)
                {
                    for block in blocks {
                        let url = match block["type"].as_str() {
                            Some("image") => Some(format!(
                                "data:{};base64,{}",
                                block["mimeType"].as_str().unwrap_or("image/png"),
                                block["data"].as_str().unwrap_or_default()
                            )),
                            Some("resource")
                                if block["resource"]["mimeType"]
                                    .as_str()
                                    .is_some_and(|m| m.starts_with("image/")) =>
                            {
                                Some(format!(
                                    "data:{};base64,{}",
                                    block["resource"]["mimeType"].as_str().unwrap(),
                                    block["resource"]["blob"].as_str().unwrap_or_default()
                                ))
                            }
                            _ => None,
                        };
                        if url.as_deref().is_some_and(|url| urls.contains(url)) {
                            block[REJECTED] = json!(true);
                            changed = true;
                        }
                    }
                }
                if let Some(recent) = part
                    .get_mut("compactionRecent")
                    .and_then(Value::as_array_mut)
                {
                    changed |= replace_images(recent, |url| urls.contains(url));
                }
                if changed {
                    crate::messages::update_part(&app, &part)?;
                }
            }
        }
        // Release borrowed URLs before mutating their source request.
        drop(urls);
        replace_images(&mut request, |_| true);
        Ok((request, recent, true))
    })
    .await
    .map_err(|error| OperationError::from(error.to_string()))??;
    *messages = request;
    if let Some(retained) = retained {
        *retained = recent;
    }
    Ok(recovered)
}
