//! Question preparation, interaction lifetime and result formatting.
use super::{decode, failure, hook_context, ToolCtx};
use crate::events as event;
use crate::types::{OperationError, ToolOutput};
use serde_json::{json, Value};

#[derive(serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
pub(super) struct Input {
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

struct QuestionGuard {
    state: crate::engine::question::QuestionState,
    id: String,
}
impl Drop for QuestionGuard {
    fn drop(&mut self) {
        self.state.reject(&self.id);
    }
}

pub(super) fn parse(input: &Value) -> Result<Input, OperationError> {
    let input: Input = decode(input)?;
    if input.questions.is_empty() {
        return Err(failure("questions must be a nonempty array"));
    }
    Ok(input)
}

pub(super) async fn run(
    ctx: &ToolCtx,
    input: &Input,
    wire: &Value,
) -> Result<ToolOutput, OperationError> {
    let effective;
    let typed;
    let (input, wire) = if ctx.app.hooks.has(crate::tools::hooks::QUESTION_ASK_BEFORE) {
        effective = ctx
            .app
            .hooks
            .run(
                crate::tools::hooks::QUESTION_ASK_BEFORE,
                &hook_context(ctx, "question"),
                wire.clone(),
                Some(&ctx.cancel),
            )
            .await?
            .input;
        typed = parse(&effective)?;
        (&typed, &effective)
    } else {
        (input, wire)
    };
    if ctx.cancel.is_cancelled() {
        return Err(failure("Question cancelled"));
    }
    let questions = &input.questions;
    let (request, receiver) = ctx.app.questions.ask_for_tool(
        &ctx.session_id,
        wire["questions"].clone(),
        Some(json!({"messageID":ctx.message_id,"callID":ctx.call_id})),
        hook_context(ctx, "question"),
    );
    let _guard = QuestionGuard {
        state: ctx.app.questions.clone(),
        id: request.id.clone(),
    };
    ctx.app.hooks.emit(
        event::Question::Asked { request: &request },
        &ctx.app.session_context(&ctx.session_id, json!({})),
    );
    let answers = tokio::select! {
        answers = receiver => answers.map_err(|_| failure("Question cancelled"))?,
        _ = ctx.cancel.cancelled() => return Err(failure("Question cancelled")),
    }
    .ok_or_else(|| OperationError::from("User dismissed the question"))?;
    let text = questions
        .iter()
        .zip(&answers)
        .map(|(question, answer)| format!("{}: {}", question.question, answer.join(", ")))
        .collect::<Vec<_>>()
        .join("\n");
    Ok(ToolOutput {
        output: text,
        title: "User answered".into(),
        metadata: json!({"questions":wire["questions"],"answers":answers}),
    })
}
