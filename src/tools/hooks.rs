use crate::hooks::{FileHook, QuestionHook, ShellHook, SkillHook, TaskHook, ToolHook};
use crate::hooks::{Hook, HookName, Shape};
pub(crate) const TOOL_EXECUTE_BEFORE: Hook = Hook::new(
    HookName::Tool(ToolHook::ExecuteBefore),
    &[("args", Shape::Object)],
);
pub(crate) const TOOL_EXECUTE_AFTER: Hook = Hook::new(
    HookName::Tool(ToolHook::ExecuteAfter),
    &[
        ("output", Shape::Text),
        ("title", Shape::Text),
        ("metadata", Shape::Object),
    ],
)
.validate(validate_output);
pub(crate) const TOOL_CATALOG_TRANSFORM: Hook = Hook::new(
    HookName::Tool(ToolHook::CatalogTransform),
    &[("tools", Shape::Array)],
)
.validate(validate_catalog);
pub(crate) const SHELL_ENV: Hook =
    Hook::new(HookName::Shell(ShellHook::Env), &[("env", Shape::Object)]).validate(validate_env);
pub(crate) const QUESTION_ASK_BEFORE: Hook = Hook::new(
    HookName::Question(QuestionHook::AskBefore),
    &[("questions", Shape::Array)],
)
.validate(validate_questions);
pub(crate) const TASK_SPAWN_BEFORE: Hook = Hook::new(
    HookName::Task(TaskHook::SpawnBefore),
    &[
        ("prompt", Shape::Text),
        ("description", Shape::Text),
        ("subagent_type", Shape::Text),
        (
            "model",
            Shape::Record(&[("providerID", Shape::Text), ("modelID", Shape::Text)]),
        ),
    ],
);
pub(crate) const SKILL_LOAD_AFTER: Hook = Hook::new(
    HookName::Skill(SkillHook::LoadAfter),
    &[("content", Shape::Text)],
);
pub(crate) const FILE_CHANGE_BEFORE: Hook = Hook::new(
    HookName::File(FileHook::ChangeBefore),
    &[("changes", Shape::Array)],
);
pub(crate) const HOOKS: &[Hook] = &[
    TOOL_EXECUTE_BEFORE,
    TOOL_EXECUTE_AFTER,
    TOOL_CATALOG_TRANSFORM,
    SHELL_ENV,
    QUESTION_ASK_BEFORE,
    TASK_SPAWN_BEFORE,
    SKILL_LOAD_AFTER,
    FILE_CHANGE_BEFORE,
];

use serde_json::{Map, Value};
pub(crate) const PROTECTED_METADATA: &[&str] = &[
    "exit",
    "status",
    "childID",
    "sessionId",
    "sessionID",
    "model",
    "callID",
    "outputPath",
    "truncated",
    "diff",
    "patch",
    "attachments",
    "files",
    "outputBytes",
    "time",
    "usage",
    "background",
    "pid",
    "timeout",
    "logPath",
];
fn validate_output(input: &Value, set: &Map<String, Value>) -> Result<(), String> {
    if !set.is_empty()
        && (input.get("error").is_some_and(|e| !e.is_null())
            || input["status"] == "error"
            || input["success"] == false)
    {
        return Err("failed tool results are read-only".into());
    }
    if let Some(metadata) = set.get("metadata") {
        for &key in PROTECTED_METADATA {
            if input["metadata"].get(key) != metadata.get(key) {
                return Err(format!(
                    "protected runtime metadata {key} cannot be changed"
                ));
            }
        }
    }
    Ok(())
}
fn validate_env(_: &Value, set: &Map<String, Value>) -> Result<(), String> {
    if let Some(env) = set.get("env").and_then(Value::as_object) {
        for (key, value) in env {
            if key.is_empty()
                || key.starts_with("OCMINI_HOOK_")
                || key.contains(['=', '\0'])
                || !(value.is_string() || value.is_null())
                || value.as_str().is_some_and(|s| s.contains('\0'))
            {
                return Err("invalid environment replacement".into());
            }
        }
    }
    Ok(())
}

fn validate_catalog(input: &Value, set: &Map<String, Value>) -> Result<(), String> {
    let Some(tools) = set.get("tools").and_then(Value::as_array) else {
        return Ok(());
    };
    let prior = input["tools"]
        .as_array()
        .ok_or("Tool catalog must be an array")?;
    let mut names = std::collections::HashSet::new();
    for tool in tools {
        let name = tool["function"]["name"]
            .as_str()
            .ok_or("Tool catalog has no name")?;
        let original = prior
            .iter()
            .find(|t| t["function"]["name"] == name)
            .ok_or("Tool catalog hook cannot expose an unavailable tool")?;
        if !names.insert(name)
            || tool["type"] != original["type"]
            || tool["function"]["parameters"] != original["function"]["parameters"]
            || !tool["function"]["description"].is_string()
        {
            return Err(
                "Tool catalog hook changed an executable schema or duplicated a tool".into(),
            );
        }
    }
    Ok(())
}
fn validate_questions(_: &Value, set: &Map<String, Value>) -> Result<(), String> {
    if let Some(questions) = set.get("questions") {
        super::question::parse(&serde_json::json!({"questions":questions}))
            .map_err(|e| e.message)?;
    }
    Ok(())
}
