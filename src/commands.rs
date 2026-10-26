use crate::config::Config;
use crate::types::{ModelRef, PromptRequest};
use serde_json::json;

/// Construct the same command prompt for terminal and HTTP clients.
/// Structured CLI arguments preserve shell grouping; other clients supply raw text.
pub fn prepare(
    config: &Config,
    name: &str,
    arguments: &str,
    mut request: PromptRequest,
    argv: Option<&[String]>,
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
        json!({"type":"text", "text":expand_with(template, arguments, argv)}),
    );
    request.command = Some(name.to_owned());
    request.command_arguments = Some(arguments.to_owned());
    Ok(request)
}

#[cfg(test)]
pub fn expand(template: &str, arguments: &str) -> String {
    expand_with(template, arguments, None)
}
fn expand_with(template: &str, arguments: &str, argv: Option<&[String]>) -> String {
    static ARGUMENT: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let argument_pattern = ARGUMENT.get_or_init(|| {
        regex::Regex::new(r#"(?:\[Image\s+\d+\]|"[^"]*"|'[^']*'|[^\s"']+)"#)
            .expect("constant regex")
    });
    let parsed: Vec<String> = argument_pattern
        .find_iter(arguments)
        .map(|item| item.as_str().trim_matches(['\'', '"']).to_owned())
        .collect();
    let arguments_list = argv.unwrap_or(&parsed);
    static PLACEHOLDER: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let placeholder = PLACEHOLDER
        .get_or_init(|| regex::Regex::new(r"\$(?:ARGUMENTS|(\d+))").expect("constant regex"));
    let has_numbered = placeholder
        .captures_iter(template)
        .any(|c| c.get(1).is_some());
    let last = placeholder
        .captures_iter(template)
        .filter_map(|capture| capture.get(1)?.as_str().parse::<usize>().ok())
        .max()
        .unwrap_or(0);
    let expanded = placeholder.replace_all(template, |capture: &regex::Captures<'_>| {
        let Some(number) = capture.get(1) else {
            return arguments.to_owned();
        };
        let position = number.as_str().parse::<usize>().unwrap_or(0);
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
    if !has_numbered && !template.contains("$ARGUMENTS") && !arguments.trim().is_empty() {
        format!("{}\n\n{}", expanded.trim(), arguments)
            .trim()
            .to_owned()
    } else {
        expanded.trim().to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prepared_commands_preserve_raw_arguments_for_admission_hooks() {
        let cfg: Config =
            serde_json::from_value(json!({"command":{"review":{"template":"Review $ARGUMENTS"}}}))
                .unwrap();
        let arguments = "\"two words\" $literal";
        let request = prepare(&cfg, "review", arguments, PromptRequest::default(), None).unwrap();
        assert_eq!(request.command.as_deref(), Some("review"));
        assert_eq!(request.command_arguments.as_deref(), Some(arguments));
        assert_eq!(request.parts[0]["text"], format!("Review {arguments}"));
    }
    #[test]
    fn replacements_do_not_reexpand_user_text_and_argv_keeps_grouping() {
        assert_eq!(
            expand("Wrapped: $1", "$ARGUMENTS tail"),
            "Wrapped: $ARGUMENTS tail"
        );
        let args = vec!["alpha beta".into(), "gamma delta".into()];
        assert_eq!(
            expand_with("First $1. Rest $2.", &args.join(" "), Some(&args)),
            "First alpha beta. Rest gamma delta."
        );
    }
}
