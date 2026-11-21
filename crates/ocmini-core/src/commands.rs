use crate::config::Config;
use crate::types::{ModelRef, PromptRequest};
use serde_json::{json, Value};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LocalAction {
    Agent,
    Attach,
    Compact,
    Context,
    Copy,
    Fork,
    Help,
    Mcp,
    Model,
    New,
    Quit,
    Rename,
    Revert,
    Sandbox,
    Variant,
}

pub struct LocalCommand {
    pub name: &'static str,
    pub description: &'static str,
    pub arguments: Option<&'static str>,
    pub shortcut: Option<&'static str>,
    pub action: LocalAction,
}

pub const LOCAL_COMMANDS: &[LocalCommand] = &[
    LocalCommand {
        name: "agent",
        description: "Show or switch agent",
        arguments: Some("[name]"),
        shortcut: None,
        action: LocalAction::Agent,
    },
    LocalCommand {
        name: "attach",
        description: "Attach a file to the next prompt",
        arguments: Some("<path>"),
        shortcut: None,
        action: LocalAction::Attach,
    },
    LocalCommand {
        name: "compact",
        description: "Summarize context with optional focus",
        arguments: Some("[instructions]"),
        shortcut: None,
        action: LocalAction::Compact,
    },
    LocalCommand {
        name: "context",
        description: "Inspect approximate context usage",
        arguments: None,
        shortcut: None,
        action: LocalAction::Context,
    },
    LocalCommand {
        name: "copy",
        description: "Copy last assistant reply to clipboard",
        arguments: None,
        shortcut: None,
        action: LocalAction::Copy,
    },
    LocalCommand {
        name: "exit",
        description: "Quit",
        arguments: None,
        shortcut: None,
        action: LocalAction::Quit,
    },
    LocalCommand {
        name: "fork",
        description: "Fork the current session",
        arguments: None,
        shortcut: None,
        action: LocalAction::Fork,
    },
    LocalCommand {
        name: "help",
        description: "Show this help",
        arguments: None,
        shortcut: Some("F1"),
        action: LocalAction::Help,
    },
    LocalCommand {
        name: "mcp",
        description: "Show MCP server status",
        arguments: Some("[connect|disconnect <name>]"),
        shortcut: None,
        action: LocalAction::Mcp,
    },
    LocalCommand {
        name: "model",
        description: "Show or switch model",
        arguments: Some("[provider/model[#variant]]"),
        shortcut: None,
        action: LocalAction::Model,
    },
    LocalCommand {
        name: "new",
        description: "Start a new session",
        arguments: None,
        shortcut: None,
        action: LocalAction::New,
    },
    LocalCommand {
        name: "quit",
        description: "Quit",
        arguments: None,
        shortcut: Some("Ctrl+D"),
        action: LocalAction::Quit,
    },
    LocalCommand {
        name: "rename",
        description: "Rename the current session",
        arguments: Some("<title>"),
        shortcut: None,
        action: LocalAction::Rename,
    },
    LocalCommand {
        name: "revert",
        description: "Revert the last turn",
        arguments: None,
        shortcut: None,
        action: LocalAction::Revert,
    },
    LocalCommand {
        name: "sandbox",
        description: "Show or switch sandbox preset",
        arguments: Some("[preset]"),
        shortcut: None,
        action: LocalAction::Sandbox,
    },
    LocalCommand {
        name: "variant",
        description: "Show or switch model variant",
        arguments: Some("[name]"),
        shortcut: None,
        action: LocalAction::Variant,
    },
];

pub enum Command<'a> {
    Local(&'static LocalCommand),
    Configured {
        name: &'a str,
        definition: &'a Value,
    },
}

impl Command<'_> {
    pub fn name(&self) -> &str {
        match self {
            Self::Local(command) => command.name,
            Self::Configured { name, .. } => name,
        }
    }

    pub fn description(&self) -> &str {
        match self {
            Self::Local(command) => command.description,
            Self::Configured { definition, .. } => {
                definition["description"].as_str().unwrap_or_default()
            }
        }
    }
}

pub struct Registry<'a> {
    config: &'a Config,
}

impl<'a> Registry<'a> {
    pub fn new(config: &'a Config) -> Self {
        Self { config }
    }

    /// Prompt templates are also callable through the HTTP command endpoint.
    pub fn configured(&self) -> impl Iterator<Item = (&'a str, &'a Value)> {
        self.config
            .command
            .iter()
            .filter(|(_, definition)| template(definition).is_some())
            .map(|(name, definition)| (name.as_str(), definition))
    }

    pub fn terminal(&self) -> impl Iterator<Item = Command<'a>> {
        LOCAL_COMMANDS.iter().map(Command::Local).chain(
            self.configured()
                .filter(|(name, _)| valid_slash_name(name) && local_command(name).is_none())
                .map(|(name, definition)| Command::Configured { name, definition }),
        )
    }

    pub fn resolve_terminal<'text>(&self, text: &'text str) -> Option<(Command<'a>, &'text str)> {
        let (name, rest) = split_slash(text)?;
        let arguments = rest.trim();
        if let Some(command) = local_command(name) {
            return (arguments.is_empty() || command.arguments.is_some())
                .then_some((Command::Local(command), arguments));
        }
        if !valid_slash_name(name) {
            return None;
        }
        let (name, definition) = self.config.command.get_key_value(name)?;
        template(definition)?;
        Some((Command::Configured { name, definition }, arguments))
    }
}

fn local_command(name: &str) -> Option<&'static LocalCommand> {
    LOCAL_COMMANDS.iter().find(|command| command.name == name)
}

fn valid_slash_name(name: &str) -> bool {
    !name.is_empty() && !name.contains(|ch: char| ch.is_whitespace() || ch.is_control())
}

/// The command name and its untouched suffix, including the separator.
pub fn split_slash(text: &str) -> Option<(&str, &str)> {
    let slash = text.trim_start().strip_prefix('/')?;
    Some(slash.split_at(slash.find(char::is_whitespace).unwrap_or(slash.len())))
}

fn template(definition: &Value) -> Option<&str> {
    definition
        .get("template")
        .and_then(Value::as_str)
        .or_else(|| definition.as_str())
}

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
    let template = template(command).ok_or_else(|| format!("Command {name} has no template"))?;
    if request.model.is_none() {
        if request.variant.is_none() {
            request.variant = command["variant"].as_str().map(str::to_owned);
        }
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
    fn registry_uses_the_same_configured_commands_for_listing_and_dispatch() {
        let cfg: Config = serde_json::from_value(json!({
            "command": {
                "help": {"template": "Configured help", "description": "HTTP help"},
                "nested/review": "Review $ARGUMENTS",
                "invalid": {"description": "Missing template"}
            }
        }))
        .unwrap();
        let registry = Registry::new(&cfg);
        assert_eq!(registry.terminal().count(), LOCAL_COMMANDS.len() + 1);
        assert!(matches!(
            registry.resolve_terminal(" /help "),
            Some((
                Command::Local(LocalCommand {
                    action: LocalAction::Help,
                    ..
                }),
                ""
            ))
        ));
        assert!(matches!(
            registry.resolve_terminal("/nested/review\t\"two words\" more "),
            Some((
                Command::Configured {
                    name: "nested/review",
                    ..
                },
                "\"two words\" more"
            ))
        ));
        assert!(registry.resolve_terminal("/invalid").is_none());
        assert!(registry.resolve_terminal("Please /help").is_none());
        assert_eq!(
            crate::inventory::commands_json(&cfg),
            json!([
                {"name":"help","template":"Configured help","description":"HTTP help","source":"command","hints":[]},
                {"name":"nested/review","template":"Review $ARGUMENTS","source":"command","hints":["$ARGUMENTS"]}
            ])
        );
    }

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

#[cfg(test)]
mod argument_tests {
    use super::*;
    #[test]
    fn command_arguments_are_literal_and_last_position_collects_remainder() {
        assert_eq!(
            expand("First $1. Rest $2.", "\"alpha beta\" gamma delta"),
            "First alpha beta. Rest gamma delta."
        );
        assert_eq!(
            expand("$ARGUMENTS / $ARGUMENTS", "$& $$ $` $'"),
            "$& $$ $` $' / $& $$ $` $'"
        );
        assert_eq!(expand("Review.", "branch main"), "Review.\n\nbranch main");
        assert_eq!(expand("$2", "only"), "");
    }
}
