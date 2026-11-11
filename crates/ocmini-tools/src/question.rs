use crate::{decode, types::OperationError};
use serde_json::Value;
#[derive(serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
pub struct Input {
    #[schemars(length(min = 1))]
    questions: Vec<Question>,
}

#[derive(serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
struct Question {
    question: String,
    header: String,
    #[serde(default)]
    multiple: bool,
    #[serde(default = "custom_allowed")]
    custom: bool,
    options: Vec<QuestionOption>,
}

#[derive(serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
struct QuestionOption {
    label: String,
    description: String,
}

fn custom_allowed() -> bool {
    true
}

pub fn parse(input: &Value) -> Result<Input, OperationError> {
    let input: Input = decode(input)?;
    if input.questions.is_empty() {
        return Err("questions must be a nonempty array".into());
    }
    Ok(input)
}
pub fn render(input: &Input, answers: &[Vec<String>]) -> String {
    input
        .questions
        .iter()
        .zip(answers)
        .map(|(q, a)| format!("{}: {}", q.question, a.join(", ")))
        .collect::<Vec<_>>()
        .join("\n")
}
