//! Concrete transcript formatting, shared between live and saved output.
use super::input::{terminal_width, Display, Draft};
use super::{truncate, PendingPrompt, RowKind, Ui, BOLD, CYAN, DIM, GREEN, RED, RESET, YELLOW};
use crate::client::{TextKind, TextUpdate};
use crate::events as event;
use crate::types::MessageWithParts;
use serde_json::Value;

/// The parts that identify a tool call, in the order shown. Newlines survive,
/// so an expanded multi-line command reads as typed.
pub(super) fn input_summary(input: &Value) -> String {
    let parts: Vec<_> = ["command", "filePath", "pattern", "url", "prompt"]
        .into_iter()
        .filter_map(|key| input[key].as_str())
        .collect();
    if parts.is_empty() {
        input.to_string()
    } else {
        parts.join(" ")
    }
}

/// The collapsed transcript line for a tool call.
pub(crate) fn summarize_input(input: &Value) -> String {
    truncate(&input_summary(input), 120)
}

/// Text one expanded tool call paints. The tool layer already caps a result at
/// 50 KiB and points at the file it saved (`tools/output.rs`), so this is that
/// same bound used as a display guard: a tool that ever handed over a 1 GiB
/// result cannot flood the transcript and the terminal's scrollback.
pub(super) const MAX_EXPANDED: usize = 50 * 1024;

/// Whole tool text for the expanded view, capped at `MAX_EXPANDED` and indented
/// two columns, so a multi-line command or result reads as one block under its
/// marker. A cut-off block names what it left out, so truncation never reads as
/// a complete answer.
pub(super) fn expand(text: &str) -> String {
    let shown: String = text.chars().take(MAX_EXPANDED).collect();
    let omitted = text.chars().count() - shown.chars().count();
    let note = if omitted == 0 {
        String::new()
    } else {
        format!("\n… {omitted} more characters not shown")
    };
    format!("{shown}{note}").replace('\n', "\n  ")
}

/// A call's brief and full command rows. The grouped view derives its count
/// and tool names from retained calls instead of printing these rows. Live
/// events and saved history use the same formatting.
pub(super) fn tool_line(part: &Value) -> (String, String) {
    let mark = format!("{CYAN}● {}{RESET}", part["tool"].as_str().unwrap_or("tool"));
    let input = input_summary(&part["state"]["input"]);
    let status = part["state"]["status"]
        .as_str()
        .filter(|status| !matches!(*status, "completed" | "error"))
        .map(|status| format!(" {DIM}· {status}{RESET}"))
        .unwrap_or_default();
    (
        format!("{mark} {DIM}{}{RESET}{status}", truncate(&input, 120)),
        format!("{mark} {DIM}{}{RESET}{status}", expand(&input)),
    )
}

/// The result row of a finished tool call, in both forms: `✓` with an output
/// preview, `✗` with the error. `None` while the call is still pending or
/// running. A failure shows in both forms, so collapsing a call can never make
/// it look like it succeeded.
pub(super) fn tool_result(part: &Value) -> Option<(String, String)> {
    let state = &part["state"];
    let (mark, text) = match state["status"].as_str() {
        // The preview is dim as a secondary line; a failure is not.
        Some("completed") => (
            format!("{GREEN}✓{RESET} {DIM}"),
            state["output"].as_str().unwrap_or_default(),
        ),
        Some("error") => (
            format!("{RED}✗{RESET} "),
            state["error"].as_str().unwrap_or("failed"),
        ),
        _ => return None,
    };
    Some((
        format!("  {mark}{}{RESET}", truncate(text, 160)),
        format!("  {mark}{}{RESET}", expand(text)),
    ))
}

/// Option label and description in the order shown to the user.
pub(super) fn option_values(options: &Value) -> Vec<(String, String)> {
    options
        .as_array()
        .map(|list| list.as_slice())
        .unwrap_or(&[][..])
        .iter()
        .map(|option| {
            (
                option["label"].as_str().unwrap_or_default().to_string(),
                option["description"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
            )
        })
        .collect()
}

pub(super) fn option_labels(options: &Value) -> Vec<String> {
    option_values(options)
        .into_iter()
        .map(|(label, _)| label)
        .collect()
}

/// The question being answered with its numbered options, so the prompt always
/// shows what a typed number refers to. Shown when the user reaches a question,
/// including a revisit; a saved answer stays visible when navigating back.
pub(super) fn render_question(
    questions: &[Value],
    answers: &[Vec<String>],
    which: usize,
) -> String {
    let Some(question) = questions.get(which) else {
        return String::new();
    };
    let mut out = format!(
        "\n{YELLOW}? {}/{}: {}{RESET}",
        which + 1,
        questions.len(),
        question["question"].as_str().unwrap_or_default(),
    );
    if let Some(header) = question["header"]
        .as_str()
        .filter(|header| !header.is_empty())
    {
        out.push_str(&format!(" {DIM}{header}{RESET}"));
    }
    if question["multiple"] == true {
        out.push_str(&format!(" {DIM}(one or more){RESET}"));
    }
    out.push('\n');
    for (index, (label, description)) in option_values(&question["options"]).iter().enumerate() {
        out.push_str(&format!(
            "  {CYAN}{}{RESET}. {label} {DIM}{description}{RESET}\n",
            index + 1
        ));
    }
    if let Some(answer) = answers.get(which).filter(|answer| !answer.is_empty()) {
        out.push_str(&format!("  {GREEN}✓{RESET} {}\n", answer.join(", ")));
    }
    out.push_str(&format!("{DIM}number(s) + Enter answers · Tab: next question · Shift+Tab: previous · Esc: dismiss{RESET}\n"));
    out
}

/// A saved answer as a fresh input line: option labels become their numbers, so
/// Enter keeps the answer unchanged and typing replaces it.
pub(super) fn answer_line(answer: &[String], labels: &[String]) -> String {
    answer
        .iter()
        .map(|answer| {
            labels
                .iter()
                .position(|label| label == answer)
                .map(|index| (index + 1).to_string())
                .unwrap_or_else(|| answer.clone())
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Transcript text for one bus event, split into runs that the Ctrl+T / Ctrl+O
/// toggles can hide independently.
fn render_interaction(ui: &mut Ui, event: &crate::types::BusEvent) -> String {
    let properties = &event.properties;
    match event.kind {
        event::EventKind::Permission(event::PermissionKind::Asked) => {
            let id = properties["id"].as_str().unwrap_or_default().to_string();
            let text = format!(
                "\n{YELLOW}⚠ permission{}: {}{RESET} {DIM}[o]nce [a]lways [r]eject{RESET}",
                properties["permission"]
                    .as_str()
                    .map(|p| format!(" {p}"))
                    .unwrap_or_default(),
                truncate(
                    properties["patterns"]
                        .as_array()
                        .map(|list| list
                            .iter()
                            .filter_map(Value::as_str)
                            .collect::<Vec<_>>()
                            .join(", "))
                        .unwrap_or_default()
                        .as_str(),
                    120
                )
            );
            ui.prompt = Some(PendingPrompt::Permission(id));
            text
        }
        event::EventKind::Question(event::QuestionKind::Asked) => {
            let id = properties["id"].as_str().unwrap_or_default().to_string();
            let questions = properties["questions"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            let answers: Vec<Vec<String>> = vec![Vec::new(); questions.len()];
            ui.prompt = Some(PendingPrompt::Question {
                id,
                questions: questions.clone(),
                answers: answers.clone(),
                current: 0,
                confirm: false,
                drafts: (0..questions.len()).map(|_| Draft::default()).collect(),
            });
            render_question(&questions, &answers, 0)
        }
        _ => String::new(),
    }
}

/// One transcript instruction for the display.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Row {
    /// Committed transcript text, tagged with the toggle that can hide it.
    Text(RowKind, String),
    /// Replace this call's state in its retained chain, including pending calls.
    Tool(ToolCall),
    Thought(ThoughtUpdate),
    ThoughtTokens {
        message_id: String,
        tokens: i64,
    },
    /// A turn ended, or the next user prompt begins a new chain.
    EndTools,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct ThoughtUpdate {
    pub id: String,
    pub message_id: String,
    pub text: String,
    pub new: bool,
    pub reset: bool,
    pub start: Option<i64>,
    pub end: Option<i64>,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct ToolCall {
    pub id: String,
    pub name: String,
    pub status: String,
    pub brief: String,
    pub full: String,
}

impl ToolCall {
    fn from_part(part: &Value) -> Self {
        let (mut brief, mut full) = tool_line(part);
        if let Some((result, full_result)) = tool_result(part) {
            brief.push_str(&format!("\n{result}"));
            full.push_str(&format!("\n{full_result}"));
        }
        Self {
            id: format!(
                "{}:{}",
                part["sessionID"].as_str().unwrap_or_default(),
                part["id"].as_str().unwrap_or_default()
            ),
            name: part["tool"].as_str().unwrap_or("tool").into(),
            status: part["state"]["status"].as_str().unwrap_or("pending").into(),
            brief,
            full,
        }
    }
}

fn render_text(update: TextUpdate, source: &Value) -> Vec<Row> {
    if update.kind == TextKind::Reasoning {
        return vec![Row::Thought(ThoughtUpdate {
            id: source["id"]
                .as_str()
                .or_else(|| source["partID"].as_str())
                .unwrap_or_default()
                .into(),
            message_id: source["messageID"].as_str().unwrap_or_default().into(),
            text: update.text,
            new: update.new,
            reset: update.reset,
            start: source["time"]["start"].as_i64(),
            end: source["time"]["end"].as_i64(),
        })];
    }
    let mut text = if update.new {
        format!("\n{BOLD}assistant{RESET}\n")
    } else {
        String::new()
    };
    if update.reset {
        text.push_str(&format!("\n{DIM}[response updated]{RESET}\n"));
    }
    text.push_str(&update.text);
    vec![Row::Text(RowKind::Plain, text)]
}

pub(super) fn render_part(ui: &mut Ui, part: &Value) -> Vec<Row> {
    if let Some(update) = ui.text.part(part) {
        return render_text(update, part);
    }
    let mut rows = Vec::new();
    if part["type"] == "step-finish" {
        if let Some(tokens) = part["tokens"]["reasoning"]
            .as_i64()
            .filter(|tokens| *tokens > 0)
        {
            rows.push(Row::ThoughtTokens {
                message_id: part["messageID"].as_str().unwrap_or_default().into(),
                tokens,
            });
        }
    }
    if part["type"] == "tool" {
        let id = part["id"].as_str().unwrap_or_default();
        let key = format!("{}:{id}", part["sessionID"].as_str().unwrap_or_default());
        if !ui.tool_done.contains(&key) {
            if tool_result(part).is_some() {
                ui.tool_done.insert(key);
            }
            rows.push(Row::Tool(ToolCall::from_part(part)));
        }
    }
    rows
}

/// Transcript updates shared by the event stream and saved history.
pub(super) fn render_event(
    ui: &mut Ui,
    event: &crate::types::BusEvent,
    session_id: &str,
) -> Vec<Row> {
    let mut rows = Vec::new();
    let properties = &event.properties;
    if crate::client::event_session(event).is_some_and(|id| id != session_id)
        && !matches!(event.kind, event::EventKind::Permission(_))
        && !matches!(event.kind, event::EventKind::Question(_))
    {
        return Vec::new();
    }
    if event.kind == event::EventKind::Part(event::PartKind::Updated) {
        return render_part(ui, &properties["part"]);
    }
    if event.kind == event::EventKind::Message(event::MessageKind::Updated)
        && properties["info"]["role"] == "user"
    {
        rows.push(Row::EndTools);
    }
    if let Some(update) = ui.text.apply(event) {
        rows.extend(render_text(update, properties));
    }
    if ui.interactions.apply(event) {
        ui.prompt = None;
        if let Some(request) = ui.interactions.current().cloned() {
            rows.push(Row::Text(RowKind::Plain, render_interaction(ui, &request)));
        }
    }
    let notice = match event.kind {
        event::EventKind::Session(event::SessionKind::Error) => {
            let message = properties["error"]["data"]["message"]
                .as_str()
                .unwrap_or("unknown error");
            Some(format!("\n{RED}error:{RESET} {message}\n"))
        }
        event::EventKind::Session(event::SessionKind::Idle) => {
            rows.push(Row::EndTools);
            Some(format!("\n{DIM}└ idle{RESET}\n"))
        }
        event::EventKind::Session(event::SessionKind::Title) => {
            match properties["state"].as_str() {
                Some("start") => Some(format!("\n{DIM}◦ generating title…{RESET}\n")),
                Some("done") => Some(format!(
                    "{DIM}◦ title: {}{RESET}\n",
                    truncate(properties["title"].as_str().unwrap_or_default(), 60)
                )),
                _ => None,
            }
        }
        _ => None,
    };
    if let Some(notice) = notice {
        rows.push(Row::Text(RowKind::Plain, notice));
    }
    rows
}

pub(super) fn render_history(display: &mut Display, ui: &mut Ui, history: &[MessageWithParts]) {
    let width = terminal_width();
    for message in history {
        ui.text.remember_role(&message.info.id, &message.info.role);
        if message.info.role != "assistant" {
            display.end_tools();
            display.output(&format!("\n{BOLD}{}{RESET}\n", message.info.role), width);
        }
        for part in &message.parts {
            match part["type"].as_str() {
                Some("text") if message.info.role != "assistant" => display.output(
                    &format!("{}\n", part["text"].as_str().unwrap_or_default()),
                    width,
                ),
                Some("file") => display.output(
                    &format!(
                        "{DIM}attachment: {}{RESET}\n",
                        part["filename"].as_str().unwrap_or("attachment")
                    ),
                    width,
                ),
                _ => {
                    display.apply(render_part(ui, part), width);
                }
            }
        }
        if let Some(error) = &message.info.error {
            display.output(
                &format!(
                    "{RED}error: {}{RESET}\n",
                    error["data"]["message"].as_str().unwrap_or("interrupted")
                ),
                width,
            );
        }
        if let Some(tokens) = message
            .info
            .tokens
            .as_ref()
            .map(|tokens| tokens.reasoning)
            .filter(|tokens| *tokens > 0)
        {
            display.apply(
                vec![Row::ThoughtTokens {
                    message_id: message.info.id.clone(),
                    tokens,
                }],
                width,
            );
        }
    }
    display.end_tools();
}

/// The session's user prompts in order, for Up/Down recall.
pub(super) fn session_prompts(messages: &[MessageWithParts]) -> impl Iterator<Item = String> + '_ {
    messages
        .iter()
        .filter(|message| message.info.role == "user")
        .filter_map(|message| {
            message
                .parts
                .iter()
                .find(|part| part["type"].as_str() == Some("text"))
                .and_then(|part| part["text"].as_str())
                .map(str::to_string)
        })
}

pub(crate) fn display_model(
    model_ref: Option<&crate::types::ModelRef>,
    config_model: &str,
) -> String {
    let display = model_ref
        .map(|model| format!("{}/{}", model.provider_id, model.model_id))
        .unwrap_or_else(|| config_model.to_string());
    if display.is_empty() {
        "unconfigured".into()
    } else {
        display
    }
}
