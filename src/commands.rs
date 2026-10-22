use crate::config::Config;
use crate::types::{ModelRef, PromptRequest};
use serde_json::json;

/// Construct the same command prompt for terminal and HTTP clients.
pub fn prepare(
    config: &Config,
    name: &str,
    arguments: &str,
    mut request: PromptRequest,
) -> Result<PromptRequest, String> {
    let command = config
        .command
        .get(name)
        .ok_or_else(|| format!("Command {name} is not configured"))?;
    let template = command
        .get("template")
        .and_then(|value| value.as_str())
        .or_else(|| command.as_str())
        .ok_or_else(|| format!("Command {name} has no template"))?;
    if request.model.is_none() {
        request.model = command
            .get("model")
            .map(|value| {
                value
                    .as_str()
                    .ok_or_else(|| "Command model must be a provider/model string".to_string())
                    .and_then(ModelRef::parse)
            })
            .transpose()?;
    }
    if request.agent.is_none() {
        request.agent = command
            .get("agent")
            .map(|value| {
                value
                    .as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| "Command agent must be a string".to_string())
            })
            .transpose()?;
    }
    request.parts.retain(|part| part["type"] != "text");
    request.parts.insert(
        0,
        json!({"type":"text", "text":expand(template, arguments)}),
    );
    request.command = Some(name.to_owned());
    Ok(request)
}

pub fn expand(template: &str, arguments: &str) -> String {
    let argument_pattern = regex::Regex::new(r#"(?:\[Image\s+\d+\]|"[^"]*"|'[^']*'|[^\s"']+)"#)
        .expect("constant regex");
    let arguments_list: Vec<String> = argument_pattern
        .find_iter(arguments)
        .map(|item| item.as_str().trim_matches(['\'', '"']).to_owned())
        .collect();
    let placeholder = regex::Regex::new(r"\$(\d+)").expect("constant regex");
    let has_numbered = placeholder.is_match(template);
    let last = placeholder
        .captures_iter(template)
        .filter_map(|capture| capture[1].parse::<usize>().ok())
        .max()
        .unwrap_or(0);
    let expanded = placeholder.replace_all(template, |capture: &regex::Captures<'_>| {
        let position = capture[1].parse::<usize>().unwrap_or(0);
        let Some(index) = position.checked_sub(1) else {
            return String::new();
        };
        if index >= arguments_list.len() {
            return String::new();
        }
        if position == last {
            arguments_list[index..].join(" ")
        } else {
            arguments_list[index].clone()
        }
    });
    let expanded = expanded.replace("$ARGUMENTS", arguments);
    if !has_numbered && !template.contains("$ARGUMENTS") && !arguments.trim().is_empty() {
        format!("{}\n\n{}", expanded.trim(), arguments)
            .trim()
            .to_owned()
    } else {
        expanded.trim().to_owned()
    }
}
