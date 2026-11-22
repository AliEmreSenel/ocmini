// Terminal transcript and input over the shared session engine.
use crate::commands::{Command, LocalAction, Registry, LOCAL_COMMANDS};
use crate::config::ScrollMode;
use crate::engine::{self, App, EngineApi};
use crate::engine_impl::Engine;
use crate::types::PromptRequest;
use crossterm::{
    event::{
        DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event, EventStream, KeyCode, KeyEventKind, KeyModifiers, MouseEventKind,
    },
    terminal::{self, EnterAlternateScreen, LeaveAlternateScreen},
};
use futures::{poll, StreamExt};
use serde_json::Value;
mod clipboard;
mod completion;
mod input;
mod markdown;
mod render;
use crate::attachments::Attachment;
use crate::client::{parse_answer, shutdown_signal, TextState};
#[cfg(test)]
use clipboard::image_attachment;
use clipboard::{
    apply_clipboard, clipboard_image, copy_text, file_attachment, pasted_image_path,
    resolve_user_path, ClipboardResult,
};
use completion::{Completion, CompletionContext};
use input::{
    clean_input, input_prefix, page_move, terminal_size, terminal_width, Display, Draft, History,
    KillRing,
};
pub(crate) use input::{strip_escapes, Motion};
use render::{
    answer_line, option_labels, render_event, render_history, render_question, session_prompts,
};
pub(crate) use render::{display_model, summarize_input};
#[cfg(test)]
use render::{expand, tool_line, tool_result, Row, MAX_EXPANDED};
use std::io::IsTerminal;
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

    fn leave_alternate_screen(&mut self) {
        if self.fullscreen {
            let mut out = std::io::stdout();
            let _ = crossterm::execute!(out, DisableMouseCapture, LeaveAlternateScreen);
            self.fullscreen = false;
        }
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

/// The only escape sequences the transcript paints. Assistant text, tool
/// output, fetched pages, MCP servers, hooks, model-written session titles and
/// file paths all reach the transcript, so every other sequence is consumed
/// and dropped: untrusted text cannot restyle the terminal, hide the cursor,
/// clear the scrollback, set the window title or open a hyperlink. Styling the
/// app wants is written from these constants, so it always passes.
pub(crate) const STYLES: &[&str] = &[RESET, DIM, BOLD, GREEN, RED, YELLOW, CYAN];

/// Whether an escape sequence is the app's own styling and may reach the
/// terminal.
pub(crate) fn style_ok(escape: &str) -> bool {
    STYLES.contains(&escape)
}

pub(crate) fn truncate(text: &str, max: usize) -> String {
    let mut chars = text.chars();
    let mut out: String = chars.by_ref().take(max).collect();
    if chars.next().is_some() {
        out.push('…');
    }
    out.replace('\n', " ")
}

/// Transcript rows and retained tool chains. Plain and literal text always stay visible.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum RowKind {
    Plain,
    /// Literal output (user prompts, permissions, questions, errors, notices)
    /// that bypasses Markdown parsing.
    Literal,
    /// Chain of thought: the `◦ thinking` header and every reasoning delta.
    Thinking,
    /// A retained tool chain, rendered in the selected tool view.
    Tools(usize),
    /// A reasoning block: full text or its duration/token summary.
    Thought(usize),
}

#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub(crate) enum ToolView {
    Grouped,
    #[default]
    Brief,
    Full,
}

impl ToolView {
    fn next(self) -> Self {
        match self {
            Self::Grouped => Self::Brief,
            Self::Brief => Self::Full,
            Self::Full => Self::Grouped,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Grouped => "grouped",
            Self::Brief => "brief",
            Self::Full => "full",
        }
    }
}

/// Transcript noise the reader can hide while chatting, with Ctrl+T and Ctrl+O.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Visibility {
    pub thinking: bool,
    pub tools: ToolView,
}

impl Default for Visibility {
    fn default() -> Self {
        Self {
            thinking: true,
            tools: ToolView::default(),
        }
    }
}

impl Visibility {
    fn visible(&self, kind: RowKind) -> bool {
        match kind {
            RowKind::Plain | RowKind::Literal => true,
            RowKind::Thinking => self.thinking,
            RowKind::Tools(_) | RowKind::Thought(_) => true,
        }
    }

    /// Compact status for the current transcript visibility.
    fn hints(&self) -> String {
        format!(
            " · thinking {} · tools {}",
            if self.thinking { "on" } else { "off" },
            self.tools.label()
        )
    }
}

#[derive(Default)]
struct Ui {
    interactions: crate::client::Interactions,
    text: TextState,
    tool_done: std::collections::HashSet<String>,
    // Prompt interaction routed from the terminal: lines when idle, keys while a request is pending.
    prompt: Option<PendingPrompt>,
    completion: Completion,
    /// Live transcript toggles, mirrored into the display on every change.
    visibility: Visibility,
    /// When running in native scrollback mode, only build the active tool variant.
    native_tool_view: Option<ToolView>,
    /// The answer's `assistant` label is already on screen. A turn runs one
    /// model step per tool batch, each step in its own assistant message, so a
    /// label per message printed one in the middle of the same answer. The next
    /// user prompt spends it.
    answer_labelled: bool,
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

#[derive(Clone, Default)]
pub(super) struct StatusBar {
    pub agent: String,
    pub model: String,
    pub variant: Option<String>,
    pub sandbox: Option<String>,
    pub busy: bool,
    pub tokens: i64,
    pub cost: f64,
}

impl StatusBar {
    fn from_state(
        app: &App,
        session: &crate::types::Session,
        selected: &PromptRequest,
        busy: bool,
    ) -> Self {
        let agent = selected
            .agent
            .as_deref()
            .or_else(|| {
                app.config
                    .extra
                    .get("default_agent")
                    .and_then(Value::as_str)
            })
            .unwrap_or("build")
            .to_owned();
        let model = display_model(selected.model.as_ref(), &app.config.model);
        let sandbox = session
            .sandbox
            .as_ref()
            .and_then(|s| s["preset"].as_str())
            .filter(|p| !p.is_empty() && *p != "off")
            .map(str::to_owned);
        let tokens = session.tokens.input
            + session.tokens.output
            + session.tokens.reasoning
            + session.tokens.cache.read
            + session.tokens.cache.write;
        Self {
            agent,
            model,
            variant: selected.variant.clone(),
            sandbox,
            busy,
            tokens,
            cost: session.cost,
        }
    }
}

pub(super) trait HeaderSource {
    fn format(&self, visibility: Visibility, width: usize) -> String;
}

impl HeaderSource for str {
    fn format(&self, visibility: Visibility, _width: usize) -> String {
        prompt_header(self, visibility)
    }
}

impl HeaderSource for String {
    fn format(&self, visibility: Visibility, _width: usize) -> String {
        prompt_header(self, visibility)
    }
}

impl HeaderSource for StatusBar {
    fn format(&self, visibility: Visibility, width: usize) -> String {
        prompt_header_for(self, visibility, width)
    }
}

fn compact_tokens(n: i64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

fn format_usage(tokens: i64, cost: f64) -> Option<String> {
    if tokens <= 0 && cost <= 0.0 {
        return None;
    }
    let mut out = if tokens > 0 {
        format!("{} tok", compact_tokens(tokens))
    } else {
        String::new()
    };
    if cost > 0.0 {
        if !out.is_empty() {
            out.push_str(" · ");
        }
        if cost < 1.0 {
            out.push_str(&format!("${cost:.4}"));
        } else {
            out.push_str(&format!("${cost:.2}"));
        }
    }
    Some(out)
}

fn prompt_header(model: &str, visibility: Visibility) -> String {
    format!(
        "{DIM}┌ /help · F1 help · model {model}{}{RESET}",
        visibility.hints()
    )
}

fn prompt_header_for(status: &StatusBar, visibility: Visibility, width: usize) -> String {
    let mut parts: Vec<String> = vec!["/help · F1 help".into()];
    if status.busy {
        parts.push(format!("{YELLOW}● working{RESET}{DIM}"));
    }
    let model_label = match &status.variant {
        Some(variant) if !variant.is_empty() => format!("{}#{variant}", status.model),
        _ => status.model.clone(),
    };
    let agent = if status.agent.is_empty() {
        "build"
    } else {
        &status.agent
    };
    parts.push(format!("{agent} · {model_label}"));

    let mut optional: Vec<String> = Vec::new();
    if let Some(sandbox) = status.sandbox.as_deref().filter(|s| *s != "off") {
        optional.push(format!("sandbox {sandbox}"));
    }
    if let Some(usage) = format_usage(status.tokens, status.cost) {
        optional.push(usage);
    }
    optional.push(format!(
        "thinking {} · tools {}",
        if visibility.thinking { "on" } else { "off" },
        visibility.tools.label()
    ));

    let visible_len = |s: &str| {
        let mut plain = 0usize;
        let mut rest = s;
        while !rest.is_empty() {
            if rest.starts_with("\x1b[") {
                if let Some(end) = rest[2..].find(|ch: char| ('@'..='~').contains(&ch)) {
                    rest = &rest[end + 3..];
                    continue;
                }
            }
            let ch = rest.chars().next().unwrap();
            plain += 1;
            rest = &rest[ch.len_utf8()..];
        }
        plain
    };

    let mut line = format!("┌ {}", parts.join(" · "));
    let mut len = visible_len(&line);
    for seg in optional {
        let add = 3 + visible_len(&seg);
        if len + add <= width {
            line.push_str(" · ");
            line.push_str(&seg);
            len += add;
        }
    }
    format!("{DIM}{line}{RESET}")
}

/// Local help is shown only on request, never sent to the model.
fn help_text(fullscreen: bool) -> String {
    let mut commands = String::new();
    for command in LOCAL_COMMANDS {
        let mut invocation = format!("/{}", command.name);
        if let Some(arguments) = command.arguments {
            invocation.push(' ');
            invocation.push_str(arguments);
        }
        if let Some(shortcut) = command.shortcut {
            invocation.push_str(" or ");
            invocation.push_str(shortcut);
        }
        commands.push_str(&format!("  {invocation:<25} {}\n", command.description));
    }
    let tools = if fullscreen {
        "Cycle tools: grouped / brief / full"
    } else {
        "Brief/full tools (new output)"
    };
    let scroll = if fullscreen {
        "  PgUp/PgDn              Scroll transcript\n  Ctrl+Home/End          Jump to top/bottom\n  Mouse wheel            Scroll transcript\n"
    } else {
        "  PgUp/PgDn              Move through draft\n  Mouse wheel            Terminal scrollback\n"
    };
    format!(
        "\n{BOLD}Help{RESET}\n{commands}\
  /                      Show commands (Up/Down select, Tab completes, Esc hides)\n\
  @path                  Complete and attach workspace files\n\
  Enter                  Send\n\
  Alt/Shift+Enter, Ctrl+J Insert newline\n\
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
  Alt+Backspace          Remove latest attachment\n\
  Ctrl+U                 Clear draft and attachments\n\
  Ctrl+C                 Abort turn\n\
  Ctrl+L                 Clear screen\n\
  Ctrl+R                 Switch session\n\
  Ctrl+T                 Toggle thinking\n\
  Ctrl+O                 {tools}\n\
{scroll}\
  Permissions: y/o once; a always; n/r/Esc reject\n\
  Questions: Tab/Shift+Tab switch; Enter answers/confirms; Esc dismisses\n"
    )
}

/// One answer line for one question. A line of option numbers ("2", "1 3" for
/// multiple) answers with those options' labels: the question contract and the
/// model both expect labels, so the displayed index must not reach the tool
/// output. Any other line - prose, an out-of-range number - is one custom
/// answer, kept exactly as typed.
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
    if crate::types::validate_answer(question, &choices).is_err() {
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
    /// Shared caret motion; Shift extends the selection.
    Move(Motion, bool),
    Undo,
    /// Ctrl+K: caret to line end. Ctrl+W: word behind the caret. Ctrl+Y: yank.
    KillLine,
    KillWord,
    Yank,
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
            (KeyCode::Char('a'), KeyModifiers::CONTROL) => Key::Move(Motion::Home, false),
            (KeyCode::Char('e'), KeyModifiers::CONTROL) => Key::Move(Motion::End, false),
            (KeyCode::Char('b'), KeyModifiers::ALT) => Key::Move(Motion::WordLeft, false),
            (KeyCode::Char('f'), KeyModifiers::ALT) => Key::Move(Motion::WordRight, false),
            (KeyCode::Backspace, modifiers) if modifiers.contains(KeyModifiers::ALT) => {
                Key::RemoveAttachment
            }
            // Plain Enter sends; Alt+Enter, Shift+Enter and Ctrl+J insert a newline.
            (KeyCode::Enter, modifiers)
                if modifiers.intersects(KeyModifiers::ALT | KeyModifiers::SHIFT) =>
            {
                Key::NewLine
            }
            (KeyCode::Char('j'), KeyModifiers::CONTROL) => Key::NewLine,
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
            (KeyCode::Left | KeyCode::Right, modifiers) => {
                let word = modifiers.intersects(KeyModifiers::ALT | KeyModifiers::CONTROL);
                let motion = match (key.code, word) {
                    (KeyCode::Left, true) => Motion::WordLeft,
                    (KeyCode::Right, true) => Motion::WordRight,
                    (KeyCode::Left, false) => Motion::Left,
                    _ => Motion::Right,
                };
                Key::Move(motion, modifiers.contains(KeyModifiers::SHIFT))
            }
            (KeyCode::Home | KeyCode::End, modifiers) => {
                match (key.code, modifiers.contains(KeyModifiers::CONTROL)) {
                    (KeyCode::Home, true) => Key::ScrollTop,
                    (_, true) => Key::ScrollBottom,
                    (KeyCode::Home, false) => {
                        Key::Move(Motion::Home, modifiers.contains(KeyModifiers::SHIFT))
                    }
                    _ => Key::Move(Motion::End, modifiers.contains(KeyModifiers::SHIFT)),
                }
            }
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

/// Interactive resume path for `--resume`: needs a real terminal because it
/// enters raw mode and reads keys before the chat loop exists.
pub(crate) async fn pick_session(
    app: &App,
    directory: Option<&str>,
) -> Result<crate::types::Session, String> {
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

impl Ui {
    /// Resolve dialog keys before ordinary draft editing. Global controls stay in the main loop.
    fn prompt_key(&mut self, app: &App, key: &Key) -> Option<String> {
        self.prompt_key_with(&app.perms, &app.questions, key)
    }

    fn prompt_key_with(
        &mut self,
        perms: &crate::permission::PermissionState,
        questions: &crate::engine::question::QuestionState,
        key: &Key,
    ) -> Option<String> {
        if matches!(
            key,
            Key::CtrlD
                | Key::CtrlC
                | Key::CtrlL
                | Key::CtrlU
                | Key::Help
                | Key::ToggleThinking
                | Key::ToggleTools
                | Key::PageUp
                | Key::PageDown
                | Key::ScrollTop
                | Key::ScrollBottom
                | Key::ScrollUp(_)
                | Key::ScrollDown(_)
                | Key::Other
        ) {
            return None;
        }
        let state = self.prompt.as_mut()?;
        if let PendingPrompt::Permission(id) = state {
            let reply = match key {
                Key::Char('y' | 'Y' | 'o' | 'O') => "once",
                Key::Char('a' | 'A') => "always",
                Key::Char('n' | 'N' | 'r' | 'R') | Key::Esc => "reject",
                Key::PageUp
                | Key::PageDown
                | Key::ScrollTop
                | Key::ScrollBottom
                | Key::ScrollUp(_)
                | Key::ScrollDown(_) => return None,
                // Ignore unrecognized keys instead of silently rejecting on a stray press.
                _ => return Some(String::new()),
            };
            if perms.reply(id, reply) {
                self.prompt = None;
            }
            return Some(String::new());
        }
        let confirming = matches!(state, PendingPrompt::Question { confirm: true, .. });
        match key {
            Key::Enter if confirming => {
                if let PendingPrompt::Question { id, answers, .. } = state {
                    if questions.reply(id, answers.clone()) {
                        self.prompt = None;
                    }
                }
                Some(String::new())
            }
            Key::Enter => {
                if let PendingPrompt::Question {
                    id,
                    questions: items,
                    ..
                } = state
                {
                    if items.is_empty() {
                        questions.reply(id, Vec::new());
                        self.prompt = None;
                        return Some(String::new());
                    }
                }
                let (finished, text) = answer_question(state);
                if finished {
                    self.prompt = None;
                }
                Some(text)
            }
            Key::Backspace => {
                if let PendingPrompt::Question {
                    current, drafts, ..
                } = state
                {
                    if let Some(draft) = drafts.get_mut(*current) {
                        if !draft.text.is_empty() {
                            draft.backspace();
                        } else if *current > 0 {
                            return switch_question(state, -1);
                        }
                    }
                }
                Some(String::new())
            }
            Key::Tab | Key::PreviousQuestion => Some(
                switch_question(state, if matches!(key, Key::Tab) { 1 } else { -1 })
                    .unwrap_or_default(),
            ),
            Key::Esc => {
                if let PendingPrompt::Question { id, .. } = state {
                    questions.reject(id);
                }
                self.prompt = None;
                Some(String::new())
            }
            _ if confirming => {
                if let PendingPrompt::Question {
                    confirm,
                    current,
                    questions,
                    answers,
                    ..
                } = state
                {
                    *confirm = false;
                    return Some(render_question(questions, answers, *current));
                }
                None
            }
            _ => None,
        }
    }
}

fn parse_model_spec(spec: &str) -> Result<(crate::types::ModelRef, Option<String>), String> {
    let trimmed = spec.trim();
    let (model_str, variant) = match trimmed.split_once('#') {
        Some((m, v)) => (m.trim(), Some(v.trim().to_owned())),
        None => (trimmed, None),
    };
    let model = crate::types::ModelRef::parse(model_str)?;
    let variant = variant.filter(|v| !v.is_empty());
    Ok((model, variant))
}

fn available_sandbox_presets(app: &App) -> Vec<String> {
    crate::sessions::sandbox_choices(app, &app.config)
        .ok()
        .and_then(|v| {
            v["presets"].as_array().map(|arr| {
                arr.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
        })
        .unwrap_or_default()
}

fn last_assistant_text(messages: &[crate::types::MessageWithParts]) -> Option<String> {
    for message in messages.iter().rev() {
        if message.info.role != "assistant" {
            continue;
        }
        let text: String = message
            .parts
            .iter()
            .filter(|part| part["type"].as_str() == Some("text"))
            .filter_map(|part| part["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n");
        if !text.trim().is_empty() {
            return Some(text);
        }
    }
    None
}

fn format_model_list(app: &App, selected: &PromptRequest) -> String {
    let active = display_model(selected.model.as_ref(), &app.config.model);
    let mut out = format!("\n{BOLD}Model{RESET}: {active}");
    if let Some(variant) = selected.variant.as_deref() {
        out.push_str(&format!("#{variant}"));
    }
    out.push('\n');
    let connected = app.auth.connected_for(&app.config);
    let mut models: Vec<String> = app
        .config
        .catalog()
        .into_iter()
        .filter(|m| {
            connected.is_empty()
                || connected
                    .iter()
                    .any(|p| m["providerID"].as_str() == Some(p.as_str()))
        })
        .filter_map(|m| {
            let p = m["providerID"].as_str()?;
            let id = m["modelID"].as_str()?;
            let mut label = format!("{p}/{id}");
            if let Some(variants) = m["variants"].as_object().filter(|v| !v.is_empty()) {
                let mut keys: Vec<&str> = variants.keys().map(String::as_str).collect();
                keys.sort_unstable();
                label.push_str(&format!(" {DIM}[{}]{RESET}", keys.join(", ")));
            }
            Some(label)
        })
        .collect();
    models.sort_unstable();
    if models.is_empty() {
        out.push_str(&format!("{DIM}  No configured models.{RESET}\n"));
    } else {
        for item in models {
            out.push_str(&format!("  {item}\n"));
        }
    }
    out
}

fn format_agent_list(app: &App, selected: &PromptRequest) -> String {
    let active = selected
        .agent
        .as_deref()
        .or_else(|| {
            app.config
                .extra
                .get("default_agent")
                .and_then(Value::as_str)
        })
        .unwrap_or("build");
    let mut out = format!("\n{BOLD}Agent{RESET}: {active}\n");
    for agent in crate::inventory::agents_json(&app.config)
        .as_array()
        .into_iter()
        .flatten()
    {
        if agent["hidden"] == true {
            continue;
        }
        let name = agent["name"].as_str().unwrap_or_default();
        let mode = agent["mode"].as_str().unwrap_or("all");
        let desc = agent["description"].as_str().unwrap_or_default();
        let mark = if name == active { "›" } else { " " };
        out.push_str(&format!("  {mark} {name} {DIM}({mode}) {desc}{RESET}\n"));
    }
    out
}

fn format_variant_list(app: &App, selected: &PromptRequest) -> String {
    let current = selected.variant.as_deref().unwrap_or("default");
    let mut out = format!("\n{BOLD}Variant{RESET}: {current}\n");
    let model_ref = selected
        .model
        .clone()
        .or_else(|| crate::types::ModelRef::parse(&app.config.model).ok());
    let mut variants = Vec::new();
    if let Some(mref) = model_ref {
        if let Some(model) = app.config.catalog().into_iter().find(|m| {
            m["providerID"].as_str() == Some(mref.provider_id.as_str())
                && m["modelID"].as_str() == Some(mref.model_id.as_str())
        }) {
            if let Some(map) = model["variants"].as_object() {
                variants.extend(map.keys().cloned());
                variants.sort_unstable();
            }
        }
    }
    if variants.is_empty() {
        out.push_str(&format!(
            "{DIM}  No named variants declared for this model (use /variant default to clear).{RESET}\n"
        ));
    } else {
        out.push_str(&format!("  Available: default, {}\n", variants.join(", ")));
    }
    out
}

fn format_mcp_status(app: &App) -> String {
    let runtime = app.mcp().status();
    let mut names: Vec<String> = app.config.mcp.keys().cloned().collect();
    if let Some(obj) = runtime.as_object() {
        for key in obj.keys() {
            if !names.contains(key) {
                names.push(key.clone());
            }
        }
    }
    names.sort_unstable();
    if names.is_empty() {
        return format!("\n{DIM}No MCP servers configured.{RESET}\n");
    }
    let mut out = format!("\n{BOLD}MCP servers{RESET}\n");
    for name in names {
        let status_obj = runtime.get(&name);
        let status = match status_obj.and_then(|v| v["status"].as_str()) {
            Some(s) => s,
            None => {
                if app
                    .config
                    .mcp
                    .get(&name)
                    .is_some_and(|c| c.enabled == Some(false))
                {
                    "disabled"
                } else {
                    "configured (not started)"
                }
            }
        };
        let error = status_obj
            .and_then(|v| v["error"].as_str())
            .filter(|e| !e.is_empty())
            .map(|e| format!(" — {e}"))
            .unwrap_or_default();
        out.push_str(&format!("  {name}: {status}{error}\n"));
    }
    out
}

pub async fn run(app: App, args: crate::TuiArgs) -> Result<(), crate::types::OperationError> {
    if args.fork && args.session.is_none() && !args.continue_last && !args.resume {
        return Err("--fork requires --session, --continue, or --resume".into());
    }
    let mut initial_attachments = Vec::new();
    for path in &args.file {
        initial_attachments.push(crate::attachments::from_file(std::path::Path::new(path))?);
    }
    let mut session = if args.resume {
        pick_session(&app, args.directory.as_deref()).await?
    } else if args.session.is_some() || args.continue_last {
        crate::sessions::open_for_app(
            &app,
            args.directory.as_deref(),
            args.session.as_deref(),
            args.continue_last,
        )
        .await?
    } else {
        let dir = crate::sessions::directory(args.directory.as_deref().unwrap_or("."))?;
        let mut session =
            crate::types::Session::new(args.title.as_deref().unwrap_or("tui session"), &dir);
        if args.title.is_some() {
            session.title_source = Some(crate::types::TitleSource::Explicit);
        }
        crate::sessions::create(&app, session, None).await?
    };
    if args.fork {
        session = crate::sessions::fork(&app, &session, None).await?;
    }
    let model_override = args.model.clone();
    let selected = PromptRequest {
        model: args.model,
        agent: args.agent,
        variant: args.variant,
        ..Default::default()
    };
    run_session(app, session, selected, model_override, initial_attachments).await
}

pub(crate) async fn run_session(
    app: App,
    mut session: crate::types::Session,
    mut selected: PromptRequest,
    model_override: Option<crate::types::ModelRef>,
    initial_attachments: Vec<Attachment>,
) -> Result<(), crate::types::OperationError> {
    let engine = Engine::new();
    let (mut app, history) = crate::client::activate(&app, &session, &mut selected)?;
    let mut sandbox_presets = available_sandbox_presets(&app);
    let mut status_bar = StatusBar::from_state(&app, &session, &selected, false);

    let fullscreen = matches!(app.config.as_ref().ui.scroll_mode, ScrollMode::Fullscreen);
    let shutdown =
        shutdown_signal().map_err(|error| format!("Cannot listen for shutdown: {error}"))?;
    let mut raw = RawMode::enter(fullscreen)
        .map_err(|error| format!("Cannot initialize terminal: {error}"))?;
    let mut events = engine::bus().subscribe();
    let mut keys = EventStream::new();
    tokio::pin!(shutdown);

    let mut draft = Draft::default();
    draft.attachments = initial_attachments;
    let mut killring = KillRing::default();
    let mut prompt_history = History::default();
    prompt_history.load(session_prompts(&history));
    let mut display = Display::new(fullscreen);
    let mut ui = Ui::default();
    let width = terminal_width();
    display.output_literal(
        &format!(
            "{BOLD}ocmini{RESET} {DIM}{}{RESET}\n",
            crate::OPENCODE_COMPAT_VERSION
        ),
        width,
    );
    display.output_literal(
        &format!(
            "{DIM}session {} · {}{RESET}\n",
            session.id, session.directory
        ),
        width,
    );
    if !fullscreen {
        ui.native_tool_view = Some(ui.visibility.tools);
    }
    render_history(&mut display, &mut ui, &history);
    let initial_size = terminal_size();
    display.draw_sized(&ui, &draft, initial_size, &status_bar);
    let (clipboard_tx, mut clipboard_rx) =
        tokio::sync::mpsc::unbounded_channel::<ClipboardResult>();
    let spawn_clipboard = |tx: &tokio::sync::mpsc::UnboundedSender<ClipboardResult>| {
        let tx = tx.clone();
        tokio::spawn(async move {
            let result =
                tokio::time::timeout(std::time::Duration::from_secs(3), clipboard_image()).await;
            let _ = tx.send(result);
        });
    };

    loop {
        tokio::select! {
            _ = &mut shutdown => break,
            Some(result) = clipboard_rx.recv() => {
                let size = terminal_size();
                display.erase();
                apply_clipboard(result, &mut draft, &mut display, size.0);
                display.draw_sized(&ui, &draft, size, &status_bar);
            }
            event = keys.next() => {
                let Some(event) = event else { break; };
                let event = event.map_err(|error| error.to_string())?;
                let is_resize = matches!(event, Event::Resize(_, _));
                let key = decode_event(event);
                if !is_resize && matches!(key, Key::Other) {
                    continue;
                }
                let size = terminal_size();
                let width = size.0;
                display.erase();
                if is_resize && !fullscreen {
                    display.finish_line();
                }
                let pending_permission = matches!(ui.prompt, Some(PendingPrompt::Permission(_)));
                let pending_question = matches!(ui.prompt, Some(PendingPrompt::Question { .. }));
                if let Some(text) = ui.prompt_key(&app, &key) {
                    if !text.is_empty() { display.output_literal(&text, width); }
                    display.draw_sized(&ui, &draft, size, &status_bar);
                    continue;
                }
                if ui.prompt.is_none() {
                    let (handled, notice) = ui.completion.apply_key(
                        &key,
                        &mut draft,
                        Some(std::path::Path::new(&session.directory)),
                    );
                    if let Some(msg) = notice {
                        display.output_literal(&format!("\n{YELLOW}{msg}{RESET}\n"), width);
                    }
                    if handled {
                        let ctx = CompletionContext {
                            config: &app.config,
                            auth: &app.auth,
                            directory: std::path::Path::new(&session.directory),
                            active_model: selected.model.as_ref(),
                            sandbox_presets: &sandbox_presets,
                        };
                        ui.completion.update_with(&draft, &ctx);
                        display.draw_sized(&ui, &draft, size, &status_bar);
                        continue;
                    }
                }
                match key {
                    Key::CtrlD => break,
                    Key::CtrlC if pending_permission => { if let Some(PendingPrompt::Permission(id)) = ui.prompt.take() { app.perms.reply(&id, "reject"); } }
                    Key::CtrlC => { ui.prompt = None; status_bar.busy = false; engine.abort(&session.id); display.output_literal(&format!("\n{YELLOW}⏹ aborted{RESET}\n"), width); }
                    Key::CtrlL => display.clear_screen(),
                    Key::Help => {
                        display.finish_line();
                        display.output_literal(&help_text(fullscreen), width);
                        display.scroll_bottom();
                    }
                    key @ (Key::ToggleThinking | Key::ToggleTools) => {
                        // Fullscreen cycles all retained chains through three
                        // views. Native mode keeps brief/full for new output.
                        match key {
                            Key::ToggleThinking => ui.visibility.thinking = !ui.visibility.thinking,
                            _ => {
                                ui.visibility.tools = if fullscreen {
                                    ui.visibility.tools.next()
                                } else if ui.visibility.tools == ToolView::Full {
                                    ToolView::Brief
                                } else {
                                    ToolView::Full
                                };
                                if !fullscreen {
                                    ui.native_tool_view = Some(ui.visibility.tools);
                                }
                            }
                        }
                        display.set_visibility(ui.visibility);
                    }
                    Key::CtrlU => {
                        if !pending_permission {
                            active_draft(&mut ui.prompt, &mut draft).clear();
                            if ui.prompt.is_none() {
                                prompt_history.reset();
                            }
                        }
                    }
                    Key::RemoveAttachment if !pending_permission && !pending_question => { draft.attachments.pop(); }
                    Key::CtrlV if !pending_permission && !pending_question => {
                        spawn_clipboard(&clipboard_tx);
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
                                        display.output_literal(
                                            &format!("{RED}error:{RESET} {error}\n"),
                                            width,
                                        );
                                    }
                                    Ok(resumed) => {
                                        let mut next_selected = PromptRequest { model: model_override.clone(), ..Default::default() };
                                        match crate::client::activate(&app, &resumed, &mut next_selected) {
                                            Err(error) => {
                                                display.output_literal(
                                                    &format!("{RED}error:{RESET} {error}\n"),
                                                    width,
                                                );
                                            }
                                            Ok((scoped, history)) => {
                                                engine.abort(&session.id);
                                                selected = next_selected;
                                                app = scoped;
                                                session = resumed;
                                                sandbox_presets = available_sandbox_presets(&app);
                                                ui.interactions.clear();
                                                ui.text = TextState::default();
                                                ui.tool_done.clear();
                                                ui.answer_labelled = false;
                                                ui.prompt = None;
                                                ui.completion = Completion::default();
                                                status_bar = StatusBar::from_state(&app, &session, &selected, false);
                                                draft = Draft::default();
                                                prompt_history.load(session_prompts(&history));
                                                display.reset_session();
                                                display.output_literal(
                                                    &format!(
                                                        "\n{BOLD}resumed{RESET} {DIM}{} · {}{RESET}\n",
                                                        session.id, session.directory
                                                    ),
                                                    width,
                                                );
                                                render_history(&mut display, &mut ui, &history);
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    Key::Paste(_) if pending_permission => {}
                    Key::Paste(text) => {
                        if text.is_empty() {
                            if !pending_question {
                                spawn_clipboard(&clipboard_tx);
                            }
                        } else if let Some(path) = pasted_image_path(&text).filter(|_| !pending_question) {
                            match file_attachment(&path) {
                                Ok(attachment) => draft.attachments.push(attachment),
                                Err(message) => display.output_literal(&format!("\n{YELLOW}{message}{RESET}\n"), width),
                            }
                        } else {
                            // Answers are text-only: attachments stay with the prompt.
                            active_draft(&mut ui.prompt, &mut draft).insert_str(&clean_input(&text));
                        }
                    }
                    Key::Enter if pending_permission => {},
                    Key::Enter => {
                        display.finish_line();
                        let command = Registry::new(&app.config).resolve_terminal(&draft.text);
                        let configured = match command {
                            Some((Command::Local(command), arguments)) => {
                                let arguments = arguments.to_owned();
                                // Local actions leave attachments in the draft (except /new and /fork).
                                match command.action {
                                    LocalAction::Help => {
                                        draft.clear_text();
                                        display.output_literal(&help_text(fullscreen), width);
                                        display.scroll_bottom();
                                    }
                                    LocalAction::Context => {
                                        draft.clear_text();
                                        match crate::engine_impl::inspect_context(&app, &session, &selected).await {
                                            Ok(report) => display.output_literal(&format!("\n{report}"), width),
                                            Err(error) => display.output_literal(&format!("\n{RED}error:{RESET} {error}\n"), width),
                                        }
                                        display.scroll_bottom();
                                    }
                                    LocalAction::Compact => {
                                        let mut part = serde_json::json!({"type":"compaction","auto":false});
                                        if !arguments.is_empty() {
                                            part["ocminiFocus"] = Value::String(arguments);
                                        }
                                        display.user_prompt(draft.text.trim(), width);
                                        draft.clear_text();
                                        let request = PromptRequest {
                                            message_id: Some(crate::types::hex_id("msg_")),
                                            parts: vec![part],
                                            command: Some("__ocmini_compact".into()),
                                            ..selected.clone()
                                        };
                                        if let Err(error) = engine.admit(app.clone(), session.id.clone(), request).await {
                                            display.output_literal(&format!("{RED}error:{RESET} {error}\n"), width);
                                        }
                                    }
                                    LocalAction::Model => {
                                        draft.clear_text();
                                        if arguments.is_empty() {
                                            display.output_literal(&format_model_list(&app, &selected), width);
                                        } else {
                                            match parse_model_spec(&arguments) {
                                                Ok((model_ref, variant)) => {
                                                    let changed = selected.model.as_ref() != Some(&model_ref);
                                                    selected.model = Some(model_ref.clone());
                                                    if let Some(v) = variant {
                                                        selected.variant = (!matches!(v.as_str(), "default" | "none" | "off")).then_some(v);
                                                    } else if changed {
                                                        selected.variant = None;
                                                    }
                                                    status_bar = StatusBar::from_state(&app, &session, &selected, status_bar.busy);
                                                    let shown = match &selected.variant {
                                                        Some(v) => format!("{model_ref}#{v}"),
                                                        None => model_ref.to_string(),
                                                    };
                                                    display.output_literal(&format!("\n{DIM}◦ model: {shown}{RESET}\n"), width);
                                                }
                                                Err(error) => {
                                                    display.output_literal(&format!("\n{RED}error:{RESET} {error}\n"), width);
                                                }
                                            }
                                        }
                                        display.scroll_bottom();
                                    }
                                    LocalAction::Agent => {
                                        draft.clear_text();
                                        if arguments.is_empty() {
                                            display.output_literal(&format_agent_list(&app, &selected), width);
                                        } else {
                                            let known = crate::inventory::agents_json(&app.config)
                                                .as_array()
                                                .is_some_and(|list| list.iter().any(|a| a["name"].as_str() == Some(arguments.as_str())));
                                            if known {
                                                selected.agent = Some(arguments.clone());
                                                status_bar = StatusBar::from_state(&app, &session, &selected, status_bar.busy);
                                                display.output_literal(&format!("\n{DIM}◦ agent: {arguments}{RESET}\n"), width);
                                            } else {
                                                display.output_literal(&format!("\n{RED}error:{RESET} Unknown agent: {arguments}\n"), width);
                                            }
                                        }
                                        display.scroll_bottom();
                                    }
                                    LocalAction::Variant => {
                                        draft.clear_text();
                                        if arguments.is_empty() {
                                            display.output_literal(&format_variant_list(&app, &selected), width);
                                        } else if matches!(arguments.as_str(), "default" | "clear" | "off") {
                                            selected.variant = None;
                                            status_bar = StatusBar::from_state(&app, &session, &selected, status_bar.busy);
                                            display.output_literal(&format!("\n{DIM}◦ variant: default{RESET}\n"), width);
                                        } else {
                                            selected.variant = Some(arguments.clone());
                                            status_bar = StatusBar::from_state(&app, &session, &selected, status_bar.busy);
                                            display.output_literal(&format!("\n{DIM}◦ variant: {arguments}{RESET}\n"), width);
                                        }
                                        display.scroll_bottom();
                                    }
                                    LocalAction::New => {
                                        draft.clear();
                                        engine.abort(&session.id);
                                        let fresh = crate::types::Session::new("tui session", &session.directory);
                                        match crate::sessions::create(&app, fresh, None).await {
                                            Ok(created) => {
                                                let mut next_selected = PromptRequest { model: model_override.clone(), ..Default::default() };
                                                match crate::client::activate(&app, &created, &mut next_selected) {
                                                    Ok((scoped, history)) => {
                                                        selected = next_selected;
                                                        app = scoped;
                                                        session = created;
                                                        sandbox_presets = available_sandbox_presets(&app);
                                                        ui.interactions.clear();
                                                        ui.text = TextState::default();
                                                        ui.tool_done.clear();
                                                        ui.prompt = None;
                                                        ui.completion = Completion::default();
                                                        ui.answer_labelled = false;
                                                        status_bar = StatusBar::from_state(&app, &session, &selected, false);
                                                        prompt_history.load(session_prompts(&history));
                                                        display.reset_session();
                                                        display.output_literal(
                                                            &format!("{BOLD}new session{RESET} {DIM}{} · {}{RESET}\n", session.id, session.directory),
                                                            width,
                                                        );
                                                    }
                                                    Err(error) => display.output_literal(&format!("\n{RED}error:{RESET} {error}\n"), width),
                                                }
                                            }
                                            Err(error) => display.output_literal(&format!("\n{RED}error:{RESET} {error}\n"), width),
                                        }
                                    }
                                    LocalAction::Fork => {
                                        draft.clear_text();
                                        match crate::sessions::fork(&app, &session, None).await {
                                            Ok(forked) => {
                                                let mut next_selected = selected.clone();
                                                match crate::client::activate(&app, &forked, &mut next_selected) {
                                                    Ok((scoped, history)) => {
                                                        selected = next_selected;
                                                        app = scoped;
                                                        session = forked;
                                                        sandbox_presets = available_sandbox_presets(&app);
                                                        ui.interactions.clear();
                                                        ui.text = TextState::default();
                                                        ui.tool_done.clear();
                                                        ui.prompt = None;
                                                        ui.completion = Completion::default();
                                                        ui.answer_labelled = false;
                                                        status_bar = StatusBar::from_state(&app, &session, &selected, false);
                                                        prompt_history.load(session_prompts(&history));
                                                        display.reset_session();
                                                        display.output_literal(
                                                            &format!("{BOLD}forked{RESET} {DIM}{} · {}{RESET}\n", session.id, session.title),
                                                            width,
                                                        );
                                                        render_history(&mut display, &mut ui, &history);
                                                    }
                                                    Err(error) => display.output_literal(&format!("\n{RED}error:{RESET} {error}\n"), width),
                                                }
                                            }
                                            Err(error) => display.output_literal(&format!("\n{RED}error:{RESET} {error}\n"), width),
                                        }
                                    }
                                    LocalAction::Rename => {
                                        draft.clear_text();
                                        if arguments.is_empty() {
                                            display.output_literal(&format!("\n{YELLOW}usage: /rename <title>{RESET}\n"), width);
                                        } else {
                                            match crate::sessions::update(&app, &session, serde_json::json!({"title": arguments})).await {
                                                Ok(updated) => {
                                                    session = updated;
                                                    display.output_literal(&format!("\n{DIM}◦ title: {}{RESET}\n", clean_input(&session.title)), width);
                                                }
                                                Err(error) => display.output_literal(&format!("\n{RED}error:{RESET} {error}\n"), width),
                                            }
                                        }
                                        display.scroll_bottom();
                                    }
                                    LocalAction::Revert => {
                                        draft.clear_text();
                                        if app.store.is_busy(&session.id).unwrap_or(false) {
                                            display.output_literal(&format!("\n{YELLOW}Stop the current turn before reverting.{RESET}\n"), width);
                                        } else {
                                            match app.store.messages(&session.id) {
                                                Ok(messages) => {
                                                    if let Some(last_user) = messages.iter().rev().find(|m| m.info.role == "user") {
                                                        let restored_text = last_user
                                                            .parts
                                                            .iter()
                                                            .filter(|p| p["type"].as_str() == Some("text"))
                                                            .filter_map(|p| p["text"].as_str())
                                                            .collect::<Vec<_>>()
                                                            .join("\n");
                                                        match app.store.v2_revert(&session.id, Some(&last_user.info.id)) {
                                                            Ok(()) => {
                                                                let history = app.store.messages(&session.id).unwrap_or_default();
                                                                if let Ok(Some(fresh)) = app.store.get_session(&session.id) {
                                                                    session = fresh;
                                                                }
                                                                ui.text = TextState::default();
                                                                ui.tool_done.clear();
                                                                ui.answer_labelled = false;
                                                                status_bar = StatusBar::from_state(&app, &session, &selected, false);
                                                                prompt_history.load(session_prompts(&history));
                                                                display.reset_session();
                                                                render_history(&mut display, &mut ui, &history);
                                                                display.output_literal(&format!("\n{DIM}◦ reverted last turn{RESET}\n"), width);
                                                                if !restored_text.is_empty() {
                                                                    draft.insert_str(&restored_text);
                                                                }
                                                            }
                                                            Err(error) => display.output_literal(&format!("\n{RED}error:{RESET} {error}\n"), width),
                                                        }
                                                    } else {
                                                        display.output_literal(&format!("\n{YELLOW}Nothing to revert.{RESET}\n"), width);
                                                    }
                                                }
                                                Err(error) => display.output_literal(&format!("\n{RED}error:{RESET} {error}\n"), width),
                                            }
                                        }
                                        display.scroll_bottom();
                                    }
                                    LocalAction::Mcp => {
                                        draft.clear_text();
                                        let mut parts = arguments.split_whitespace();
                                        match (parts.next(), parts.next()) {
                                            (Some("connect"), Some(name)) => {
                                                app.wait_mcp_ready().await;
                                                match app.mcp().connect_async(name).await {
                                                    Ok(st) => display.output_literal(&format!("\n{DIM}◦ mcp {name}: {}{RESET}\n", st["status"].as_str().unwrap_or("unknown")), width),
                                                    Err(error) => display.output_literal(&format!("\n{RED}error:{RESET} {error}\n"), width),
                                                }
                                            }
                                            (Some("disconnect"), Some(name)) => {
                                                match app.mcp().disconnect(name) {
                                                    Ok(_) => display.output_literal(&format!("\n{DIM}◦ mcp {name}: disconnected{RESET}\n"), width),
                                                    Err(error) => display.output_literal(&format!("\n{RED}error:{RESET} {error}\n"), width),
                                                }
                                            }
                                            _ => {
                                                display.output_literal(&format_mcp_status(&app), width);
                                            }
                                        }
                                        display.scroll_bottom();
                                    }
                                    LocalAction::Sandbox => {
                                        draft.clear_text();
                                        if arguments.is_empty() {
                                            match crate::sessions::sandbox_choices(&app, &app.config) {
                                                Ok(info) => {
                                                    let current = session
                                                        .sandbox
                                                        .as_ref()
                                                        .and_then(|s| s["preset"].as_str())
                                                        .or_else(|| info["default"].as_str())
                                                        .unwrap_or("off");
                                                    let presets: Vec<&str> = info["presets"]
                                                        .as_array()
                                                        .into_iter()
                                                        .flatten()
                                                        .filter_map(Value::as_str)
                                                        .collect();
                                                    let forced = info["forced"].as_str().map(|f| format!(" (forced: {f})")).unwrap_or_default();
                                                    display.output_literal(
                                                        &format!("\n{BOLD}Sandbox{RESET}: {current}{forced}\n  Presets: {}\n", presets.join(", ")),
                                                        width,
                                                    );
                                                }
                                                Err(error) => display.output_literal(&format!("\n{RED}error:{RESET} {error}\n"), width),
                                            }
                                        } else if app.store.is_busy(&session.id).unwrap_or(false) {
                                            display.output_literal(&format!("\n{YELLOW}Stop the current turn before switching sandbox.{RESET}\n"), width);
                                        } else {
                                            match crate::sessions::select_sandbox(&app, &session, &arguments) {
                                                Ok(saved) => {
                                                    session.sandbox = Some(saved);
                                                    status_bar = StatusBar::from_state(&app, &session, &selected, status_bar.busy);
                                                    display.output_literal(&format!("\n{DIM}◦ sandbox: {arguments} (applies from next turn){RESET}\n"), width);
                                                }
                                                Err(error) => display.output_literal(&format!("\n{RED}error:{RESET} {error}\n"), width),
                                            }
                                        }
                                        display.scroll_bottom();
                                    }
                                    LocalAction::Copy => {
                                        draft.clear_text();
                                        match app.store.messages(&session.id) {
                                            Ok(messages) => match last_assistant_text(&messages) {
                                                Some(reply) => match copy_text(&reply).await {
                                                    Ok(()) => display.output_literal(
                                                        &format!("\n{DIM}◦ copied last reply to clipboard ({} chars){RESET}\n", reply.chars().count()),
                                                        width,
                                                    ),
                                                    Err(error) => display.output_literal(&format!("\n{YELLOW}{error}{RESET}\n"), width),
                                                },
                                                None => display.output_literal(&format!("\n{YELLOW}No assistant reply to copy.{RESET}\n"), width),
                                            },
                                            Err(error) => display.output_literal(&format!("\n{RED}error:{RESET} {error}\n"), width),
                                        }
                                        display.scroll_bottom();
                                    }
                                    LocalAction::Attach => {
                                        draft.clear_text();
                                        if arguments.is_empty() {
                                            display.output_literal(&format!("\n{YELLOW}usage: /attach <path>{RESET}\n"), width);
                                        } else {
                                            match resolve_user_path(&arguments, &session.directory)
                                                .and_then(|path| crate::attachments::from_file(&path))
                                            {
                                                Ok(attachment) => {
                                                    let name = clean_input(&attachment.filename);
                                                    draft.attachments.push(attachment);
                                                    display.output_literal(&format!("\n{DIM}◦ attached: {name}{RESET}\n"), width);
                                                }
                                                Err(error) => display.output_literal(&format!("\n{YELLOW}{error}{RESET}\n"), width),
                                            }
                                        }
                                        display.scroll_bottom();
                                    }
                                    LocalAction::Quit => {
                                        display.user_prompt(&draft.text, width);
                                        break;
                                    }
                                }
                                let ctx = CompletionContext {
                                    config: &app.config,
                                    auth: &app.auth,
                                    directory: std::path::Path::new(&session.directory),
                                    active_model: selected.model.as_ref(),
                                    sandbox_presets: &sandbox_presets,
                                };
                                ui.completion.update_with(&draft, &ctx);
                                display.draw_sized(&ui, &draft, size, &status_bar);
                                continue;
                            }
                            Some((Command::Configured { name, .. }, arguments)) => {
                                Some((name, arguments.to_owned()))
                            }
                            None => None,
                        };
                        let (text, files) = draft.take();
                        display.user_prompt(&text, width);
                        for file in &files {
                            display.output_literal(
                                &format!(
                                    "{DIM}● attached: {}{RESET}\n",
                                    clean_input(&file.filename)
                                ),
                                width,
                            );
                        }
                        prompt_history.remember(text.clone());
                        let mut parts: Vec<Value> = Vec::new();
                        if !text.trim().is_empty() {
                            parts.push(serde_json::json!({"type": "text", "text": text}));
                        }
                        for attachment in files {
                            parts.push(attachment.part());
                        }
                        if !parts.is_empty() {
                            let request = match configured {
                                Some((name, arguments)) => prepare_configured_command(
                                    &app.config,
                                    name,
                                    &arguments,
                                    parts,
                                    &selected,
                                    model_override.as_ref(),
                                )
                                .map_err(crate::types::OperationError::invalid),
                                None => Ok(PromptRequest {
                                    message_id: Some(crate::types::hex_id("msg_")),
                                    parts,
                                    ..selected.clone()
                                }),
                            };
                            let result = match request {
                                Ok(request) => engine.admit(app.clone(), session.id.clone(), request).await.map(drop),
                                Err(error) => Err(error),
                            };
                            if let Err(error) = result {
                                display.output_literal(
                                    &format!("{RED}error:{RESET} {error}\n"),
                                    width,
                                );
                            }
                        }
                    }
                    key if !pending_permission && input::edit(&key, active_draft(&mut ui.prompt, &mut draft), &mut killring) => {}
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
                                    active_draft(&mut ui.prompt, &mut draft).motion(Motion::Home, false);
                                }
                                _ => active_draft(&mut ui.prompt, &mut draft).motion(Motion::End, false),
                            }
                        }
                    }
                    _ => {}
                }
                let ctx = CompletionContext {
                    config: &app.config,
                    auth: &app.auth,
                    directory: std::path::Path::new(&session.directory),
                    active_model: selected.model.as_ref(),
                    sandbox_presets: &sandbox_presets,
                };
                ui.completion.update_with(&draft, &ctx);
                display.draw_sized(&ui, &draft, size, &status_bar);
            }
            event = events.recv() => {
                let size = terminal_size();
                let width = size.0;
                let mut dirty = false;
                let mut closed = false;
                let mut current = Some(event);
                while let Some(item) = current.take() {
                    match item {
                        Ok(event) => {
                            if crate::client::accepts(&app.store, &session.id, &event) {
                                let is_current = crate::client::event_session(&event)
                                    .is_none_or(|id| id == session.id);
                                if is_current {
                                    let status_changed = match event.kind {
                                        crate::events::EventKind::Session(crate::events::SessionKind::Status) => {
                                            let kind = event.properties["status"]["type"].as_str().unwrap_or_default();
                                            let busy = matches!(kind, "busy" | "retry");
                                            let changed = status_bar.busy != busy;
                                            status_bar.busy = busy;
                                            changed
                                        }
                                        crate::events::EventKind::Session(crate::events::SessionKind::Idle) => {
                                            status_bar.busy = false;
                                            if let Ok(Some(fresh)) = app.store.get_session(&session.id) {
                                                session = fresh;
                                                status_bar = StatusBar::from_state(&app, &session, &selected, false);
                                            }
                                            true
                                        }
                                        crate::events::EventKind::Part(crate::events::PartKind::Updated)
                                            if event.properties["part"]["type"].as_str() == Some("step-finish") =>
                                        {
                                            if let Ok(Some(fresh)) = app.store.get_session(&session.id) {
                                                session = fresh;
                                                status_bar = StatusBar::from_state(&app, &session, &selected, status_bar.busy);
                                            }
                                            true
                                        }
                                        _ => false,
                                    };
                                    if status_changed && !dirty {
                                        display.erase();
                                        dirty = true;
                                    }
                                }
                                let event_session = crate::client::event_session(&event).unwrap_or(&session.id);
                                let rows = render_event(&mut ui, &event, event_session);
                                if !rows.is_empty() {
                                    if !dirty {
                                        display.erase();
                                        dirty = true;
                                    }
                                    display.apply(rows, width);
                                }
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                            if !dirty {
                                display.erase();
                                dirty = true;
                            }
                            let history = app.store.messages(&session.id)?;
                            if let Ok(Some(fresh)) = app.store.get_session(&session.id) {
                                session = fresh;
                            }
                            let busy = app.store.is_busy(&session.id).unwrap_or(false);
                            status_bar = StatusBar::from_state(&app, &session, &selected, busy);
                            ui = Ui {
                                visibility: ui.visibility,
                                native_tool_view: ui.native_tool_view,
                                ..Default::default()
                            };
                            let ctx = CompletionContext {
                                config: &app.config,
                                auth: &app.auth,
                                directory: std::path::Path::new(&session.directory),
                                active_model: selected.model.as_ref(),
                                sandbox_presets: &sandbox_presets,
                            };
                            ui.completion.update_with(&draft, &ctx);
                            display.clear_screen();
                            display.output_literal(&format!("{YELLOW}⚠ event stream resynchronized{RESET}\n"), width);
                            render_history(&mut display, &mut ui, &history);
                            for pending in crate::client::pending_events(&app, &session.id) {
                                display.apply(render_event(&mut ui, &pending, &session.id), width);
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                            closed = true;
                            break;
                        }
                    }
                    match events.try_recv() {
                        Ok(next) => current = Some(Ok(next)),
                        Err(tokio::sync::broadcast::error::TryRecvError::Lagged(n)) => {
                            current = Some(Err(tokio::sync::broadcast::error::RecvError::Lagged(n)));
                        }
                        Err(tokio::sync::broadcast::error::TryRecvError::Closed) => {
                            closed = true;
                            break;
                        }
                        Err(tokio::sync::broadcast::error::TryRecvError::Empty) => {}
                    }
                }
                if closed {
                    break;
                }
                if dirty {
                    display.draw_sized(&ui, &draft, size, &status_bar);
                }
            }
        }
    }
    display.erase();
    display.finish_line();
    raw.leave_alternate_screen();
    display.leave_screen();
    engine.abort_and_wait(&session.id).await;
    Ok(())
}

fn prepare_configured_command(
    config: &crate::config::Config,
    name: &str,
    arguments: &str,
    parts: Vec<Value>,
    selected: &PromptRequest,
    model_override: Option<&crate::types::ModelRef>,
) -> Result<PromptRequest, String> {
    let base = PromptRequest {
        message_id: Some(crate::types::hex_id("msg_")),
        model: model_override.cloned(),
        parts,
        ..Default::default()
    };
    let mut request = crate::commands::prepare(config, name, arguments, base, None)?;
    if request.agent.is_none() {
        request.agent = selected.agent.clone();
    }
    if request.model.is_none() {
        request.model = selected.model.clone();
        if request.variant.is_none() {
            request.variant = selected.variant.clone();
        }
    } else if request.variant.is_none() && request.model == selected.model {
        request.variant = selected.variant.clone();
    }
    Ok(request)
}

#[cfg(test)]
mod tests {
    use super::{
        answer_line, decode_event, expand, file_attachment, help_text, image_attachment,
        parse_answer, pasted_image_path, prompt_header, render_confirmation, render_event,
        render_question, switch_question, tool_line, tool_result, Draft, Key, Motion,
        PendingPrompt, Row, RowKind, ToolView, Ui, Visibility, MAX_EXPANDED, WHEEL,
    };
    use crossterm::event::{
        Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEvent, MouseEventKind,
    };

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
            (
                KeyCode::Char('a'),
                KeyModifiers::CONTROL,
                Key::Move(Motion::Home, false),
            ),
            (
                KeyCode::Char('e'),
                KeyModifiers::CONTROL,
                Key::Move(Motion::End, false),
            ),
            (
                KeyCode::Char('f'),
                KeyModifiers::ALT,
                Key::Move(Motion::WordRight, false),
            ),
            (
                KeyCode::Char('b'),
                KeyModifiers::ALT,
                Key::Move(Motion::WordLeft, false),
            ),
            (
                KeyCode::Left,
                KeyModifiers::SHIFT,
                Key::Move(Motion::Left, true),
            ),
            (
                KeyCode::Right,
                KeyModifiers::SHIFT,
                Key::Move(Motion::Right, true),
            ),
            (KeyCode::Tab, KeyModifiers::NONE, Key::Tab),
            (KeyCode::BackTab, KeyModifiers::SHIFT, Key::PreviousQuestion),
            (
                KeyCode::Home,
                KeyModifiers::SHIFT,
                Key::Move(Motion::Home, true),
            ),
            (
                KeyCode::End,
                KeyModifiers::SHIFT,
                Key::Move(Motion::End, true),
            ),
            (
                KeyCode::Left,
                KeyModifiers::ALT | KeyModifiers::SHIFT,
                Key::Move(Motion::WordLeft, true),
            ),
            (
                KeyCode::Right,
                KeyModifiers::ALT | KeyModifiers::SHIFT,
                Key::Move(Motion::WordRight, true),
            ),
            (KeyCode::Enter, KeyModifiers::ALT, Key::NewLine),
            (KeyCode::Enter, KeyModifiers::empty(), Key::Enter),
            (
                KeyCode::Left,
                KeyModifiers::empty(),
                Key::Move(Motion::Left, false),
            ),
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
            (
                KeyCode::Home,
                KeyModifiers::empty(),
                Key::Move(Motion::Home, false),
            ),
            (
                KeyCode::End,
                KeyModifiers::empty(),
                Key::Move(Motion::End, false),
            ),
            (
                KeyCode::Char('a'),
                KeyModifiers::CONTROL,
                Key::Move(Motion::Home, false),
            ),
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
        let directory = crate::test_tempdir().unwrap();
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
    fn tool_block(part: &serde_json::Value) -> (String, String) {
        let rows = super::render::render_part(&mut Ui::default(), part);
        (
            tool_text(&rows, ToolView::Brief),
            tool_text(&rows, ToolView::Full),
        )
    }

    fn new_ui() -> Ui {
        Ui::default()
    }

    #[test]
    fn descendant_interactions_settle_by_request_id_and_user_text_has_no_assistant_heading() {
        let mut ui = new_ui();
        let question = bus_event(
            "question.asked",
            serde_json::json!({"sessionID":"child","id":"question","questions":[{"question":"Choose","header":"Choice","options":[]}]}),
        );
        assert!(!render_event(&mut ui, &question, "root").is_empty());
        assert!(matches!(ui.prompt, Some(PendingPrompt::Question { .. })));
        render_event(
            &mut ui,
            &bus_event(
                "question.rejected",
                serde_json::json!({"sessionID":"child","requestID":"unrelated"}),
            ),
            "root",
        );
        assert!(ui.prompt.is_some());
        render_event(
            &mut ui,
            &bus_event(
                "question.replied",
                serde_json::json!({"sessionID":"child","requestID":"question"}),
            ),
            "root",
        );
        assert!(ui.prompt.is_none());
        render_event(
            &mut ui,
            &bus_event(
                "message.updated",
                serde_json::json!({"info":{"id":"user","sessionID":"root","role":"user"}}),
            ),
            "root",
        );
        assert!(render_event(&mut ui, &bus_event("message.part.updated", serde_json::json!({"part":{"id":"part","messageID":"user","sessionID":"root","type":"text","text":"prompt"}})), "root").is_empty());
    }

    fn bus_event(type_: &str, properties: serde_json::Value) -> crate::types::BusEvent {
        crate::types::BusEvent {
            id: "e1".into(),
            kind: serde_json::from_value(serde_json::json!(type_)).expect("known event"),
            properties,
        }
    }

    fn tool_text(rows: &[Row], view: ToolView) -> String {
        rows.iter()
            .filter_map(|row| match row {
                Row::Tool(call) => Some(format!(
                    "{}\n",
                    if view == ToolView::Full {
                        &call.full
                    } else {
                        &call.brief
                    }
                )),
                _ => None,
            })
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
            let rows = super::render::render_part(&mut Ui::default(), &part);
            // Pending and running updates carry both detail views. Neither
            // appends transcript text while the batch is being generated.
            for view in [ToolView::Brief, ToolView::Full] {
                let text = plain(&tool_text(&rows, view));
                assert!(text.contains("● bash sleep 30"), "{text}");
                assert!(text.ends_with(&format!("· {status}\n")), "{text}");
            }
            assert!(rows.iter().all(|row| matches!(row, Row::Tool(_))));
            assert!(tool_result(&part).is_none(), "{status} has no result line");
        }
    }

    #[test]
    fn a_tool_update_keeps_its_result_under_its_call() {
        let mut ui = new_ui();
        let part = |state: serde_json::Value| serde_json::json!({"id":"t1","sessionID":"s","type":"tool","tool":"bash","state":state});
        let running = render_event(
            &mut ui,
            &bus_event(
                "message.part.updated",
                serde_json::json!({"sessionID": "s", "part": part(serde_json::json!(
                    {"status":"running","input":{"command":"ls"},"time":{"start":1}}))}),
            ),
            "s",
        );
        // State updates replace the retained call instead of appending rows.
        assert!(matches!(running[0], Row::Tool(_)));
        assert!(running.iter().all(|row| !matches!(row, Row::Text(..))));
        let done = render_event(
            &mut ui,
            &bus_event(
                "message.part.updated",
                serde_json::json!({"sessionID": "s", "part": part(serde_json::json!(
                    {"status":"completed","input":{"command":"ls"},"output":"file1\nfile2",
                     "time":{"start":1,"end":2}}))}),
            ),
            "s",
        );
        let [Row::Tool(call)] = done.as_slice() else {
            panic!("the finished call updates one retained block")
        };
        assert_eq!(plain(&call.brief), "● bash ls\n  ✓ file1 file2");
        assert_eq!(call.status, "completed");
    }

    #[test]
    fn pending_calls_refresh_their_input_and_status_until_they_finish() {
        let mut ui = new_ui();
        let mut part = completed_tool();
        for status in ["pending", "running", "completed"] {
            part["state"]["status"] = serde_json::json!(status);
            part["state"]["input"]["command"] = serde_json::json!(format!("echo {status}"));
            let rows = super::render::render_part(&mut ui, &part);
            let [Row::Tool(call)] = rows.as_slice() else {
                panic!("expected one call update")
            };
            assert_eq!(call.status, status);
            assert!(plain(&call.brief).contains(&format!("echo {status}")));
        }
        assert!(super::render::render_part(&mut ui, &part).is_empty());
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
        assert_eq!(tool_text(&segments, ToolView::Brief), collapsed);
        assert_eq!(tool_text(&segments, ToolView::Full), expanded);

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
        assert!(starts.thinking && starts.tools == ToolView::Brief);
        assert_eq!(starts.tools.next(), ToolView::Full);
        assert_eq!(starts.tools.next().next(), ToolView::Grouped);
        assert_eq!(starts.tools.next().next().next(), ToolView::Brief);
        let header = plain(&prompt_header("mock/m", starts));
        assert!(header.contains("thinking on"));
        assert!(header.contains("tools brief"));
        let header = plain(&prompt_header(
            "mock/m",
            Visibility {
                thinking: false,
                tools: ToolView::Full,
            },
        ));
        assert!(header.contains("thinking off"));
        assert!(header.contains("tools full"));
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
        let Row::Thought(thought) = &segments[0] else {
            panic!("a reasoning part opens a retained thought block")
        };
        assert!(thought.new && thought.text.is_empty());
        // Its deltas follow the part's kind, so hiding it hides the stream.
        let segments = render_event(
            &mut ui,
            &bus_event(
                "message.part.delta",
                serde_json::json!({"sessionID": "s", "partID": "r1", "delta": "pondering"}),
            ),
            "s",
        );
        let Row::Thought(thought) = &segments[0] else {
            panic!("a reasoning delta updates its retained thought block")
        };
        assert_eq!(thought.text, "pondering");
        assert_eq!(thought.id, "r1");
        // The retained call carries brief and full detail views. The full
        // view preserves the long command and every line of the result.
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
        let collapsed = plain(&tool_text(&segments, ToolView::Brief));
        let expanded_form = plain(&tool_text(&segments, ToolView::Full));
        assert!(collapsed.contains("● bash") && collapsed.contains("ls dir/"));
        // Collapsed keeps the one-liners it always had.
        assert!(collapsed.contains('…') && !collapsed.contains(&output));
        // One row each for the call and its result, the result under the call.
        assert_eq!(collapsed.lines().count(), 2);
        // Expanded shows the whole command and the whole result, the multi-line
        // result indented under its marker.
        assert!(expanded_form.contains(&command));
        assert!(expanded_form.contains("plan.md"));
        assert!(expanded_form.contains("\n  long "));
        assert_eq!(expanded_form.lines().count(), 1 + output.lines().count());
        assert!(segments
            .iter()
            .all(|row| !matches!(row, Row::Text(RowKind::Plain, _))));
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
        assert_eq!(
            segments,
            vec![Row::Text(RowKind::Plain, "the answer".into())]
        );
    }

    #[test]
    fn a_failed_tool_call_shows_in_both_forms() {
        for tools in [ToolView::Brief, ToolView::Full] {
            let mut ui = new_ui();
            ui.visibility.tools = tools;
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
            assert!(plain(&tool_text(&segments, tools)).contains("✗ permission denied"));
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
        let shown = tool_text(&segments, ToolView::Full);
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

    #[test]
    fn configured_commands_keep_resumed_selection_unless_overridden() {
        let cfg: crate::config::Config = serde_json::from_value(serde_json::json!({
            "command": {
                "plain": "Review $ARGUMENTS",
                "custom": {
                    "template": "Audit $ARGUMENTS",
                    "agent": "explore",
                    "model": "other/fast"
                }
            }
        }))
        .unwrap();
        let resumed = super::PromptRequest {
            model: Some(crate::types::ModelRef {
                provider_id: "p".into(),
                model_id: "m".into(),
            }),
            agent: Some("plan".into()),
            variant: Some("high".into()),
            ..Default::default()
        };
        let plain =
            super::prepare_configured_command(&cfg, "plain", "diff", vec![], &resumed, None)
                .unwrap();
        assert_eq!(plain.model, resumed.model);
        assert_eq!(plain.agent.as_deref(), Some("plan"));
        assert_eq!(plain.variant.as_deref(), Some("high"));

        let custom =
            super::prepare_configured_command(&cfg, "custom", "diff", vec![], &resumed, None)
                .unwrap();
        assert_eq!(
            custom.model,
            Some(crate::types::ModelRef {
                provider_id: "other".into(),
                model_id: "fast".into(),
            })
        );
        assert_eq!(custom.agent.as_deref(), Some("explore"));
        assert!(custom.variant.is_none());

        let cli_model = crate::types::ModelRef {
            provider_id: "cli".into(),
            model_id: "override".into(),
        };
        let overridden = super::prepare_configured_command(
            &cfg,
            "custom",
            "diff",
            vec![],
            &resumed,
            Some(&cli_model),
        )
        .unwrap();
        assert_eq!(overridden.model.as_ref(), Some(&cli_model));
        assert_eq!(overridden.agent.as_deref(), Some("explore"));
        assert!(overridden.variant.is_none());
    }

    #[test]
    fn newline_shortcuts_support_shift_enter_ctrl_j_and_alt_enter() {
        for (code, modifiers) in [
            (KeyCode::Enter, KeyModifiers::ALT),
            (KeyCode::Enter, KeyModifiers::SHIFT),
            (KeyCode::Char('j'), KeyModifiers::CONTROL),
        ] {
            assert_eq!(
                decode_event(Event::Key(KeyEvent::new(code, modifiers))),
                Key::NewLine,
                "{code:?} + {modifiers:?}"
            );
        }
    }

    #[test]
    fn permission_prompt_accepts_run_keys_and_ignores_stray_characters() {
        let perms = crate::permission::PermissionState::default();
        let questions = crate::engine::question::QuestionState::default();
        let make = |perms: &crate::permission::PermissionState| {
            let (req, rx) = perms.ask_for_tool(
                serde_json::json!({"sessionID": "ses_perm", "tool": "bash"}),
                "bash",
                "ls",
                &serde_json::json!({"command": "ls"}),
            );
            let ui = Ui {
                prompt: Some(PendingPrompt::Permission(req.id)),
                ..Default::default()
            };
            (ui, rx)
        };

        // Stray characters do NOT dismiss or reject the permission prompt.
        let (mut ui, mut rx) = make(&perms);
        assert_eq!(
            ui.prompt_key_with(&perms, &questions, &Key::Char('x')),
            Some(String::new())
        );
        assert_eq!(
            ui.prompt_key_with(&perms, &questions, &Key::Char(' ')),
            Some(String::new())
        );
        assert_eq!(
            ui.prompt_key_with(&perms, &questions, &Key::Enter),
            Some(String::new())
        );
        assert!(ui.prompt.is_some());
        assert!(rx.try_recv().is_err());

        // y / o -> once
        assert_eq!(
            ui.prompt_key_with(&perms, &questions, &Key::Char('y')),
            Some(String::new())
        );
        assert!(ui.prompt.is_none());
        assert_eq!(rx.try_recv().unwrap().decision, "once");

        let (mut ui, mut rx) = make(&perms);
        assert_eq!(
            ui.prompt_key_with(&perms, &questions, &Key::Char('o')),
            Some(String::new())
        );
        assert_eq!(rx.try_recv().unwrap().decision, "once");

        // a -> always
        let (mut ui, mut rx) = make(&perms);
        assert_eq!(
            ui.prompt_key_with(&perms, &questions, &Key::Char('a')),
            Some(String::new())
        );
        assert_eq!(rx.try_recv().unwrap().decision, "always");

        // n / r / Esc -> reject
        for key in [Key::Char('n'), Key::Char('r'), Key::Esc] {
            let (mut ui, mut rx) = make(&perms);
            assert_eq!(
                ui.prompt_key_with(&perms, &questions, &key),
                Some(String::new())
            );
            assert!(ui.prompt.is_none());
            assert_eq!(rx.try_recv().unwrap().decision, "reject");
        }
    }

    #[test]
    fn model_spec_and_status_bar_render_rich_metadata() {
        let (m, v) = super::parse_model_spec("openai/gpt-5#high").unwrap();
        assert_eq!(m.provider_id, "openai");
        assert_eq!(m.model_id, "gpt-5");
        assert_eq!(v.as_deref(), Some("high"));

        let (m, v) = super::parse_model_spec("mock/test-model").unwrap();
        assert_eq!(m.provider_id, "mock");
        assert_eq!(m.model_id, "test-model");
        assert!(v.is_none());

        let bar = super::StatusBar {
            agent: "plan".into(),
            model: "mock/m".into(),
            variant: Some("high".into()),
            sandbox: Some("read-only".into()),
            busy: true,
            tokens: 1_500,
            cost: 0.0125,
        };
        let wide = plain(&super::prompt_header_for(&bar, Visibility::default(), 160));
        assert!(wide.contains("● working"), "{wide}");
        assert!(wide.contains("plan · mock/m#high"), "{wide}");
        assert!(wide.contains("sandbox read-only"), "{wide}");
        assert!(wide.contains("1.5k tok · $0.0125"), "{wide}");
        assert!(wide.contains("thinking on · tools brief"), "{wide}");
    }
}
