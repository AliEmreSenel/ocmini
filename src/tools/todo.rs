//! Todo validation and persistence.
use super::{decode, failure, ToolCtx};
use crate::events as event;
use crate::types::{OperationError, ToolOutput};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

#[derive(Deserialize, schemars::JsonSchema)]
pub(super) struct Input {
    todos: Vec<Todo>,
}

#[derive(Deserialize, Serialize, schemars::JsonSchema)]
struct Todo {
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<String>,
    content: String,
    status: Status,
    priority: Priority,
    #[serde(flatten)]
    extra: Map<String, Value>,
}

#[derive(Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
enum Status {
    Pending,
    InProgress,
    Completed,
    Cancelled,
}

#[derive(Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
enum Priority {
    High,
    Medium,
    Low,
}

pub(super) fn parse(input: &Value) -> Result<Input, OperationError> {
    let input: Input = decode(input)?;
    if input
        .todos
        .iter()
        .any(|todo| todo.content.trim().is_empty())
    {
        return Err(failure("Each todo needs nonempty content"));
    }
    Ok(input)
}

pub(super) fn run(ctx: &ToolCtx, input: &Input) -> Result<ToolOutput, OperationError> {
    let todos = json!(input.todos);
    ctx.app.store.set_todos(&ctx.session_id, &todos)?;
    ctx.app.hooks.emit(
        event::Todo::Updated {
            session_id: &ctx.session_id,
            todos: &todos,
        },
        &ctx.app.session_context(&ctx.session_id, json!({})),
    );
    Ok(ToolOutput {
        output: serde_json::to_string_pretty(&todos).map_err(|e| failure(e.to_string()))?,
        title: format!("{} todos", input.todos.len()),
        metadata: json!({"todos":todos}),
    })
}
