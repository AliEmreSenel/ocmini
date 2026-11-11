use crate::{decode, types::OperationError};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
#[derive(Deserialize, schemars::JsonSchema)]
pub struct Input {
    pub todos: Vec<Todo>,
}

#[derive(Deserialize, Serialize, schemars::JsonSchema)]
pub struct Todo {
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

pub fn parse(input: &Value) -> Result<Input, OperationError> {
    let input: Input = decode(input)?;
    if input.todos.iter().any(|t| t.content.trim().is_empty()) {
        return Err("Each todo needs nonempty content".into());
    }
    Ok(input)
}
