// Terminal transcript and input over the shared session engine.
use crate::config::ScrollMode;
use crate::engine::{self, App, EngineApi};
use crate::engine_impl::Engine;
use crate::types::{MessageWithParts, PromptRequest};
use base64::Engine as _;
use crossterm::{
    event::{
        DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event, EventStream, KeyCode, KeyEventKind, KeyModifiers, MouseEventKind,
    },
    terminal::{self, EnterAlternateScreen, LeaveAlternateScreen},
};
use futures::{poll, StreamExt};
use serde_json::Value;
mod input;
use input::{
    clean_input, input_prefix, page_move, terminal_width, Display, Draft, History, KillRing,
};
use std::collections::HashMap;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::task::Poll;

struct RawMode {
    fullscreen: bool,
}

impl RawMode {
    /// Full-screen mode owns the terminal: the alternate screen leaves the
    /// native scrollback intact, and mouse capture routes the wheel here.
    fn enter(fullscreen: bool) -> std::io::Result<Self> {
        terminal::enable_raw_mode()?;
        let guard = Self { fullscreen };
        let mut out = std::io::stdout();
        if fullscreen {
            crossterm::execute!(out, EnterAlternateScreen, EnableMouseCapture)?;
        }
        crossterm::execute!(out, EnableBracketedPaste)?;
        Ok(guard)
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        let mut out = std::io::stdout();
        let _ = crossterm::execute!(out, DisableBracketedPaste);
        if self.fullscreen {
            let _ = crossterm::execute!(out, DisableMouseCapture, LeaveAlternateScreen);
        }
        let _ = terminal::disable_raw_mode();
        std::println!();
    }
}

// Raw terminals do not translate LF to CRLF for transcript output. Exported so
// the session picker shares one CRLF-safe printing path.
#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => { std::print!("{}", format!($($arg)*).replace('\n', "\r\n")) };
}
#[macro_export]
macro_rules! println {
    () => { $crate::print!("\n") };
    ($($arg:tt)*) => { $crate::print!("{}\n", format!($($arg)*)) };
}

pub(crate) const RESET: &str = "\x1b[0m";
pub(crate) const DIM: &str = "\x1b[2m";
pub(crate) const BOLD: &str = "\x1b[1m";
const GREEN: &str = "\x1b[32m";
const RED: &str = "\x1b[31m";
pub(crate) const YELLOW: &str = "\x1b[33m";
const CYAN: &str = "\x1b[36m";

pub(crate) fn truncate(text: &str, max: usize) -> String {
    let mut chars = text.chars();
    let mut out: String = chars.by_ref().take(max).collect();
    if chars.next().is_some() {
        out.push('…');
    }
    out.replace('\n', " ")
}

/// Which runtime toggle hides a transcript row. Everything else is always
/// painted, so a toggle can never swallow a prompt, an error or a tool's name.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum RowKind {
    Plain,
    /// Chain of thought: the `◦ thinking` header and every reasoning delta.
    Thinking,
    /// A tool call's collapsed form: the `● tool` line and a truncated result.
    ToolBrief,
    /// The same call with its full command and its full result. Exactly one of
    /// the two forms is painted, so Ctrl+O swaps views instead of hiding work.
    ToolFull,
}

/// Transcript noise the reader can hide while chatting, with Ctrl+T and Ctrl+O.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Visibility {
    pub thinking: bool,
    /// Tool calls: collapsed one-liners, or the full command and full result.
    pub tools_expanded: bool,
}

impl Default for Visibility {
    fn default() -> Self {
        Self {
            thinking: true,
            tools_expanded: false,
        }
    }
}

impl Visibility {
    fn visible(&self, kind: RowKind) -> bool {
        match kind {
            RowKind::Plain => true,
            RowKind::Thinking => self.thinking,
            RowKind::ToolBrief => !self.tools_expanded,
            RowKind::ToolFull => self.tools_expanded,
        }
    }

    /// Compact status for the current transcript visibility.
    fn hints(&self) -> String {
        format!(
            " · thinking {} · tools {}",
            if self.thinking { "on" } else { "off" },
            if self.tools_expanded {
                "expanded"
            } else {
                "collapsed"
            }
        )
    }
}

/// Transcript text split by the kind that hides it, so the renderer filters per
/// row instead of per call: one event can emit a visible tool line and a
/// hideable result line.
#[derive(Default)]
struct Transcript(Vec<(RowKind, String)>);

impl Transcript {
    fn text(&mut self, kind: RowKind) -> &mut String {
        if self.0.last().is_none_or(|(last, _)| *last != kind) {
            self.0.push((kind, String::new()));
        }
        &mut self.0.last_mut().unwrap().1
    }
}

struct Ui {
    part_kind: HashMap<String, &'static str>, // partID -> text | reasoning
    headered: std::collections::HashSet<String>, // part ids that printed their header
    tool_done: std::collections::HashSet<String>,
    // Prompt interaction routed from the terminal: lines when idle, keys while a request is pending.
    prompt: Option<PendingPrompt>,
    /// Live transcript toggles, mirrored into the display on every change.
    visibility: Visibility,
}

enum PendingPrompt {
    Permission(String),
    Question {
        id: String,
        // One entry per question: the full question object as asked (question,
        // header, multiple, options), so it can be re-shown while navigating.
        questions: Vec<Value>,
        // One entry per question, in the same order. Empty until answered, so
        // earlier answers survive while the user goes back to change them.
        answers: Vec<Vec<String>>,
        // The question whose answer is being typed.
        current: usize,
        // The answered summary is on screen: Enter sends the answers, Tab
        // steps back into them, any other key edits the current question.
        confirm: bool,
        // Each question keeps its own draft, so switching questions (Tab /
        // Shift+Tab, or Backspace on an empty line) never loses a half-typed
        // answer, while the prompt draft waits untouched underneath the set.
        drafts: Vec<Draft>,
    },
}

impl Drop for Ui {
    fn drop(&mut self) {
        use std::io::Write;
        let _ = std::io::stdout().flush();
    }
}

fn prompt_header(model: &str, visibility: Visibility) -> String {
    format!(
        "{DIM}┌ /help · F1 help · model {model}{}{RESET}",
        visibility.hints()
    )
}

/// Local help is shown only on request, never sent to the model.
fn help_text(fullscreen: bool) -> String {
    let scroll = if fullscreen {
        "  PgUp/PgDn              Scroll transcript\n  Ctrl+Home/End          Jump to top/bottom\n  Mouse wheel            Scroll transcript\n"
    } else {
        "  PgUp/PgDn              Move through draft\n  Mouse wheel            Terminal scrollback\n"
    };
    format!(
        "\n{BOLD}Help{RESET}\n\
  /help or F1            Show this help\n\
  /quit or Ctrl+D        Quit\n\
  Enter                  Send\n\
  Alt+Enter              Insert newline\n\
  Up/Down                History on empty draft; move in text\n\
  Left/Right, Home/End   Move caret\n\
  Alt+Left/Right         Move by word (also Ctrl+Left/Right)\n\
  Alt+B/F                Move by word\n\
  Shift + motion         Select text\n\
  Ctrl+A/E               Start/end of line\n\
  Ctrl+K/W               Cut to line end / previous word\n\
  Ctrl+Y                 Yank\n\
  Ctrl+Z                 Undo\n\
  Paste                  Insert all lines\n\
  Ctrl+V/Ctrl+Shift+V    Attach clipboard image\n\
  Alt+Backspace          Remove latest image\n\
  Ctrl+U                 Clear draft and images\n\
  Ctrl+C                 Abort turn\n\
  Ctrl+L                 Clear screen\n\
  Ctrl+R                 Switch session\n\
  Ctrl+T                 Toggle thinking\n\
  Ctrl+O                 Expand/collapse tools\n\
{scroll}\
  Questions: Tab/Shift+Tab switch; Enter answers/confirms; Esc dismisses\n"
    )
}

/// The parts that identify a tool call, in the order shown. Newlines survive,
/// so an expanded multi-line command reads as typed.
fn input_summary(input: &Value) -> String {
    let mut parts = Vec::new();
    if let Some(command) = input["command"].as_str() {
        parts.push(command.to_string());
    }
    if let Some(path) = input["filePath"].as_str() {
        parts.push(path.to_string());
    }
    if let Some(pattern) = input["pattern"].as_str() {
        parts.push(pattern.to_string());
    }
    if let Some(url) = input["url"].as_str() {
        parts.push(url.to_string());
    }
    if let Some(prompt) = input["prompt"].as_str() {
        parts.push(prompt.to_string());
    }
    if parts.is_empty() {
        parts.push(input.to_string());
    }
    parts.join(" ")
}

/// The collapsed transcript line for a tool call.
pub(crate) fn summarize_input(input: &Value) -> String {
    truncate(&input_summary(input), 120)
}

/// Text one expanded tool call paints. The tool layer already caps a result at
/// 50 KiB and points at the file it saved (`tools/output.rs`), so this is that
/// same bound used as a display guard: a tool that ever handed over a 1 GiB
/// result cannot flood the transcript and the terminal's scrollback.
const MAX_EXPANDED: usize = 50 * 1024;

/// Whole tool text for the expanded view, capped at `MAX_EXPANDED` and indented
/// two columns, so a multi-line command or result reads as one block under its
/// marker. A cut-off block names what it left out, so truncation never reads as
/// a complete answer.
fn expand(text: &str) -> String {
    let shown: String = text.chars().take(MAX_EXPANDED).collect();
    let omitted = text.chars().count() - shown.chars().count();
    let note = if omitted == 0 {
        String::new()
    } else {
        format!("\n… {omitted} more characters not shown")
    };
    format!("{shown}{note}").replace('\n', "\n  ")
}

/// A tool call's opening row in the two forms `Ctrl+O` switches between:
/// `(collapsed, expanded)` — the input summary truncated to one line, and the
/// whole command. Shared by the live event stream and the resumed history, so a
/// session reads the same after `--continue` as it did while running, and a
/// saved call expands just like a live one.
fn tool_line(part: &Value) -> (String, String) {
    let mark = format!("{CYAN}● {}{RESET}", part["tool"].as_str().unwrap_or("tool"));
    let input = input_summary(&part["state"]["input"]);
    (
        format!("{mark} {DIM}{}{RESET}", truncate(&input, 120)),
        format!("{mark} {DIM}{}{RESET}", expand(&input)),
    )
}

/// The result row of a finished tool call, in both forms: `✓` with an output
/// preview, `✗` with the error. `None` while the call is still pending or
/// running. A failure shows in both forms, so collapsing a call can never make
/// it look like it succeeded.
fn tool_result(part: &Value) -> Option<(String, String)> {
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

/// One saved tool call in both forms: its row plus its result, or the status it
/// stopped at when it never finished.
fn tool_block(part: &Value) -> (String, String) {
    let (line, full_line) = tool_line(part);
    match tool_result(part) {
        Some((result, full_result)) => (
            format!("\n{line}\n{result}\n"),
            format!("\n{full_line}\n{full_result}\n"),
        ),
        // A call that never finished has no result, so both forms show the
        // status it stopped at.
        None => {
            let status = part["state"]["status"].as_str().unwrap_or_default();
            (
                format!("\n{line} {DIM}· {status}{RESET}\n"),
                format!("\n{full_line} {DIM}· {status}{RESET}\n"),
            )
        }
    }
}

/// Option label and description in the order shown to the user.
fn option_values(options: &Value) -> Vec<(String, String)> {
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

fn option_labels(options: &Value) -> Vec<String> {
    option_values(options)
        .into_iter()
        .map(|(label, _)| label)
        .collect()
}

/// The question being answered with its numbered options, so the prompt always
/// shows what a typed number refers to. Shown when the user reaches a question,
/// including a revisit; a saved answer stays visible when navigating back.
fn render_question(questions: &[Value], answers: &[Vec<String>], which: usize) -> String {
    let question = &questions[which];
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
fn answer_line(answer: &[String], labels: &[String]) -> String {
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
fn render_event(
    ui: &mut Ui,
    event: &crate::types::BusEvent,
    session_id: &str,
) -> Vec<(RowKind, String)> {
    use std::fmt::Write as _;
    let mut transcript = Transcript::default();
    macro_rules! print {
        ($kind:expr, $($arg:tt)*) => {{ let _ = write!(transcript.text($kind), $($arg)*); }};
    }
    macro_rules! println {
        ($kind:expr, $($arg:tt)*) => {{ let _ = writeln!(transcript.text($kind), $($arg)*); }};
    }
    let properties = &event.properties;
    if properties["sessionID"]
        .as_str()
        .or_else(|| properties["part"]["sessionID"].as_str())
        .is_some_and(|id| id != session_id)
    {
        return Vec::new();
    }
    match event.r#type.as_str() {
        "message.part.updated" => {
            let part = &properties["part"];
            let part_id = part["id"].as_str().unwrap_or_default().to_string();
            match part["type"].as_str() {
                Some("text") | Some("reasoning") => {
                    let kind = if part["type"] == "text" {
                        "text"
                    } else {
                        "reasoning"
                    };
                    let is_new = ui.part_kind.insert(part_id.clone(), kind).is_none();
                    if is_new {
                        if kind == "reasoning" {
                            println!(RowKind::Thinking, "\n{DIM}◦ thinking{RESET}");
                        } else {
                            println!(RowKind::Plain, "\n{BOLD}assistant{RESET}");
                        }
                    }
                }
                Some("tool") => {
                    // Both forms are emitted and Ctrl+O picks which one is
                    // painted, so expanding shows a call that already ran.
                    let (line, full_line) = tool_line(part);
                    if ui.headered.insert(format!("{part_id}:start")) {
                        println!(RowKind::ToolBrief, "\n{line}");
                        println!(RowKind::ToolFull, "\n{full_line}");
                    }
                    if let Some((result, full_result)) = tool_result(part) {
                        if ui.tool_done.insert(part_id) {
                            println!(RowKind::ToolBrief, "{result}");
                            println!(RowKind::ToolFull, "{full_result}");
                        }
                    }
                }
                _ => {}
            }
        }
        "message.part.delta" => {
            let part_id = properties["partID"].as_str().unwrap_or_default();
            let delta = properties["delta"].as_str().unwrap_or_default();
            match ui.part_kind.get(part_id).copied() {
                Some("text") => print!(RowKind::Plain, "{delta}"),
                Some("reasoning") => print!(RowKind::Thinking, "{DIM}{delta}{RESET}"),
                _ => {}
            }
        }
        "permission.asked" => {
            let id = properties["id"].as_str().unwrap_or_default().to_string();
            println!(
                RowKind::Plain,
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
        }
        "question.asked" => {
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
            // Questions appear one at a time: the first is shown now, each
            // next when the user reaches it.
            if !questions.is_empty() {
                print!(
                    RowKind::Plain,
                    "{}",
                    render_question(&questions, &answers, 0)
                );
            }
        }
        "session.error" => {
            let message = properties["error"]["data"]["message"]
                .as_str()
                .unwrap_or("unknown error");
            println!(RowKind::Plain, "\n{RED}error:{RESET} {message}");
        }
        "session.idle" => {
            println!(RowKind::Plain, "\n{DIM}└ idle{RESET}");
        }
        "session.title" => match properties["state"].as_str() {
            Some("start") => println!(RowKind::Plain, "\n{DIM}◦ generating title…{RESET}"),
            Some("done") => println!(
                RowKind::Plain,
                "{DIM}◦ title: {}{RESET}",
                truncate(properties["title"].as_str().unwrap_or_default(), 60)
            ),
            _ => {}
        },
        _ => {}
    }
    transcript.0
}

/// One answer line for one question. A line of option numbers ("2", "1 3" for
/// multiple) answers with those options' labels: the question contract and the
/// model both expect labels, so the displayed index must not reach the tool
/// output. Any other line - prose, an out-of-range number - is one custom
/// answer, kept exactly as typed.
fn parse_answer(line: &str, labels: &[String]) -> Vec<String> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }
    let numbers: Option<Vec<usize>> = trimmed
        .split_ascii_whitespace()
        .map(|token| {
            token
                .parse::<usize>()
                .ok()
                .filter(|number| (1..=labels.len()).contains(number))
        })
        .collect();
    match numbers {
        Some(numbers) => numbers
            .iter()
            .map(|number| labels[number - 1].clone())
            .collect(),
        None => vec![trimmed.to_string()],
    }
}

/// The draft the keyboard edits: a pending question's own answer draft, so
/// answering gets the full input editor while the prompt draft waits untouched
/// and returns when the question settles.
fn active_draft<'a>(prompt: &'a mut Option<PendingPrompt>, draft: &'a mut Draft) -> &'a mut Draft {
    match prompt {
        Some(PendingPrompt::Question {
            drafts, current, ..
        }) => drafts.get_mut(*current).unwrap_or(draft),
        _ => draft,
    }
}

/// Send the pending question's answer line. Returns `(finished, transcript)`:
/// `finished` clears the pending question, and `transcript` is text to commit
/// above the input - the answered question's checkmark, the next question when
/// advancing, the confirmation screen, or a warning. A line that answers
/// nothing stays in the answer draft.
fn answer_question(state: &mut PendingPrompt) -> (bool, String) {
    let PendingPrompt::Question {
        questions,
        answers,
        current,
        confirm,
        drafts,
        ..
    } = state
    else {
        return (false, String::new());
    };
    let Some(question) = questions.get(*current) else {
        return (true, String::new());
    };
    let labels = option_labels(&question["options"]);
    let choices = parse_answer(&drafts[*current].text, &labels);
    if choices.is_empty() {
        return (false, String::new());
    }
    if question["multiple"] != true && choices.len() > 1 {
        // The reply contract takes one answer per single-choice question;
        // accepting a list here would stall the tool.
        return (false, format!("{YELLOW}one answer only{RESET}\n"));
    }
    drafts[*current] = Draft::default();
    answers[*current] = choices;
    let mut out = format!("  {GREEN}✓{RESET} {}\n", answers[*current].join(", "));
    // The last answer opens the confirmation screen; Enter sends from there.
    if answers.iter().all(|answer| !answer.is_empty()) {
        *confirm = true;
        return (false, render_confirmation(questions, answers));
    }
    // Enter walks the questions in order, wrapping to the first; an answered
    // one reopens prefilled, so going back to change an answer returns
    // forward through every question, not across the answered ones.
    *current = (*current + 1) % questions.len();
    let labels = option_labels(&questions[*current]["options"]);
    let prefill = answer_line(&answers[*current], &labels);
    if !prefill.is_empty() {
        drafts[*current].insert_str(&prefill);
    }
    out.push_str(&render_question(questions, answers, *current));
    (false, out)
}

/// The confirmation screen, shown when the last answer lands, before the
/// answers are sent: every question with its answer, opencode's review step.
/// Enter sends, Tab steps back to edit, Esc dismisses.
fn render_confirmation(questions: &[Value], answers: &[Vec<String>]) -> String {
    let mut out = String::from(&format!("\n{BOLD}Confirm answers{RESET}\n")[..]);
    for (index, question) in questions.iter().enumerate() {
        out.push_str(&format!(
            "  {GREEN}✓{RESET} {CYAN}{}{RESET}. {}\n      {BOLD}{}{RESET}\n",
            index + 1,
            question["question"].as_str().unwrap_or_default(),
            answers
                .get(index)
                .map(|answer| answer.join(", "))
                .unwrap_or_default(),
        ));
    }
    out.push_str(&format!(
        "{DIM}Enter: send · Tab: edit answers · Esc: dismiss{RESET}\n"
    ));
    out
}

/// Send the confirmed answers. True when the question prompt is finished.
fn confirm_question(app: &App, state: &mut PendingPrompt) -> bool {
    let PendingPrompt::Question { id, answers, .. } = state else {
        return false;
    };
    app.questions.reply(id, std::mem::take(answers));
    true
}

/// Move to the next (positive) or previous (negative) question, wrapping.
/// Each question keeps its own draft, so a half-typed answer survives the
/// trip; an empty draft on an answered question reopens prefilled with its
/// saved answer. Returns the question text to print, if the prompt is a
/// question with more than one entry.
fn switch_question(state: &mut PendingPrompt, delta: isize) -> Option<String> {
    let PendingPrompt::Question {
        questions,
        answers,
        current,
        confirm,
        drafts,
        ..
    } = state
    else {
        return None;
    };
    if questions.is_empty() {
        return None;
    }
    *confirm = false;
    *current = (*current as isize + delta).rem_euclid(questions.len() as isize) as usize;
    let draft = drafts.get_mut(*current)?;
    if draft.text.is_empty() {
        let labels = option_labels(&questions[*current]["options"]);
        draft.insert_str(&answer_line(&answers[*current], &labels));
    }
    Some(render_question(questions, answers, *current))
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Key {
    Char(char),
    Enter,
    /// Insert a newline into the draft instead of sending (Alt+Enter, Ctrl+J).
    NewLine,
    Backspace,
    Delete,
    Esc,
    CtrlC,
    CtrlD,
    CtrlL,
    CtrlR,
    Help,
    CtrlV,
    CtrlU,
    RemoveAttachment,
    /// Move by N rows; page keys report a full page.
    Up(usize),
    Down(usize),
    Left,
    Right,
    /// Word-wise motion with Alt or Ctrl held.
    WordLeft,
    WordRight,
    /// Motion with Shift held extends the selection.
    SelectLeft,
    SelectRight,
    SelectHome,
    SelectEnd,
    SelectWordLeft,
    SelectWordRight,
    Undo,
    /// Ctrl+K: caret to line end. Ctrl+W: word behind the caret. Ctrl+Y: yank.
    KillLine,
    KillWord,
    Yank,
    Home,
    End,
    Tab,
    /// Shift+Tab: the previous question (Tab is the next-question motion).
    PreviousQuestion,
    /// Ctrl+T / Ctrl+O: hide chain of thought and tool results while chatting.
    ToggleThinking,
    ToggleTools,
    /// Transcript scrolling. Full-screen mode scrolls its own transcript with
    /// these; native mode keeps the terminal's wheel and pages the draft.
    PageUp,
    PageDown,
    ScrollTop,
    ScrollBottom,
    /// Mouse wheel: rows per notch.
    ScrollUp(usize),
    ScrollDown(usize),
    Paste(String),
    Other,
}

/// Rows a page key moves, matching the picker's viewport height.
const PAGE: usize = 10;
/// Transcript rows one wheel notch scrolls in full-screen mode.
const WHEEL: usize = 3;

pub(crate) fn decode_event(event: Event) -> Key {
    match event {
        Event::Paste(text) => Key::Paste(text.replace("\r\n", "\n").replace('\r', "\n")),
        Event::Key(key) if key.kind != KeyEventKind::Release => match (key.code, key.modifiers) {
            (KeyCode::Char('c'), KeyModifiers::CONTROL) => Key::CtrlC,
            (KeyCode::Char('d'), KeyModifiers::CONTROL) => Key::CtrlD,
            (KeyCode::Char('l'), KeyModifiers::CONTROL) => Key::CtrlL,
            (KeyCode::Char('r'), KeyModifiers::CONTROL) => Key::CtrlR,
            (KeyCode::F(1), _) => Key::Help,
            (KeyCode::Char('v' | 'V'), modifiers)
                if modifiers.contains(KeyModifiers::CONTROL)
                    && !modifiers.intersects(KeyModifiers::ALT | KeyModifiers::SUPER) =>
            {
                Key::CtrlV
            }
            (KeyCode::Char('u'), KeyModifiers::CONTROL) => Key::CtrlU,
            (KeyCode::Char('z'), KeyModifiers::CONTROL) => Key::Undo,
            (KeyCode::Char('k'), KeyModifiers::CONTROL) => Key::KillLine,
            (KeyCode::Char('w'), KeyModifiers::CONTROL) => Key::KillWord,
            (KeyCode::Char('y'), KeyModifiers::CONTROL) => Key::Yank,
            (KeyCode::Char('t'), KeyModifiers::CONTROL) => Key::ToggleThinking,
            (KeyCode::Char('o'), KeyModifiers::CONTROL) => Key::ToggleTools,
            // Readline aliases for the line-start / line-end motion.
            (KeyCode::Char('a'), KeyModifiers::CONTROL) => Key::Home,
            (KeyCode::Char('e'), KeyModifiers::CONTROL) => Key::End,
            (KeyCode::Char('b'), KeyModifiers::ALT) => Key::WordLeft,
            (KeyCode::Char('f'), KeyModifiers::ALT) => Key::WordRight,
            (KeyCode::Backspace, modifiers) if modifiers.contains(KeyModifiers::ALT) => {
                Key::RemoveAttachment
            }
            // Plain Enter sends; Alt+Enter inserts a newline. Ctrl+J is reported by
            // crossterm as a bare Enter, so it is not a distinct binding here.
            (KeyCode::Enter, modifiers) if modifiers.contains(KeyModifiers::ALT) => Key::NewLine,
            (KeyCode::Enter, _) => Key::Enter,
            (KeyCode::Backspace, _) => Key::Backspace,
            (KeyCode::Delete, _) => Key::Delete,
            (KeyCode::Esc, _) => Key::Esc,
            (KeyCode::Up, modifiers) => Key::Up(if modifiers.contains(KeyModifiers::SHIFT) {
                PAGE
            } else {
                1
            }),
            (KeyCode::Down, modifiers) => Key::Down(if modifiers.contains(KeyModifiers::SHIFT) {
                PAGE
            } else {
                1
            }),
            (KeyCode::PageUp, _) => Key::PageUp,
            (KeyCode::PageDown, _) => Key::PageDown,
            (KeyCode::Left, modifiers)
                if modifiers.contains(KeyModifiers::ALT | KeyModifiers::SHIFT) =>
            {
                Key::SelectWordLeft
            }
            (KeyCode::Right, modifiers)
                if modifiers.contains(KeyModifiers::ALT | KeyModifiers::SHIFT) =>
            {
                Key::SelectWordRight
            }
            (KeyCode::Left, modifiers)
                if modifiers.intersects(KeyModifiers::ALT | KeyModifiers::CONTROL) =>
            {
                Key::WordLeft
            }
            (KeyCode::Right, modifiers)
                if modifiers.intersects(KeyModifiers::ALT | KeyModifiers::CONTROL) =>
            {
                Key::WordRight
            }
            (KeyCode::Left, modifiers) if modifiers.contains(KeyModifiers::SHIFT) => {
                Key::SelectLeft
            }
            (KeyCode::Right, modifiers) if modifiers.contains(KeyModifiers::SHIFT) => {
                Key::SelectRight
            }
            (KeyCode::Left, _) => Key::Left,
            (KeyCode::Right, _) => Key::Right,
            (KeyCode::Home, modifiers) if modifiers.contains(KeyModifiers::CONTROL) => {
                Key::ScrollTop
            }
            (KeyCode::End, modifiers) if modifiers.contains(KeyModifiers::CONTROL) => {
                Key::ScrollBottom
            }
            (KeyCode::Home, modifiers) if modifiers.contains(KeyModifiers::SHIFT) => {
                Key::SelectHome
            }
            (KeyCode::End, modifiers) if modifiers.contains(KeyModifiers::SHIFT) => Key::SelectEnd,
            (KeyCode::Home, _) => Key::Home,
            (KeyCode::End, _) => Key::End,
            (KeyCode::Tab, _) => Key::Tab,
            (KeyCode::BackTab, _) => Key::PreviousQuestion,
            (KeyCode::Char(ch), modifiers)
                if !modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                Key::Char(ch)
            }
            _ => Key::Other,
        },
        Event::Mouse(mouse) => match mouse.kind {
            MouseEventKind::ScrollUp => Key::ScrollUp(WHEEL),
            MouseEventKind::ScrollDown => Key::ScrollDown(WHEEL),
            _ => Key::Other,
        },
        _ => Key::Other,
    }
}

/// A pending image. Clipboard images use data URLs so removing a draft leaves
/// no temporary files and saved sessions can still read the image later.
struct Attachment {
    filename: String,
    url: String,
    mime: &'static str,
}

const MAX_ATTACHMENT: u64 = 20 * 1024 * 1024;

fn file_attachment(path: &Path) -> Result<Attachment, String> {
    let path =
        std::fs::canonicalize(path).map_err(|e| format!("Cannot read {}: {e}", path.display()))?;
    let size = std::fs::metadata(&path).map_err(|e| e.to_string())?.len();
    if size > MAX_ATTACHMENT {
        return Err("Image exceeds the 20 MiB attachment limit".into());
    }
    let mime = crate::tools::media_type(&std::fs::read(&path).map_err(|e| e.to_string())?)
        .filter(|mime| mime.starts_with("image/"))
        .ok_or_else(|| format!("{} is not a PNG, JPEG, GIF or WebP image", path.display()))?;
    Ok(Attachment {
        filename: path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "image".into()),
        url: reqwest::Url::from_file_path(&path)
            .map_err(|_| format!("{} is not an absolute path", path.display()))?
            .to_string(),
        mime,
    })
}

/// Retain supported clipboard images in the message itself, without temp files.
fn image_attachment(bytes: Vec<u8>) -> Option<Attachment> {
    let mime = crate::tools::media_type(&bytes).filter(|mime| mime.starts_with("image/"))?;
    if bytes.len() as u64 > MAX_ATTACHMENT {
        return None;
    }
    let ext = mime.rsplit('/').next().unwrap_or("png");
    Some(Attachment {
        filename: format!("clipboard.{ext}"),
        url: format!(
            "data:{mime};base64,{}",
            base64::engine::general_purpose::STANDARD.encode(bytes)
        ),
        mime,
    })
}

/// A paste that is only a quoted, file: or ~/ path to an image file. Many
/// terminals paste dragged or copied files as paths, so those become
/// attachments; anything else stays plain text.
fn pasted_image_path(text: &str) -> Option<PathBuf> {
    let trimmed = text.trim();
    if trimmed.contains(['\n', '\r']) {
        return None;
    }
    let unquoted = trimmed
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .or_else(|| {
            trimmed
                .strip_prefix('\'')
                .and_then(|s| s.strip_suffix('\''))
        })
        .unwrap_or(trimmed);
    let path = if unquoted.starts_with("file:") {
        reqwest::Url::parse(unquoted).ok()?.to_file_path().ok()?
    } else {
        PathBuf::from(unquoted.replace("\\ ", " "))
    };
    let expanded = match path.to_str()?.strip_prefix("~/") {
        Some(rest) => std::env::var_os("HOME").map(|home| PathBuf::from(home).join(rest))?,
        None => path,
    };
    is_image_file(&expanded).then_some(expanded)
}

// Sniff only the header: pasting a path to a huge file must not read it all.
fn is_image_file(path: &Path) -> bool {
    use std::io::Read;
    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };
    if !file.metadata().is_ok_and(|m| m.is_file()) {
        return false;
    }
    let mut head = [0u8; 16];
    let read = file.read(&mut head).unwrap_or(0);
    crate::tools::media_type(&head[..read]).is_some_and(|mime| mime.starts_with("image/"))
}

async fn paste_clipboard(draft: &mut Draft, display: &mut Display) {
    let result = tokio::time::timeout(std::time::Duration::from_secs(3), clipboard_image()).await;
    let message = match result {
        Ok(Ok(Some(attachment))) => {
            draft.attachments.push(attachment);
            return;
        }
        Ok(Ok(None)) => {
            "Clipboard has no supported image. Paste an image file path to attach it.".into()
        }
        Ok(Err(message)) => message,
        Err(_) => "Clipboard read timed out. Paste an image file path to attach it.".into(),
    };
    display.output(&format!("\n{YELLOW}{message}{RESET}\n"), terminal_width());
}

#[cfg(target_os = "linux")]
async fn clipboard_image() -> Result<Option<Attachment>, String> {
    use tokio::process::Command;
    let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();
    let x11 = std::env::var_os("DISPLAY").is_some();
    if !wayland && !x11 {
        return Err(
            if std::env::var_os("SSH_CONNECTION").is_some() || std::env::var_os("SSH_TTY").is_some()
            {
                "SSH does not forward your local image clipboard. Copy the image to this host (for example with scp), then paste its remote path; X11 clipboard forwarding also works with a forwarded DISPLAY and xclip.".into()
            } else {
                "No desktop clipboard is available. Paste an image file path, or use wl-paste (Wayland) / xclip (X11) in a desktop session.".into()
            },
        );
    }
    let mut available = false;
    for (program, enabled) in [("wl-paste", wayland), ("xclip", x11)] {
        if !enabled {
            continue;
        }
        for mime in [
            "image/png",
            "image/jpeg",
            "image/webp",
            "image/gif",
            "text/uri-list",
        ] {
            let mut command = Command::new(program);
            if program == "wl-paste" {
                // --no-newline keeps binary payloads byte-exact.
                command.args(["--type", mime, "--no-newline"]);
            } else {
                command.args(["-selection", "clipboard", "-t", mime, "-o"]);
            }
            match command.kill_on_drop(true).output().await {
                Ok(out) => {
                    available = true;
                    if out.status.success() {
                        if mime == "text/uri-list" {
                            if let Ok(text) = std::str::from_utf8(&out.stdout) {
                                let paths: Vec<_> = text
                                    .lines()
                                    .filter(|line| {
                                        !line.starts_with('#') && !line.trim().is_empty()
                                    })
                                    .collect();
                                if let [path] = paths.as_slice() {
                                    if let Some(path) = pasted_image_path(path) {
                                        return file_attachment(&path).map(Some);
                                    }
                                }
                            }
                        } else if let Some(attachment) = image_attachment(out.stdout) {
                            return Ok(Some(attachment));
                        }
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
                Err(error) => {
                    return Err(format!("Cannot read clipboard using {program}: {error}"))
                }
            }
        }
    }
    if !available {
        return Err("Image paste needs wl-paste (Wayland) or xclip (X11); neither helper could be started. You can also paste an image file path.".into());
    }
    Ok(None)
}

#[cfg(target_os = "macos")]
async fn clipboard_image() -> Result<Option<Attachment>, String> {
    use tokio::process::Command;
    let directory = tempfile::tempdir().map_err(|e| e.to_string())?;
    let file = directory.path().join("clipboard.png");
    let out = Command::new("osascript")
        .args([
            "-e",
            &format!(
                "set f to open for access POSIX file \"{}\" with write permission",
                file.display()
            ),
            "-e",
            "write (the clipboard as «class PNGf») to f starting at 1",
            "-e",
            "close access f",
        ])
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|e| format!("Cannot run osascript: {e}"))?;
    let bytes = std::fs::read(&file).ok();
    let _ = std::fs::remove_file(&file);
    if !out.status.success() {
        return Ok(None);
    }
    Ok(bytes.and_then(image_attachment))
}

#[cfg(windows)]
async fn clipboard_image() -> Result<Option<Attachment>, String> {
    use tokio::process::Command;
    let directory = tempfile::tempdir().map_err(|e| e.to_string())?;
    let file = directory.path().join("clipboard.png");
    let script = format!(
        "Add-Type -AssemblyName System.Windows.Forms; if ($i = [System.Windows.Forms.Clipboard]::GetImage()) {{ $i.Save('{}') }}",
        file.display()
    );
    let out = Command::new("powershell")
        .args(["-NoProfile", "-Command", &script])
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|e| format!("Cannot run powershell: {e}"))?;
    let bytes = std::fs::read(&file).ok();
    let _ = std::fs::remove_file(&file);
    if !out.status.success() {
        return Ok(None);
    }
    Ok(bytes.and_then(image_attachment))
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
async fn clipboard_image() -> Result<Option<Attachment>, String> {
    Err("Image paste is not supported on this platform".into())
}

#[cfg(unix)]
fn shutdown_signal() -> std::io::Result<impl std::future::Future<Output = ()>> {
    use tokio::signal::unix::{signal, SignalKind};
    let mut hangup = signal(SignalKind::hangup())?;
    let mut interrupt = signal(SignalKind::interrupt())?;
    let mut quit = signal(SignalKind::quit())?;
    let mut terminate = signal(SignalKind::terminate())?;
    Ok(async move {
        tokio::select! {
            _ = hangup.recv() => {},
            _ = interrupt.recv() => {},
            _ = quit.recv() => {},
            _ = terminate.recv() => {},
        }
    })
}

#[cfg(not(unix))]
fn shutdown_signal() -> std::io::Result<impl std::future::Future<Output = ()>> {
    Ok(async {
        let _ = tokio::signal::ctrl_c().await;
    })
}

fn render_history(display: &mut Display, history: &[MessageWithParts]) {
    let width = terminal_width();
    for message in history {
        display.output(&format!("\n{BOLD}{}{RESET}\n", message.info.role), width);
        for part in &message.parts {
            match part["type"].as_str() {
                Some("text") => display.output(
                    &format!("{}\n", part["text"].as_str().unwrap_or_default()),
                    width,
                ),
                // A resumed call takes part in the Ctrl+O toggle, so it
                // expands exactly like the live transcript did.
                Some("tool") => {
                    let (line, full_line) = tool_block(part);
                    display.output_as(&line, width, RowKind::ToolBrief);
                    display.output_as(&full_line, width, RowKind::ToolFull);
                }
                Some("file") => display.output(
                    &format!(
                        "{DIM}attachment: {}{RESET}\n",
                        part["filename"]
                            .as_str()
                            .or_else(|| part["url"].as_str())
                            .unwrap_or("file")
                    ),
                    width,
                ),
                _ => {}
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
    }
}

/// The session's user prompts in order, for Up/Down recall.
fn session_prompts(messages: &[MessageWithParts]) -> impl Iterator<Item = String> + '_ {
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

/// Interactive resume path for `--resume`: needs a real terminal because it
/// enters raw mode and reads keys before the chat loop exists.
async fn pick_session(app: &App, directory: Option<&str>) -> Result<crate::types::Session, String> {
    if !std::io::stdout().is_terminal() {
        return Err("Session selection needs a terminal; resume by ID with --session ID".into());
    }
    let scope = directory.map(crate::sessions::directory).transpose()?;
    let _raw =
        RawMode::enter(false).map_err(|error| format!("Cannot initialize terminal: {error}"))?;
    let mut keys = EventStream::new();
    crate::picker::select(&app.store, scope.as_deref(), &mut keys)
        .await?
        .ok_or_else(|| "No session selected".to_string())
}

pub async fn run(args: crate::TuiArgs) -> Result<(), String> {
    let app = App::new()?;
    let session = if args.resume {
        pick_session(&app, args.directory.as_deref()).await?
    } else {
        crate::sessions::open_for_app(
            &app,
            args.directory.as_deref(),
            args.session.as_deref(),
            args.continue_last,
        )
        .await?
    };
    crate::sessions::reject_correlated_reentry(&session.id)?;
    let mut app = app.for_directory(&session.directory)?;
    let mut session = session;
    let engine = Engine::new();
    let history = app.store.messages(&session.id)?;
    let previous = history
        .iter()
        .rev()
        .find(|message| message.info.role == "user");
    let model_override = args.model.clone();
    let mut model_ref = args
        .model
        .or_else(|| previous.and_then(|message| message.info.model.clone()));
    let mut agent = previous.and_then(|message| message.info.agent.clone());
    let mut variant = previous.and_then(|message| message.info.variant.clone());
    let mut model_display = display_model(model_ref.as_ref(), &app.config.lock().unwrap().model);

    let fullscreen = matches!(
        app.config.lock().unwrap().ui.scroll_mode,
        ScrollMode::Fullscreen
    );
    let shutdown =
        shutdown_signal().map_err(|error| format!("Cannot listen for shutdown: {error}"))?;
    let _raw = RawMode::enter(fullscreen)
        .map_err(|error| format!("Cannot initialize terminal: {error}"))?;
    let mut events = engine::bus().subscribe();
    let mut keys = EventStream::new();
    tokio::pin!(shutdown);

    let mut draft = Draft::default();
    let mut killring = KillRing::default();
    let mut prompt_history = History::default();
    prompt_history.load(session_prompts(&history));
    let mut display = Display::new(fullscreen);
    let mut ui = Ui {
        part_kind: HashMap::new(),
        headered: std::collections::HashSet::new(),
        tool_done: std::collections::HashSet::new(),
        prompt: None,
        visibility: Visibility::default(),
    };
    let width = terminal_width();
    display.output(
        &format!(
            "{BOLD}ocmini{RESET} {DIM}{}{RESET}\n",
            crate::OPENCODE_COMPAT_VERSION
        ),
        width,
    );
    display.output(
        &format!(
            "{DIM}session {} · {}{RESET}\n",
            session.id, session.directory
        ),
        width,
    );
    render_history(&mut display, &history);
    display.draw(&ui, &draft, terminal_width(), &model_display);

    loop {
        tokio::select! {
            _ = &mut shutdown => break,
            event = keys.next() => {
                let Some(event) = event else { break; };
                let event = event.map_err(|error| error.to_string())?;
                display.erase();
                if matches!(event, Event::Resize(_, _)) && !fullscreen {
                    display.finish_line();
                }
                let key = decode_event(event);
                let pending_permission = matches!(ui.prompt, Some(PendingPrompt::Permission(_)));
                let pending_question = matches!(ui.prompt, Some(PendingPrompt::Question { .. }));
                let confirming =
                    matches!(ui.prompt, Some(PendingPrompt::Question { confirm: true, .. }));
                match key {
                    Key::CtrlD => break,
                    Key::CtrlC if pending_permission => { if let Some(PendingPrompt::Permission(id)) = ui.prompt.take() { app.perms.reply(&id, "reject"); } }
                    Key::CtrlC => { engine.abort(&session.id); display.output(&format!("\n{YELLOW}⏹ aborted{RESET}\n"), terminal_width()); }
                    Key::CtrlL => display.clear_screen(),
                    Key::Help => {
                        display.finish_line();
                        display.output(&help_text(fullscreen), terminal_width());
                        display.scroll_bottom();
                    }
                    key @ (Key::ToggleThinking | Key::ToggleTools) => {
                        // Runtime transcript toggles. Full-screen mode repaints
                        // from the rows it kept, so hidden text comes back;
                        // native mode cannot rewrite the terminal's scrollback,
                        // so the change applies to output from here on.
                        match key {
                            Key::ToggleThinking => ui.visibility.thinking = !ui.visibility.thinking,
                            _ => ui.visibility.tools_expanded = !ui.visibility.tools_expanded,
                        }
                        display.set_visibility(ui.visibility);
                    }
                    Key::CtrlU => {
                        if !pending_permission {
                            active_draft(&mut ui.prompt, &mut draft).clear();
                        }
                    }
                    Key::RemoveAttachment if !pending_permission && !pending_question => { draft.attachments.pop(); }
                    Key::CtrlV if !pending_permission && !pending_question => {
                        paste_clipboard(&mut draft, &mut display).await;
                    }
                    Key::CtrlR if !pending_permission && !pending_question => {
                        display.finish_line();
                        let picked = crate::picker::select(&app.store, None, &mut keys).await?;
                        // The picker reads stdin through this same stream; keys typed
                        // while it was open are queued here and must not reach the prompt.
                        while let Poll::Ready(Some(_)) = poll!(keys.next()) {}
                        match picked {
                            None => {},
                            Some(picked) => {
                                match crate::sessions::open(
                                    &app.store,
                                    None,
                                    Some(&picked.id),
                                    false,
                                ) {
                                    Err(error) => {
                                        display.output(
                                            &format!("{RED}error:{RESET} {error}\n"),
                                            terminal_width(),
                                        );
                                    }
                                    Ok(resumed) => {
                                        crate::sessions::reject_correlated_reentry(&resumed.id)?;
                                        engine.abort(&session.id);
                                        app = app.for_directory(&resumed.directory)?;
                                        session = resumed;
                                        let history = app.store.messages(&session.id)?;
                                        let previous = history.iter().rev().find(|message| {
                                            message.info.role == "user"
                                        });
                                        model_ref = model_override.clone().or_else(|| {
                                            previous.and_then(|message| message.info.model.clone())
                                        });
                                        agent = previous
                                            .and_then(|message| message.info.agent.clone());
                                        variant = previous
                                            .and_then(|message| message.info.variant.clone());
                                        model_display = display_model(
                                            model_ref.as_ref(),
                                            &app.config.lock().unwrap().model,
                                        );
                                        draft = Draft::default();
                                        prompt_history.load(session_prompts(&history));
                                        display.output(
                                            &format!(
                                                "\n{BOLD}resumed{RESET} {DIM}{} · {}{RESET}\n",
                                                session.id, session.directory
                                            ),
                                            terminal_width(),
                                        );
                                        render_history(&mut display, &history);
                                    }
                                }
                            }
                        }
                    }
                    Key::Char(ch) if pending_permission => {
                        if let Some(PendingPrompt::Permission(id)) = ui.prompt.take() {
                            match ch {
                                'o' | 'O' => { app.perms.reply(&id, "once"); }
                                'a' | 'A' => { app.perms.reply(&id, "always"); }
                                _ => { app.perms.reply(&id, "reject"); }
                            }
                        }
                    }
                    Key::Enter if confirming => {
                        // The confirmation screen is up; Enter sends.
                        if let Some(state) = ui.prompt.as_mut() {
                            if confirm_question(&app, state) {
                                ui.prompt = None;
                            }
                        }
                    }
                    Key::Enter if pending_question => {
                        // The answer's draft is the active input; the prompt
                        // draft stays untouched underneath it.
                        if let Some(state) = ui.prompt.as_mut() {
                            let (finished, out) = answer_question(state);
                            if !out.is_empty() {
                                display.output(&out, terminal_width());
                            }
                            if finished {
                                ui.prompt = None;
                            }
                        }
                    }
                    Key::Backspace if pending_question => {
                        // Backspace edits the answer; on the empty line it
                        // returns to the previous question. Shift+Tab does
                        // that from any draft, keeping the typed text.
                        if let Some(PendingPrompt::Question {
                            questions,
                            answers,
                            current,
                            drafts,
                            ..
                        }) = ui.prompt.as_mut()
                        {
                            if drafts
                                .get(*current)
                                .is_some_and(|draft| !draft.text.is_empty())
                            {
                                drafts[*current].backspace();
                            } else if *current > 0 {
                                *current -= 1;
                                let labels = option_labels(&questions[*current]["options"]);
                                drafts[*current]
                                    .insert_str(&answer_line(&answers[*current], &labels));
                                let out = render_question(questions, answers, *current);
                                display.output(&out, terminal_width());
                            }
                        }
                    }
                    Key::Tab if pending_question => {
                        // Tab walks the questions forward; each keeps its own
                        // draft, so a half-typed answer survives the trip.
                        if let Some(state) = ui.prompt.as_mut() {
                            if let Some(out) = switch_question(state, 1) {
                                display.output(&out, terminal_width());
                            }
                        }
                    }
                    Key::PreviousQuestion if pending_question => {
                        if let Some(state) = ui.prompt.as_mut() {
                            if let Some(out) = switch_question(state, -1) {
                                display.output(&out, terminal_width());
                            }
                        }
                    }
                    Key::Esc if pending_question => {
                        if let Some(PendingPrompt::Question { id, .. }) = ui.prompt.take() {
                            app.questions.reject(&id);
                        }
                    }
                    _key if confirming => {
                        // Any other key steps out of the confirmation screen
                        // back to editing; the keystroke is swallowed so a
                        // stray tap can never send.
                        if let Some(PendingPrompt::Question {
                            questions,
                            answers,
                            current,
                            confirm,
                            ..
                        }) = ui.prompt.as_mut()
                        {
                            *confirm = false;
                            let out = render_question(questions, answers, *current);
                            display.output(&out, terminal_width());
                        }
                    }
                    Key::Paste(_) if pending_permission => {}
                    Key::Paste(text) => {
                        if text.is_empty() {
                            if !pending_question {
                                paste_clipboard(&mut draft, &mut display).await;
                            }
                        } else if let Some(path) = pasted_image_path(&text).filter(|_| !pending_question) {
                            match file_attachment(&path) {
                                Ok(attachment) => draft.attachments.push(attachment),
                                Err(message) => display.output(&format!("\n{YELLOW}{message}{RESET}\n"), terminal_width()),
                            }
                        } else {
                            // Answers are text-only: attachments stay with the prompt.
                            active_draft(&mut ui.prompt, &mut draft).insert_str(&clean_input(&text));
                        }
                    }
                    Key::Enter if pending_permission => {},
                    Key::Enter if draft.text.trim() == "/help" => {
                        // Keep attachments: asking for help must not discard them.
                        draft.clear_text();
                        display.finish_line();
                        display.output(&help_text(fullscreen), terminal_width());
                        display.scroll_bottom();
                    }
                    Key::Enter => {
                        display.finish_line();
                        let (text, files) = draft.take();
                        if !text.is_empty() {
                            display.output(
                                &format!("{BOLD}›{RESET} {text}\n"),
                                terminal_width(),
                            );
                        }
                        for file in &files {
                            display.output(
                                &format!(
                                    "{DIM}● attached: {}{RESET}\n",
                                    clean_input(&file.filename)
                                ),
                                terminal_width(),
                            );
                        }
                        if text.trim() == "/quit" { break; }
                        prompt_history.remember(text.clone());
                        let mut parts: Vec<Value> = Vec::new();
                        if !text.trim().is_empty() {
                            parts.push(serde_json::json!({"type": "text", "text": text}));
                        }
                        for attachment in files {
                            parts.push(serde_json::json!({
                                "type": "file",
                                "url": attachment.url,
                                "mime": attachment.mime,
                                "filename": attachment.filename,
                            }));
                        }
                        if !parts.is_empty() {
                            let request = PromptRequest {
                                message_id: Some(crate::types::hex_id("msg_")),
                                model: model_ref.clone(),
                                agent: agent.clone(),
                                variant: variant.clone(),
                                parts,
                                ..Default::default()
                            };
                            if let Err(error) = engine.admit(app.clone(), session.id.clone(), request).await {
                                display.output(
                                    &format!("{RED}error:{RESET} {error}\n"),
                                    terminal_width(),
                                );
                            }
                        }
                    }
                    Key::Backspace if !pending_permission => { draft.backspace(); }
                    Key::Char(ch) => { active_draft(&mut ui.prompt, &mut draft).insert(ch); }
                    Key::NewLine if !pending_permission => {
                        active_draft(&mut ui.prompt, &mut draft).insert('\n');
                    }
                    // While a question is pending the active input is its
                    // answer draft, so every motion, selection, kill, yank,
                    // paste and undo works on the answer.
                    Key::Delete if !pending_permission => {
                        active_draft(&mut ui.prompt, &mut draft).delete()
                    }
                    Key::Undo if !pending_permission => {
                        active_draft(&mut ui.prompt, &mut draft).undo()
                    }
                    Key::KillLine if !pending_permission => {
                        if let Some(killed) = active_draft(&mut ui.prompt, &mut draft).kill_line() {
                            killring.push(killed);
                        }
                    }
                    Key::KillWord if !pending_permission => {
                        if let Some(killed) = active_draft(&mut ui.prompt, &mut draft).kill_word() {
                            killring.push(killed);
                        }
                    }
                    Key::Yank if !pending_permission => {
                        if let Some(text) = killring.top() {
                            active_draft(&mut ui.prompt, &mut draft).insert_str(text);
                        }
                    }
                    Key::Left => active_draft(&mut ui.prompt, &mut draft).left(),
                    Key::Right => active_draft(&mut ui.prompt, &mut draft).right(),
                    Key::WordLeft => active_draft(&mut ui.prompt, &mut draft).word_left(),
                    Key::WordRight => active_draft(&mut ui.prompt, &mut draft).word_right(),
                    Key::Home => active_draft(&mut ui.prompt, &mut draft).home(),
                    Key::End => active_draft(&mut ui.prompt, &mut draft).end(),
                    Key::SelectLeft => active_draft(&mut ui.prompt, &mut draft).select_left(),
                    Key::SelectRight => active_draft(&mut ui.prompt, &mut draft).select_right(),
                    Key::SelectHome => active_draft(&mut ui.prompt, &mut draft).select_home(),
                    Key::SelectEnd => active_draft(&mut ui.prompt, &mut draft).select_end(),
                    Key::SelectWordLeft => active_draft(&mut ui.prompt, &mut draft).select_word_left(),
                    Key::SelectWordRight => active_draft(&mut ui.prompt, &mut draft).select_word_right(),
                    Key::Up(n) if !pending_permission => {
                        // Row math uses the same prefix the input row draws.
                        let prefix = input_prefix(&ui, &draft);
                        // Up on an empty draft recalls previous prompts;
                        // otherwise it moves the caret across display rows.
                        page_move(
                            &mut ui.prompt,
                            &mut draft,
                            &mut prompt_history,
                            &prefix,
                            -(n as isize),
                        );
                    }
                    Key::Down(n) if !pending_permission => {
                        let prefix = input_prefix(&ui, &draft);
                        page_move(
                            &mut ui.prompt,
                            &mut draft,
                            &mut prompt_history,
                            &prefix,
                            n as isize,
                        );
                    }
                    key @ (Key::PageUp
                    | Key::PageDown
                    | Key::ScrollTop
                    | Key::ScrollBottom
                    | Key::ScrollUp(_)
                    | Key::ScrollDown(_)) => {
                        // Full-screen mode owns transcript scrolling, so these
                        // keys scroll even while a prompt is pending. Native
                        // mode leaves the wheel to the terminal, so they keep
                        // moving the draft as before.
                        if fullscreen {
                            let page = display.page() as isize;
                            match key {
                                Key::PageUp => display.scroll_by(-page),
                                Key::PageDown => display.scroll_by(page),
                                Key::ScrollUp(rows) => display.scroll_by(-(rows as isize)),
                                Key::ScrollDown(rows) => display.scroll_by(rows as isize),
                                Key::ScrollTop => display.scroll_top(),
                                _ => display.scroll_bottom(),
                            }
                        } else if !pending_permission {
                            let prefix = input_prefix(&ui, &draft);
                            match key {
                                Key::PageUp | Key::ScrollUp(_) => page_move(
                                    &mut ui.prompt,
                                    &mut draft,
                                    &mut prompt_history,
                                    &prefix,
                                    -(PAGE as isize),
                                ),
                                Key::PageDown | Key::ScrollDown(_) => page_move(
                                    &mut ui.prompt,
                                    &mut draft,
                                    &mut prompt_history,
                                    &prefix,
                                    PAGE as isize,
                                ),
                                Key::ScrollTop => {
                                    active_draft(&mut ui.prompt, &mut draft).home();
                                }
                                _ => active_draft(&mut ui.prompt, &mut draft).end(),
                            }
                        }
                    }
                    Key::Esc if pending_permission => { if let Some(PendingPrompt::Permission(id)) = ui.prompt.take() { app.perms.reply(&id, "reject"); } }
                    _ => {}
                }
                display.draw(&ui, &draft, terminal_width(), &model_display);
            }
            event = events.recv() => {
                display.erase();
                match event {
                    Ok(event) => {
                        for (kind, text) in render_event(&mut ui, &event, &session.id) {
                            display.output_as(&text, terminal_width(), kind);
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => display.output(&format!("\n{YELLOW}⚠ some events were skipped{RESET}\n"), terminal_width()),
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
                display.draw(&ui, &draft, terminal_width(), &model_display);
            }
        }
    }
    display.erase();
    display.finish_line();
    display.leave_screen();
    engine.abort_and_wait(&session.id).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        answer_line, decode_event, expand, file_attachment, help_text, image_attachment,
        parse_answer, pasted_image_path, prompt_header, render_confirmation, render_event,
        render_question, switch_question, tool_block, tool_line, tool_result, Draft, Key,
        PendingPrompt, RowKind, Ui, Visibility, MAX_EXPANDED, WHEEL,
    };
    use crossterm::event::{
        Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEvent, MouseEventKind,
    };
    use std::collections::{HashMap, HashSet};

    pub(super) const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR";

    /// Strip SGR color codes so assertions can match plain text.
    fn plain(text: &str) -> String {
        let mut out = String::new();
        let mut rest = text;
        while let Some(index) = rest.find("\x1b[") {
            out.push_str(&rest[..index]);
            rest = &rest[index + 2..];
            // Skip the escape sequence's terminating byte.
            if let Some(end) = rest.find(|ch: char| ('@'..='~').contains(&ch)) {
                rest = &rest[end + 1..];
            }
        }
        out + rest
    }

    #[test]
    fn paste_shortcuts_accept_shift_and_uppercase_without_inserting_text() {
        for modifiers in [
            KeyModifiers::CONTROL,
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        ] {
            for ch in ['v', 'V'] {
                assert_eq!(
                    decode_event(Event::Key(KeyEvent::new(KeyCode::Char(ch), modifiers))),
                    Key::CtrlV
                );
            }
        }
        assert_eq!(
            decode_event(Event::Key(KeyEvent::new(
                KeyCode::Backspace,
                KeyModifiers::ALT
            ))),
            Key::RemoveAttachment
        );
        assert_eq!(
            decode_event(Event::Key(KeyEvent::new(
                KeyCode::Char('u'),
                KeyModifiers::CONTROL
            ))),
            Key::CtrlU
        );
        let release = KeyEvent::new_with_kind(
            KeyCode::Char('v'),
            KeyModifiers::CONTROL,
            KeyEventKind::Release,
        );
        assert_eq!(decode_event(Event::Key(release)), Key::Other);
        assert_eq!(
            decode_event(Event::Paste("a\r\nb\rc".into())),
            Key::Paste("a\nb\nc".into())
        );
    }

    #[test]
    fn editing_keys_decode_with_modifiers() {
        for (code, modifiers, key) in [
            (KeyCode::Char('z'), KeyModifiers::CONTROL, Key::Undo),
            (KeyCode::Char('k'), KeyModifiers::CONTROL, Key::KillLine),
            (KeyCode::Char('w'), KeyModifiers::CONTROL, Key::KillWord),
            (KeyCode::Char('y'), KeyModifiers::CONTROL, Key::Yank),
            (KeyCode::Char('a'), KeyModifiers::CONTROL, Key::Home),
            (KeyCode::Char('e'), KeyModifiers::CONTROL, Key::End),
            (KeyCode::Char('f'), KeyModifiers::ALT, Key::WordRight),
            (KeyCode::Char('b'), KeyModifiers::ALT, Key::WordLeft),
            (KeyCode::Left, KeyModifiers::SHIFT, Key::SelectLeft),
            (KeyCode::Right, KeyModifiers::SHIFT, Key::SelectRight),
            (KeyCode::Tab, KeyModifiers::NONE, Key::Tab),
            (KeyCode::BackTab, KeyModifiers::SHIFT, Key::PreviousQuestion),
            (KeyCode::Home, KeyModifiers::SHIFT, Key::SelectHome),
            (KeyCode::End, KeyModifiers::SHIFT, Key::SelectEnd),
            (
                KeyCode::Left,
                KeyModifiers::ALT | KeyModifiers::SHIFT,
                Key::SelectWordLeft,
            ),
            (
                KeyCode::Right,
                KeyModifiers::ALT | KeyModifiers::SHIFT,
                Key::SelectWordRight,
            ),
            (KeyCode::Enter, KeyModifiers::ALT, Key::NewLine),
            (KeyCode::Enter, KeyModifiers::empty(), Key::Enter),
            (KeyCode::Left, KeyModifiers::empty(), Key::Left),
            (KeyCode::Delete, KeyModifiers::empty(), Key::Delete),
        ] {
            assert_eq!(
                decode_event(Event::Key(KeyEvent::new(code, modifiers))),
                key
            );
        }
    }

    #[test]
    fn transcript_scroll_keys_decode_apart_from_draft_motion() {
        for (code, modifiers, key) in [
            (KeyCode::PageUp, KeyModifiers::empty(), Key::PageUp),
            (KeyCode::PageDown, KeyModifiers::empty(), Key::PageDown),
            (KeyCode::Home, KeyModifiers::CONTROL, Key::ScrollTop),
            (KeyCode::End, KeyModifiers::CONTROL, Key::ScrollBottom),
            // Plain Home/End stay draft motion; Ctrl+A/E keep their aliases.
            (KeyCode::Home, KeyModifiers::empty(), Key::Home),
            (KeyCode::End, KeyModifiers::empty(), Key::End),
            (KeyCode::Char('a'), KeyModifiers::CONTROL, Key::Home),
        ] {
            assert_eq!(
                decode_event(Event::Key(KeyEvent::new(code, modifiers))),
                key
            );
        }
        for (kind, key) in [
            (MouseEventKind::ScrollUp, Key::ScrollUp(WHEEL)),
            (MouseEventKind::ScrollDown, Key::ScrollDown(WHEEL)),
            (MouseEventKind::Moved, Key::Other),
        ] {
            assert_eq!(
                decode_event(Event::Mouse(MouseEvent {
                    kind,
                    column: 0,
                    row: 0,
                    modifiers: KeyModifiers::empty(),
                })),
                key
            );
        }
    }

    #[test]
    fn typed_numbers_answer_with_their_option_labels() {
        let labels = ["session-rename-notes".to_string(), "scratch".to_string()];
        assert_eq!(parse_answer("1", &labels), ["session-rename-notes"]);
        assert_eq!(
            parse_answer("1  2", &labels),
            ["session-rename-notes", "scratch"]
        );
        // Prose is one custom answer, exactly as typed - never one per word.
        assert_eq!(parse_answer("keep-it-brief", &labels), ["keep-it-brief"]);
        assert_eq!(
            parse_answer("no im just testing", &labels),
            ["no im just testing"]
        );
        // Numbers without an option make the whole line a custom answer.
        assert_eq!(parse_answer("0 9", &labels), ["0 9"]);
        assert_eq!(parse_answer("", &labels), Vec::<String>::new());
    }

    #[test]
    fn switching_questions_keeps_each_draft() {
        use serde_json::json;
        let mut state = PendingPrompt::Question {
            id: "q".into(),
            questions: vec![
                json!({"question": "A", "options": [{"label": "One", "description": ""}]}),
                json!({"question": "B", "options": [{"label": "Two", "description": ""}]}),
            ],
            answers: vec![Vec::new(), vec!["Two".to_string()]],
            current: 0,
            confirm: false,
            drafts: vec![Draft::default(), Draft::default()],
        };
        // A half-typed answer survives the round trip to the next question.
        if let PendingPrompt::Question {
            drafts, current, ..
        } = &mut state
        {
            drafts[*current].insert_str("half-typed");
        }
        assert!(switch_question(&mut state, 1).is_some());
        assert!(switch_question(&mut state, -1).is_some());
        if let PendingPrompt::Question {
            current, drafts, ..
        } = &state
        {
            assert_eq!(*current, 0);
            assert_eq!(drafts[0].text, "half-typed");
        }
        // An answered question reopens prefilled with its saved answer.
        switch_question(&mut state, 1);
        if let PendingPrompt::Question {
            current, drafts, ..
        } = &state
        {
            assert_eq!((*current, drafts[1].text.as_str()), (1, "1"));
        }
        // Tab steps back out of the confirmation screen.
        if let PendingPrompt::Question { confirm, .. } = &mut state {
            *confirm = true;
        }
        assert!(switch_question(&mut state, 1).is_some());
        if let PendingPrompt::Question { confirm, .. } = &state {
            assert!(!confirm);
        }
    }

    #[test]
    fn the_confirmation_screen_lists_every_answer() {
        use serde_json::json;
        let questions = vec![
            json!({"question": "Pick a model", "options": []}),
            json!({"question": "Run checks?", "options": []}),
        ];
        let answers = vec![
            vec!["A".to_string()],
            vec!["no im just testing".to_string()],
        ];
        let screen = plain(&render_confirmation(&questions, &answers));
        assert!(screen.contains("Confirm answers"));
        assert!(screen.contains("1. Pick a model"));
        assert!(screen.contains("2. Run checks?"));
        assert!(screen.contains("no im just testing"));
        assert!(screen.contains("Enter: send"));
    }

    #[test]
    fn saved_answers_reload_as_option_numbers_when_revisiting() {
        let labels = ["A".to_string(), "Keep it brief".to_string()];
        assert_eq!(answer_line(&["A".to_string()], &labels), "1");
        // A multi-select answer becomes its number list, and reparsing it
        // reproduces the stored labels, so Enter keeps the answer unchanged.
        let multi = answer_line(&["Keep it brief".to_string(), "A".to_string()], &labels);
        assert_eq!(multi, "2 1");
        assert_eq!(parse_answer(&multi, &labels), ["Keep it brief", "A"]);
        // A custom answer stays its own text.
        assert_eq!(answer_line(&["custom-7".to_string()], &labels), "custom-7");
        assert_eq!(answer_line(&[], &labels), "");
    }

    #[test]
    fn each_question_shows_its_position_options_and_saved_answer() {
        use serde_json::json;
        let questions = vec![
            json!({"question": "Pick a model", "header": "Model", "multiple": false,
                   "options": [{"label": "A", "description": "First"},
                               {"label": "B", "description": "Second"}]}),
            json!({"question": "Run checks?", "header": "Checks", "multiple": true,
                   "options": [{"label": "Unit", "description": "Unit tests"}]}),
        ];
        let answers = vec![vec!["A".to_string()], Vec::new()];
        // Revisiting question 1 shows its saved answer with numbered options.
        let revisited = plain(&render_question(&questions, &answers, 0));
        assert!(revisited.contains("1/2: Pick a model"));
        assert!(revisited.contains("Model"));
        assert!(revisited.contains("1. A"));
        assert!(revisited.contains("✓ A"));
        assert!(!revisited.contains("2/2"));
        // Question 2 is unanswered, multi-select, and marks itself as the last.
        let current = plain(&render_question(&questions, &answers, 1));
        assert!(current.contains("2/2: Run checks?"));
        assert!(current.contains("(one or more)"));
        assert!(!current.contains("✓"));
        assert!(current.contains("Tab: next question"));
    }

    #[test]
    fn pasted_image_paths_are_detected_and_attached() {
        let directory = tempfile::tempdir().unwrap();
        let png = directory.path().join("shot.png");
        std::fs::write(&png, PNG).unwrap();
        let notes = directory.path().join("notes.txt");
        std::fs::write(&notes, b"plain text").unwrap();

        let detected = pasted_image_path(&format!("  \"{}\"  ", png.display())).unwrap();
        assert_eq!(detected, png.canonicalize().unwrap());
        // Multiline pastes and non-image paths stay plain text.
        assert!(pasted_image_path(&format!("{}\nlook", png.display())).is_none());
        assert!(pasted_image_path(&notes.display().to_string()).is_none());
        assert!(pasted_image_path("/no/such/image.png").is_none());

        let attachment = file_attachment(&png).unwrap();
        assert_eq!(attachment.filename, "shot.png");
        assert_eq!(attachment.mime, "image/png");
        assert!(attachment.url.starts_with("file://"));
        // A text file is rejected as an attachment.
        assert!(file_attachment(&notes).is_err());
    }

    #[test]
    fn pasted_image_paths_decode_file_urls_and_shell_escaped_spaces() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("screen shot #1.png");
        std::fs::write(&path, PNG).unwrap();
        let url = reqwest::Url::from_file_path(&path).unwrap();
        assert_eq!(pasted_image_path(url.as_str()).unwrap(), path);
        assert_eq!(
            pasted_image_path(&path.to_string_lossy().replace(' ', "\\ ")).unwrap(),
            path
        );
        assert!(pasted_image_path(&format!("{}\n", url)).is_some());
        assert!(pasted_image_path(&format!("{}\n{}", url, url)).is_none());
    }

    #[test]
    fn clipboard_bytes_attach_only_as_images() {
        let attachment = image_attachment(PNG.to_vec()).unwrap();
        assert_eq!(attachment.filename, "clipboard.png");
        assert_eq!(attachment.mime, "image/png");
        assert_eq!(
            attachment.url,
            format!(
                "data:image/png;base64,{}",
                base64::Engine::encode(&base64::engine::general_purpose::STANDARD, PNG)
            )
        );
        assert!(image_attachment(b"not an image".to_vec()).is_none());
    }

    /// A `Ui` with no pending prompt, for transcript rendering.
    fn new_ui() -> Ui {
        Ui {
            part_kind: HashMap::new(),
            headered: HashSet::new(),
            tool_done: HashSet::new(),
            prompt: None,
            visibility: Visibility::default(),
        }
    }

    fn bus_event(type_: &str, properties: serde_json::Value) -> crate::types::BusEvent {
        crate::types::BusEvent {
            id: "e1".into(),
            r#type: type_.into(),
            properties,
        }
    }

    /// The transcript text one toggle paints.
    fn painted(segments: &[(RowKind, String)], kind: RowKind) -> String {
        segments
            .iter()
            .filter(|(row_kind, _)| *row_kind == kind)
            .map(|(_, text)| text.as_str())
            .collect()
    }

    fn completed_tool() -> serde_json::Value {
        serde_json::json!({"id":"prt_1","sessionID":"ses_1","messageID":"msg_1","type":"tool",
            "tool":"bash","callID":"call_1","state":{"status":"completed",
            "input":{"command":"ls -la"},
            "output":"total 8\ndrwxr-xr-x 2 user user 4096 .\n",
            "time":{"start":1,"end":2}}})
    }

    #[test]
    fn a_resumed_tool_call_shows_its_command_and_its_result() {
        // The saved part keeps input, output and error, so a resumed session
        // has everything the live transcript showed.
        let (collapsed, expanded) = tool_block(&completed_tool());
        let text = plain(&collapsed);
        assert!(text.contains("● bash ls -la"), "{text}");
        assert!(
            text.contains("✓ total 8 drwxr-xr-x 2 user user 4096 ."),
            "{text}"
        );
        // A resumed call expands like a live one: the saved output whole, its
        // continuation lines indented under the marker.
        let text = plain(&expanded);
        assert!(
            text.contains("total 8\n  drwxr-xr-x 2 user user 4096 ."),
            "{text}"
        );

        let failed = serde_json::json!({"type":"tool","tool":"read","state":{
            "status":"error","input":{"filePath":"/tmp/missing.txt"},
            "error":"File not found","time":{"start":1,"end":2}}});
        let (collapsed, expanded) = tool_block(&failed);
        for text in [plain(&collapsed), plain(&expanded)] {
            assert!(text.contains("● read /tmp/missing.txt"), "{text}");
            assert!(text.contains("✗ File not found"), "{text}");
        }
    }

    #[test]
    fn a_tool_call_that_never_finished_shows_the_status_it_stopped_at() {
        for status in ["running", "pending"] {
            let part = serde_json::json!({"type":"tool","tool":"bash","state":{
                "status":status,"input":{"command":"sleep 30"},"time":{"start":1}}});
            let (collapsed, expanded) = tool_block(&part);
            // The status is part of the call, so both forms carry it.
            for text in [plain(&collapsed), plain(&expanded)] {
                assert!(text.contains("● bash sleep 30"), "{text}");
                assert!(text.ends_with(&format!("· {status}\n")), "{text}");
            }
            assert!(tool_result(&part).is_none(), "{status} has no result line");
        }
    }

    #[test]
    fn the_live_tool_lines_and_the_resumed_ones_are_the_same_lines() {
        let part = completed_tool();
        let mut ui = new_ui();
        let segments = render_event(
            &mut ui,
            &bus_event(
                "message.part.updated",
                serde_json::json!({"part": part.clone()}),
            ),
            "ses_1",
        );
        // One renderer formats a call for both paths, in both forms.
        let (collapsed, expanded) = tool_block(&part);
        assert_eq!(painted(&segments, RowKind::ToolBrief), collapsed);
        assert_eq!(painted(&segments, RowKind::ToolFull), expanded);

        // The header and result each print once, however many updates follow.
        for status in ["running", "completed"] {
            let event = bus_event(
                "message.part.updated",
                serde_json::json!({"part": serde_json::json!({
                    "id":"prt_1","sessionID":"ses_1","type":"tool","tool":"bash",
                    "state":{"status":status,"input":{"command":"ls -la"}}})}),
            );
            assert!(render_event(&mut ui, &event, "ses_1").is_empty());
        }
        // A part from another session stays off this transcript.
        let other = bus_event(
            "message.part.updated",
            serde_json::json!({"part": {"id":"prt_2","sessionID":"ses_2",
                "type":"tool","tool":"bash","state":{"status":"completed","input":{}}}}),
        );
        assert!(render_event(&mut ui, &other, "ses_1").is_empty());
        assert_eq!(plain(&tool_line(&completed_tool()).0), "● bash ls -la");
    }

    #[test]
    fn help_is_discoverable_and_lists_the_active_scroll_controls() {
        assert_eq!(
            decode_event(Event::Key(KeyEvent::new(KeyCode::F(1), KeyModifiers::NONE))),
            Key::Help
        );
        let header = plain(&prompt_header("mock/m", Visibility::default()));
        assert!(header.contains("/help"));
        assert!(header.contains("F1"));
        assert!(!header.contains("Ctrl+"));
        let fullscreen = plain(&help_text(true));
        assert!(fullscreen.contains("Ctrl+T"));
        assert!(fullscreen.contains("Ctrl+O"));
        assert!(fullscreen.contains("Ctrl+Home/End"));
        assert!(!help_text(false).contains("Ctrl+Home/End"));
    }

    #[test]
    fn the_transcript_toggles_decode_and_report_their_state_in_the_header() {
        for (ch, key) in [
            ('t', Key::ToggleThinking),
            ('o', Key::ToggleTools),
            ('T', Key::Other),
            ('O', Key::Other),
        ] {
            assert_eq!(
                decode_event(Event::Key(KeyEvent::new(
                    KeyCode::Char(ch),
                    KeyModifiers::CONTROL
                ))),
                key
            );
        }
        // The status reports visibility; shortcuts are available in help.
        let starts = Visibility::default();
        assert!(starts.thinking && !starts.tools_expanded);
        let header = plain(&prompt_header("mock/m", starts));
        assert!(header.contains("thinking on"));
        assert!(header.contains("tools collapsed"));
        let header = plain(&prompt_header(
            "mock/m",
            Visibility {
                thinking: false,
                tools_expanded: true,
            },
        ));
        assert!(header.contains("thinking off"));
        assert!(header.contains("tools expanded"));
    }

    #[test]
    fn thinking_and_tool_calls_split_into_segments_the_toggles_pick() {
        let mut ui = new_ui();
        // A reasoning part opens with a hideable header.
        let segments = render_event(
            &mut ui,
            &bus_event(
                "message.part.updated",
                serde_json::json!({"sessionID": "s", "part": {"id": "r1", "sessionID": "s", "type": "reasoning"}}),
            ),
            "s",
        );
        assert_eq!(segments.len(), 1);
        assert_eq!(segments[0].0, RowKind::Thinking);
        assert!(plain(&segments[0].1).contains("thinking"));
        // Its deltas follow the part's kind, so hiding it hides the stream.
        let segments = render_event(
            &mut ui,
            &bus_event(
                "message.part.delta",
                serde_json::json!({"sessionID": "s", "partID": "r1", "delta": "pondering"}),
            ),
            "s",
        );
        assert_eq!(segments[0].0, RowKind::Thinking);
        assert_eq!(plain(&segments[0].1), "pondering");
        // A finished tool emits a collapsed form and an expanded form, so
        // Ctrl+O swaps views instead of hiding work, and shows calls that
        // already ran. The long command and result are kept whole when expanded.
        let command = format!("ls {}", "dir/".repeat(60));
        let output = "notes.txt\nplan.md\nlong ".repeat(40);
        let segments = render_event(
            &mut ui,
            &bus_event(
                "message.part.updated",
                serde_json::json!({"sessionID": "s", "part": {"id": "t1", "sessionID": "s",
                    "type": "tool", "tool": "bash",
                    "state": {"status": "completed", "input": {"command": command}, "output": output}}}),
            ),
            "s",
        );
        let segments: Vec<_> = segments
            .into_iter()
            .map(|(kind, text)| (kind, plain(&text)))
            .collect();
        let collapsed = segments
            .iter()
            .filter(|(kind, _)| *kind == RowKind::ToolBrief)
            .map(|(_, text)| text)
            .cloned()
            .collect::<Vec<_>>()
            .join("");
        let expanded_form = segments
            .iter()
            .filter(|(kind, _)| *kind == RowKind::ToolFull)
            .map(|(_, text)| text)
            .cloned()
            .collect::<Vec<_>>()
            .join("");
        assert!(collapsed.contains("● bash") && collapsed.contains("ls dir/"));
        // Collapsed keeps the one-liners it always had.
        assert!(collapsed.contains('…') && !collapsed.contains(&output));
        // A leading blank row, then one row each for the call and its result.
        assert_eq!(collapsed.lines().count(), 3);
        // Expanded shows the whole command and the whole result, the multi-line
        // result indented under its marker.
        assert!(expanded_form.contains(&command));
        assert!(expanded_form.contains("plan.md"));
        assert!(expanded_form.contains("\n  long "));
        assert_eq!(expanded_form.lines().count(), 2 + output.lines().count());
        assert!(segments.iter().all(|(kind, _)| *kind != RowKind::Plain));
        // Answer text is never hideable.
        let mut ui = new_ui();
        render_event(
            &mut ui,
            &bus_event(
                "message.part.updated",
                serde_json::json!({"sessionID": "s", "part": {"id": "x1", "sessionID": "s", "type": "text"}}),
            ),
            "s",
        );
        let segments = render_event(
            &mut ui,
            &bus_event(
                "message.part.delta",
                serde_json::json!({"sessionID": "s", "partID": "x1", "delta": "the answer"}),
            ),
            "s",
        );
        assert_eq!(segments, vec![(RowKind::Plain, "the answer".into())]);
    }

    #[test]
    fn a_failed_tool_call_shows_in_both_forms() {
        for tools_expanded in [false, true] {
            let mut ui = new_ui();
            ui.visibility.tools_expanded = tools_expanded;
            let segments = render_event(
                &mut ui,
                &bus_event(
                    "message.part.updated",
                    serde_json::json!({"sessionID": "s", "part": {"id": "t1", "sessionID": "s",
                        "type": "tool", "tool": "bash",
                        "state": {"status": "error", "input": {}, "error": "permission denied"}}}),
                ),
                "s",
            );
            // Hiding tool output must never hide a failure: a collapsed call
            // that failed has to look like it failed.
            assert!(segments
                .iter()
                .any(|(kind, text)| ui.visibility.visible(*kind)
                    && plain(text).contains("✗ permission denied")));
        }
    }

    #[test]
    fn expanded_tool_text_is_capped_and_says_so() {
        // Text that fits shows whole; continuation lines indent under the call.
        assert_eq!(expand("short"), "short");
        assert_eq!(expand("line\nline"), "line\n  line");
        // Past the cap the write to the terminal is bounded, and the note names
        // what is missing so a cut-off block never reads as complete.
        let huge = "x".repeat(MAX_EXPANDED + 5_000);
        let shown = expand(&huge);
        assert!(shown.chars().count() < huge.chars().count());
        assert!(shown.ends_with("\n  … 5000 more characters not shown"));
    }

    #[test]
    fn a_huge_tool_result_cannot_flood_the_expanded_transcript() {
        let mut ui = new_ui();
        let output = "y".repeat(4 * MAX_EXPANDED);
        let segments = render_event(
            &mut ui,
            &bus_event(
                "message.part.updated",
                serde_json::json!({"sessionID": "s", "part": {"id": "t1", "sessionID": "s",
                    "type": "tool", "tool": "bash",
                    "state": {"status": "completed", "input": {"command": "grep -r . ."}, "output": output}}}),
            ),
            "s",
        );
        // One expanded call paints its command plus its result, each capped,
        // and the result says how much of it was left out.
        let shown: String = segments
            .iter()
            .filter(|(kind, _)| *kind == RowKind::ToolFull)
            .map(|(_, text)| text.as_str())
            .collect();
        let text = plain(&shown);
        assert!(text.contains("● bash grep -r . ."), "{text}");
        assert!(text.contains("more characters not shown"), "{text}");
        assert!(
            shown.chars().count() <= 2 * MAX_EXPANDED + 200,
            "{} chars",
            shown.chars().count()
        );
    }
    #[test]
    fn a_single_confirmed_question_can_be_reopened_with_tab() {
        let mut state = PendingPrompt::Question {
            id: "q".into(),
            questions: vec![
                serde_json::json!({"question":"Pick", "options":[{"label":"first","description":""}]}),
            ],
            answers: vec![vec!["first".into()]],
            current: 0,
            confirm: true,
            drafts: vec![Draft::default()],
        };
        assert!(switch_question(&mut state, 1).is_some());
        let PendingPrompt::Question {
            confirm, drafts, ..
        } = state
        else {
            unreachable!()
        };
        assert!(!confirm);
        assert_eq!(drafts[0].text, "1");
    }
}
