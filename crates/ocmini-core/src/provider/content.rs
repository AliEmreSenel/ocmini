//! Public content conversion shared by the Anthropic and Responses codecs.
use super::Protocol;
use serde_json::{json, Value};

pub fn blocks(value: &Value, protocol: Protocol, assistant: bool) -> Result<Vec<Value>, String> {
    let anthropic = protocol == Protocol::Anthropic;
    let text_type = if anthropic {
        "text"
    } else if assistant {
        "output_text"
    } else {
        "input_text"
    };
    if let Some(text) = value.as_str() {
        return Ok(if text.is_empty() {
            Vec::new()
        } else {
            vec![json!({"type":text_type,"text":text})]
        });
    }
    if value.is_null() {
        return Ok(Vec::new());
    }
    let parts = value
        .as_array()
        .ok_or("message content must be text or an array of parts")?;
    let mut blocks = Vec::new();
    for part in parts {
        let kind = part["type"].as_str().unwrap_or_default();
        if !anthropic && matches!(kind, "input_image" | "input_file") {
            blocks.push(part.clone());
            continue;
        }
        match kind {
            "text" | "input_text" | "output_text" => {
                let text = part["text"]
                    .as_str()
                    .ok_or("text attachment is missing text")?;
                if !anthropic || !text.is_empty() {
                    blocks.push(json!({"type":text_type,"text":text}));
                }
            }
            "image_url" | "input_image" => {
                let url = part["image_url"]["url"]
                    .as_str()
                    .or_else(|| part["image_url"].as_str())
                    .ok_or("image attachment is missing its URL")?;
                let mut block = if anthropic {
                    json!({"type":"image","source":source(url)?})
                } else {
                    json!({"type":"input_image","image_url":url})
                };
                if !anthropic {
                    if let Some(detail) = part["image_url"].get("detail") {
                        block["detail"] = detail.clone();
                    }
                }
                blocks.push(block);
            }
            "file" | "input_file" => {
                let file = part.get("file").unwrap_or(part);
                if !file.is_object() {
                    return Err("file attachment must be an object".into());
                }
                let mut block = if anthropic {
                    let url = file["file_data"]
                        .as_str()
                        .or_else(|| file["file_url"].as_str())
                        .ok_or("file attachment is missing file_data/file_url")?;
                    let mut block = json!({"type":"document","source":source(url)?});
                    if let Some(title) = file["filename"].as_str() {
                        block["title"] = json!(title);
                    }
                    block
                } else {
                    file.clone()
                };
                if !anthropic {
                    if file["file_data"].as_str().is_some_and(|url| {
                        url.starts_with("http://") || url.starts_with("https://")
                    }) {
                        block["file_url"] =
                            block.as_object_mut().unwrap().remove("file_data").unwrap();
                    }
                    block["type"] = json!("input_file");
                }
                blocks.push(block);
            }
            "image" | "document" if anthropic => blocks.push(part.clone()),
            other => {
                return Err(format!(
                    "{protocol:?} does not support message part type {other}"
                ))
            }
        }
    }
    Ok(blocks)
}

fn source(url: &str) -> Result<Value, String> {
    if let Some(data) = url.strip_prefix("data:") {
        let (header, data) = data
            .split_once(',')
            .ok_or("malformed data URL attachment")?;
        if !header.ends_with(";base64") {
            return Err("Anthropic attachments must use base64 data URLs".into());
        }
        return Ok(
            json!({"type":"base64","media_type":header.trim_end_matches(";base64"),"data":data}),
        );
    }
    if url.starts_with("http://") || url.starts_with("https://") {
        return Ok(json!({"type":"url","url":url}));
    }
    Err("unsupported attachment source; use a data URL or HTTPS URL".into())
}
