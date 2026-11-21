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
    for question in &input.questions {
        if question.question.trim().is_empty() {
            return Err("Each question must have nonempty question text".into());
        }
        if question.header.trim().is_empty() {
            return Err("Each question must have a nonempty header".into());
        }
        if !question.custom && question.options.is_empty() {
            return Err("Question with custom=false must provide at least one option".into());
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_validates_question_header_and_custom_options() {
        assert!(parse(&json!({"questions": []})).is_err());
        assert!(parse(&json!({"questions": [
            {"question": "  ", "header": "Pick", "options": [{"label": "A", "description": "desc"}]}
        ]}))
        .is_err());
        assert!(parse(&json!({"questions": [
            {"question": "Which?", "header": "", "options": [{"label": "A", "description": "desc"}]}
        ]}))
        .is_err());
        assert!(parse(&json!({"questions": [
            {"question": "Which?", "header": "Pick", "custom": false, "options": []}
        ]}))
        .is_err());
        assert!(parse(&json!({"questions": [
            {"question": "Which?", "header": "Pick", "custom": true, "options": []}
        ]}))
        .is_ok());
        assert!(parse(&json!({"questions": [
            {"question": "Which?", "header": "Pick", "custom": false, "options": [{"label": "A", "description": "desc"}]}
        ]}))
        .is_ok());
    }
}
