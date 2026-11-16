use super::TEXT_TRANSFORMED;
use serde_json::{json, Value};
use std::borrow::Cow;

fn public_text(content: &Value) -> Result<String, String> {
    if let Some(text) = content.as_str() {
        return Ok(text.into());
    }
    if content.is_null() {
        return Ok(String::new());
    }
    let mut text = String::new();
    for part in content
        .as_array()
        .ok_or("transformed message content must be text or parts")?
    {
        if ["text", "input_text", "output_text"]
            .contains(&part["type"].as_str().unwrap_or_default())
        {
            text.push_str(
                part["text"]
                    .as_str()
                    .ok_or("transformed text part must contain text")?,
            );
        }
    }
    Ok(text)
}

/// Reconcile explicitly transformed public text into provider replay metadata.
/// Unknown blocks and signed/encrypted reasoning are retained byte-for-byte.
pub fn reconcile_text(message: &mut Value) -> Result<(), String> {
    if message[TEXT_TRANSFORMED] != true || message["role"] != "assistant" {
        return Ok(());
    }
    let text = public_text(&message["content"])?;
    if let Some(metadata) = message.get_mut("providerMetadata") {
        reconcile_metadata(metadata, &text)?;
    }
    Ok(())
}

pub(super) fn replay_metadata(message: &Value) -> Result<Cow<'_, Value>, String> {
    let mut metadata = Cow::Borrowed(&message["providerMetadata"]);
    if message[TEXT_TRANSFORMED] == true {
        reconcile_metadata(metadata.to_mut(), &public_text(&message["content"])?)?;
    }
    reconcile_tool_calls(&mut metadata, &message["tool_calls"])?;
    Ok(metadata)
}

/// Replay the effective public arguments, rather than the model's original
/// arguments, after tool.execute.before has changed the persisted tool input.
/// Calls keep their provider IDs, names, and all other replay fields.
fn reconcile_tool_calls(metadata: &mut Cow<'_, Value>, public: &Value) -> Result<(), String> {
    let Some(calls) = public.as_array() else {
        return Ok(());
    };
    let (field, kind, id_key, args_key) = match metadata["provider"].as_str().unwrap_or_default() {
        "anthropic" => ("content", "tool_use", "id", "input"),
        "openai-responses" => ("output", "function_call", "call_id", "arguments"),
        _ => return Ok(()),
    };
    let blocks = metadata[field]
        .as_array()
        .ok_or("invalid stored provider tool replay metadata")?;
    if blocks.iter().filter(|block| block["type"] == kind).count() != calls.len() {
        return Err("public tool calls do not match stored provider replay call linkage".into());
    }
    let mut seen = std::collections::HashSet::new();
    for call in calls {
        let id = call["id"]
            .as_str()
            .filter(|id| !id.is_empty())
            .ok_or("public tool call has no ID")?;
        if !seen.insert(id) {
            return Err("public tool call IDs must be unique".into());
        }
        let name = call["function"]["name"]
            .as_str()
            .filter(|name| !name.is_empty())
            .ok_or("public tool call has no name")?;
        let arguments = call["function"]["arguments"]
            .as_str()
            .ok_or("public tool call arguments must be JSON text")?;
        let blocks = metadata[field].as_array().unwrap();
        let index = blocks
            .iter()
            .position(|block| block["type"] == kind && block[id_key] == id)
            .ok_or("public tool call ID does not match stored provider replay")?;
        let block = &blocks[index];
        if block["name"] != name
            || blocks
                .iter()
                .filter(|block| block["type"] == kind && block[id_key] == id)
                .count()
                != 1
        {
            return Err("public tool call identity does not match stored provider replay".into());
        }
        if kind == "function_call" && block[args_key].as_str() == Some(arguments) {
            continue;
        }
        let effective: Value = serde_json::from_str(arguments)
            .map_err(|_| "public tool call arguments must be valid JSON".to_string())?;
        if !effective.is_object() {
            return Err("public tool call arguments must be a JSON object".into());
        }
        let changed = if kind == "function_call" {
            block[args_key]
                .as_str()
                .and_then(|args| serde_json::from_str::<Value>(args).ok())
                .as_ref()
                != Some(&effective)
        } else {
            block[args_key] != effective
        };
        if changed {
            metadata.to_mut()[field][index][args_key] = if kind == "function_call" {
                json!(arguments)
            } else {
                effective
            };
        }
    }
    Ok(())
}

fn replace_text(blocks: &mut Vec<Value>, text_type: &str, text: &str, placed: &mut bool) {
    blocks.retain_mut(|block| {
        if block["type"] != text_type {
            return true;
        }
        if *placed || text.is_empty() {
            // Responses message items retain their identity and valid content
            // shape even when a completed-text hook removes all public text.
            if text_type == "output_text" {
                block["text"] = json!("");
                return true;
            }
            return false;
        }
        block["text"] = json!(text);
        *placed = true;
        true
    });
}

fn reconcile_metadata(metadata: &mut Value, text: &str) -> Result<(), String> {
    match metadata["provider"].as_str().unwrap_or_default() {
        "anthropic" => {
            let blocks = metadata["content"]
                .as_array_mut()
                .ok_or("invalid stored Anthropic message metadata")?;
            let mut placed = false;
            replace_text(blocks, "text", text, &mut placed);
            if !placed && !text.is_empty() {
                let index = blocks
                    .iter()
                    .position(|b| b["type"] == "tool_use")
                    .unwrap_or(blocks.len());
                blocks.insert(index, json!({"type":"text","text":text}));
            }
        }
        "openai-responses" => {
            let items = metadata["output"]
                .as_array_mut()
                .ok_or("invalid stored Responses message metadata")?;
            let mut placed = false;
            for item in items.iter_mut() {
                if item["type"] == "message" && item["role"] == "assistant" {
                    if let Some(blocks) = item["content"].as_array_mut() {
                        replace_text(blocks, "output_text", text, &mut placed);
                    }
                }
            }
            if !placed && !text.is_empty() {
                let index = items
                    .iter()
                    .position(|b| b["type"] == "function_call")
                    .unwrap_or(items.len());
                items.insert(index, json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":text,"annotations":[]}]}));
            }
        }
        _ => {}
    }
    Ok(())
}
