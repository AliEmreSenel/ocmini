// Terminal transcript and input over the shared session engine.
use crate::engine::{self, App, EngineApi};
use crate::engine_impl::Engine;
use crate::types::{MessageWithParts, PromptRequest};
use base64::Engine as _;
use crossterm::{
    event::{
        DisableBracketedPaste, EnableBracketedPaste, Event, EventStream, KeyCode, KeyEventKind,
        KeyModifiers,
    },
    terminal,
};
use futures::{poll, StreamExt};
use serde_json::Value;
mod input;
use input::{
    attachment_prefix, clean_input, pop_grapheme, terminal_width, Display, Draft, History, KillRing,
};
use std::collections::HashMap;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::task::Poll;

struct RawMode;

impl RawMode {
    fn enter() -> std::io::Result<Self> {
        terminal::enable_raw_mode()?;
        let guard = Self;
        crossterm::execute!(std::io::stdout(), EnableBracketedPaste)?;
        Ok(guard)
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        let _ = crossterm::execute!(std::io::stdout(), DisableBracketedPaste);
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

struct Ui {
    part_kind: HashMap<String, &'static str>, // partID -> text | reasoning
    headered: std::collections::HashSet<String>, // part ids that printed their header
    tool_done: std::collections::HashSet<String>,
    // Prompt interaction routed from the terminal: lines when idle, keys while a request is pending.
    prompt: Option<PendingPrompt>,
}

enum PendingPrompt {
    Permission(String),
    Question {
        id: String,
        // One entry per question: its option labels in the numbered order shown.
        options: Vec<Vec<String>>,
        answers: Vec<Vec<String>>,
        line: String,
    },
}

impl Drop for Ui {
    fn drop(&mut self) {
        use std::io::Write;
        let _ = std::io::stdout().flush();
    }
}

fn print_prompt(model: &str) {
    println!("{DIM}┌ model {model} · Enter send · Alt+Enter newline · ↑↓ history · ←→↑↓ Home End move · Alt+←→ word · Shift select · Ctrl+A/E line · Ctrl+K/W cut · Ctrl+Y yank · Ctrl+Z undo · paste inserts all lines · Ctrl+V/Ctrl+Shift+V image · Alt+Backspace remove image · Ctrl+U clear draft · Ctrl+C abort · Ctrl+D quit · Ctrl+L clear · Ctrl+R sessions{RESET}");
}

pub(crate) fn summarize_input(input: &Value) -> String {
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
    truncate(&parts.join(" "), 120)
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

fn render_event(ui: &mut Ui, event: &crate::types::BusEvent, session_id: &str) -> String {
    use std::fmt::Write as _;
    let mut output = String::new();
    macro_rules! print {
        ($($arg:tt)*) => {{ let _ = write!(output, $($arg)*); }};
    }
    macro_rules! println {
        ($($arg:tt)*) => {{ let _ = writeln!(output, $($arg)*); }};
    }
    let properties = &event.properties;
    if properties["sessionID"]
        .as_str()
        .or_else(|| properties["part"]["sessionID"].as_str())
        .is_some_and(|id| id != session_id)
    {
        return output;
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
                            println!("\n{DIM}◦ thinking{RESET}");
                        } else {
                            println!("\n{BOLD}assistant{RESET}");
                        }
                    }
                }
                Some("tool") => {
                    let tool = part["tool"].as_str().unwrap_or("tool");
                    let state = &part["state"];
                    if ui.headered.insert(format!("{part_id}:start")) {
                        println!(
                            "\n{CYAN}● {tool}{RESET} {DIM}{}{RESET}",
                            summarize_input(&state["input"])
                        );
                    }
                    let status = state["status"].as_str().unwrap_or_default();
                    if (status == "completed" || status == "error") && ui.tool_done.insert(part_id)
                    {
                        if status == "completed" {
                            let output = state["output"].as_str().unwrap_or_default();
                            println!("  {GREEN}✓{RESET} {DIM}{}{RESET}", truncate(output, 160));
                        } else {
                            println!(
                                "  {RED}✗{RESET} {}",
                                truncate(state["error"].as_str().unwrap_or("failed"), 160)
                            );
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
                Some("text") => print!("{delta}"),
                Some("reasoning") => print!("{DIM}{delta}{RESET}"),
                _ => {}
            }
        }
        "permission.asked" => {
            let id = properties["id"].as_str().unwrap_or_default().to_string();
            println!(
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
            let mut options = Vec::new();
            if let Some(list) = properties["questions"].as_array() {
                for question in list {
                    println!(
                        "\n{YELLOW}? {}{RESET}",
                        question["question"].as_str().unwrap_or_default()
                    );
                    let choices = option_values(&question["options"]);
                    for (index, (label, description)) in choices.iter().enumerate() {
                        println!(
                            "  {CYAN}{}{RESET}. {label} {DIM}{description}{RESET}",
                            index + 1
                        );
                    }
                    options.push(choices.into_iter().map(|(label, _)| label).collect());
                }
                println!("{DIM}type number(s), Enter to answer, Esc to dismiss{RESET}");
            }
            ui.prompt = Some(PendingPrompt::Question {
                id,
                options,
                answers: Vec::new(),
                line: String::new(),
            });
        }
        "session.error" => {
            let message = properties["error"]["data"]["message"]
                .as_str()
                .unwrap_or("unknown error");
            println!("\n{RED}error:{RESET} {message}");
        }
        "session.idle" => {
            println!("\n{DIM}└ idle{RESET}");
        }
        _ => {}
    }
    output
}

/// One answer line for one question. A typed number selects an option, and the
/// answer carries that option's label: the question contract and the model both
/// expect labels, so the displayed index must not reach the tool output.
/// Anything else, including an out-of-range number, is a custom answer as typed.
fn parse_answer(line: &str, labels: &[String]) -> Vec<String> {
    line.split_ascii_whitespace()
        .map(|token| match token.parse::<usize>() {
            Ok(number) if number >= 1 => labels
                .get(number - 1)
                .cloned()
                .unwrap_or_else(|| token.to_string()),
            _ => token.to_string(),
        })
        .collect()
}

fn answer_question(app: &App, state: &mut PendingPrompt, key: Key) -> bool {
    // True when the prompt finished and the normal input prompt should return.
    let PendingPrompt::Question {
        id,
        options,
        answers,
        line,
    } = state
    else {
        return false;
    };
    match key {
        Key::Esc => {
            app.questions.reject(id);
            true
        }
        Key::Enter => {
            let labels = options
                .get(answers.len())
                .map(Vec::as_slice)
                .unwrap_or_default();
            let choices = parse_answer(line, labels);
            line.clear();
            if !choices.is_empty() {
                answers.push(choices);
            }
            if answers.len() >= options.len() {
                let payload = std::mem::take(answers);
                let _ = app.questions.reply(id, payload);
                return true;
            }
            false
        }
        Key::Backspace => {
            pop_grapheme(line);
            false
        }
        Key::Char(ch) => {
            line.push(ch);
            false
        }
        _ => false,
    }
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
    Paste(String),
    Other,
}

/// Rows a page key moves, matching the picker's viewport height.
const PAGE: usize = 10;

pub(crate) fn decode_event(event: Event) -> Key {
    match event {
        Event::Paste(text) => Key::Paste(text.replace("\r\n", "\n").replace('\r', "\n")),
        Event::Key(key) if key.kind != KeyEventKind::Release => match (key.code, key.modifiers) {
            (KeyCode::Char('c'), KeyModifiers::CONTROL) => Key::CtrlC,
            (KeyCode::Char('d'), KeyModifiers::CONTROL) => Key::CtrlD,
            (KeyCode::Char('l'), KeyModifiers::CONTROL) => Key::CtrlL,
            (KeyCode::Char('r'), KeyModifiers::CONTROL) => Key::CtrlR,
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
            (KeyCode::PageUp, _) => Key::Up(PAGE),
            (KeyCode::PageDown, _) => Key::Down(PAGE),
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
            (KeyCode::Home, modifiers) if modifiers.contains(KeyModifiers::SHIFT) => {
                Key::SelectHome
            }
            (KeyCode::End, modifiers) if modifiers.contains(KeyModifiers::SHIFT) => Key::SelectEnd,
            (KeyCode::Home, _) => Key::Home,
            (KeyCode::End, _) => Key::End,
            (KeyCode::Tab, _) => Key::Tab,
            (KeyCode::Char(ch), modifiers)
                if !modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                Key::Char(ch)
            }
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

fn render_history(history: &[MessageWithParts]) {
    for message in history {
        println!("\n{BOLD}{}{RESET}", message.info.role);
        for part in &message.parts {
            match part["type"].as_str() {
                Some("text") => println!("{}", part["text"].as_str().unwrap_or_default()),
                Some("tool") => println!(
                    "{DIM}● {} · {}{RESET}",
                    part["tool"].as_str().unwrap_or("tool"),
                    part["state"]["status"].as_str().unwrap_or_default()
                ),
                Some("file") => println!(
                    "{DIM}attachment: {}{RESET}",
                    part["filename"]
                        .as_str()
                        .or_else(|| part["url"].as_str())
                        .unwrap_or("file")
                ),
                _ => {}
            }
        }
        if let Some(error) = &message.info.error {
            println!(
                "{RED}error: {}{RESET}",
                error["data"]["message"].as_str().unwrap_or("interrupted")
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
    let _raw = RawMode::enter().map_err(|error| format!("Cannot initialize terminal: {error}"))?;
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
        crate::sessions::open(
            &app.store,
            args.directory.as_deref(),
            args.session.as_deref(),
            args.continue_last,
        )?
    };
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

    let shutdown =
        shutdown_signal().map_err(|error| format!("Cannot listen for shutdown: {error}"))?;
    let _raw = RawMode::enter().map_err(|error| format!("Cannot initialize terminal: {error}"))?;
    let mut events = engine::bus().subscribe();
    let mut keys = EventStream::new();
    tokio::pin!(shutdown);

    let mut draft = Draft::default();
    let mut killring = KillRing::default();
    let mut prompt_history = History::default();
    prompt_history.load(session_prompts(&history));
    let mut display = Display::default();
    let mut ui = Ui {
        part_kind: HashMap::new(),
        headered: std::collections::HashSet::new(),
        tool_done: std::collections::HashSet::new(),
        prompt: None,
    };
    println!(
        "{BOLD}ocmini{RESET} {DIM}{}{RESET}",
        crate::OPENCODE_COMPAT_VERSION
    );
    println!("{DIM}session {} · {}{RESET}", session.id, session.directory);
    render_history(&history);
    print_prompt(&model_display);
    display.draw(&ui, &draft, terminal_width());

    loop {
        tokio::select! {
            _ = &mut shutdown => break,
            event = keys.next() => {
                let Some(event) = event else { break; };
                let event = event.map_err(|error| error.to_string())?;
                display.erase();
                if matches!(event, Event::Resize(_, _)) {
                    display.finish_line();
                }
                let key = decode_event(event);
                let pending_permission = matches!(ui.prompt, Some(PendingPrompt::Permission(_)));
                let pending_question = matches!(ui.prompt, Some(PendingPrompt::Question { .. }));
                match key {
                    Key::CtrlD => break,
                    Key::CtrlC if pending_permission => { if let Some(PendingPrompt::Permission(id)) = ui.prompt.take() { app.perms.reply(&id, "reject"); } }
                    Key::CtrlC => { engine.abort(&session.id); display.output(&format!("\n{YELLOW}⏹ aborted{RESET}\n"), terminal_width()); }
                    Key::CtrlL => { display.finish_line(); print!("\x1b[2J\x1b[H"); print_prompt(&model_display); }
                    Key::CtrlU => {
                        if let Some(PendingPrompt::Question { line, .. }) = ui.prompt.as_mut() {
                            line.clear();
                        } else if !pending_permission {
                            draft.clear();
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
                                        println!("{RED}error:{RESET} {error}");
                                        print_prompt(&model_display);
                                    }
                                    Ok(resumed) => {
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
                                        draft.clear();
                                        prompt_history.load(session_prompts(&history));
                                        println!(
                                            "\n{BOLD}resumed{RESET} {DIM}{} · {}{RESET}",
                                            session.id, session.directory
                                        );
                                        render_history(&history);
                                        print_prompt(&model_display);
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
                    key @ (Key::Char(_) | Key::Enter | Key::Backspace | Key::Esc) if pending_question => {
                        if let Some(state) = ui.prompt.as_mut() {
                            if answer_question(&app, state, key) {
                                ui.prompt = None;
                            }
                        }
                    }
                    Key::Paste(text) if pending_question => {
                        if let Some(PendingPrompt::Question { line, .. }) = ui.prompt.as_mut() {
                            line.push_str(&clean_input(&text).replace('\n', " "));
                        }
                    }
                    Key::Paste(_) if pending_permission => {}
                    Key::Paste(text) => {
                        if text.is_empty() {
                            paste_clipboard(&mut draft, &mut display).await;
                        } else if let Some(path) = pasted_image_path(&text) {
                            match file_attachment(&path) {
                                Ok(attachment) => draft.attachments.push(attachment),
                                Err(message) => display.output(&format!("\n{YELLOW}{message}{RESET}\n"), terminal_width()),
                            }
                        } else {
                            draft.insert_str(&clean_input(&text));
                        }
                    }
                    Key::Enter if pending_permission => {},
                    Key::Enter => {
                        display.finish_line();
                        let text = std::mem::take(&mut draft.text);
                        draft.cursor = 0;
                        let files = std::mem::take(&mut draft.attachments);
                        if !text.is_empty() { println!("{BOLD}›{RESET} {text}"); }
                        for file in &files { println!("{DIM}● attached: {}{RESET}", clean_input(&file.filename)); }
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
                            if let Err(error) = engine.start_turn(app.clone(), session.id.clone(), request) {
                                println!("{RED}error:{RESET} {error}");
                            }
                        }
                    }
                    Key::Backspace if !pending_permission => { draft.backspace(); }
                    Key::Char(ch) => { draft.insert(ch); }
                    Key::NewLine if !pending_permission && !pending_question => {
                        draft.insert('\n');
                    }
                    Key::Delete if !pending_permission && !pending_question => draft.delete(),
                    Key::Undo if !pending_permission && !pending_question => draft.undo(),
                    Key::KillLine if !pending_permission && !pending_question => {
                        if let Some(killed) = draft.kill_line() {
                            killring.push(killed);
                        }
                    }
                    Key::KillWord if !pending_permission && !pending_question => {
                        if let Some(killed) = draft.kill_word() {
                            killring.push(killed);
                        }
                    }
                    Key::Yank if !pending_permission && !pending_question => {
                        if let Some(text) = killring.top() {
                            draft.insert_str(text);
                        }
                    }
                    Key::Left if !pending_question => draft.left(),
                    Key::Right if !pending_question => draft.right(),
                    Key::WordLeft if !pending_question => draft.word_left(),
                    Key::WordRight if !pending_question => draft.word_right(),
                    Key::Home if !pending_question => draft.home(),
                    Key::End if !pending_question => draft.end(),
                    Key::SelectLeft if !pending_question => draft.select_left(),
                    Key::SelectRight if !pending_question => draft.select_right(),
                    Key::SelectHome if !pending_question => draft.select_home(),
                    Key::SelectEnd if !pending_question => draft.select_end(),
                    Key::SelectWordLeft if !pending_question => draft.select_word_left(),
                    Key::SelectWordRight if !pending_question => draft.select_word_right(),
                    Key::Up(n) if !pending_permission && !pending_question => {
                        // Up on an empty draft recalls previous prompts;
                        // otherwise it moves the caret across display rows.
                        if !prompt_history.select(&mut draft, -(n as isize)) {
                            draft.move_row(&attachment_prefix(&draft), terminal_width(), -(n as isize));
                        }
                    }
                    Key::Down(n) if !pending_permission && !pending_question => {
                        if !prompt_history.select(&mut draft, n as isize) {
                            draft.move_row(&attachment_prefix(&draft), terminal_width(), n as isize);
                        }
                    }
                    Key::Esc if pending_permission => { if let Some(PendingPrompt::Permission(id)) = ui.prompt.take() { app.perms.reply(&id, "reject"); } }
                    _ => {}
                }
                display.draw(&ui, &draft, terminal_width());
            }
            event = events.recv() => {
                display.erase();
                match event {
                    Ok(event) => {
                        let output = render_event(&mut ui, &event, &session.id);
                        display.output(&output, terminal_width());
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => display.output(&format!("\n{YELLOW}⚠ some events were skipped{RESET}\n"), terminal_width()),
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
                display.draw(&ui, &draft, terminal_width());
            }
        }
    }
    display.erase();
    display.finish_line();
    engine.abort_and_wait(&session.id).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        decode_event, file_attachment, image_attachment, parse_answer, pasted_image_path, Key,
    };
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

    pub(super) const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR";

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
    fn typed_numbers_answer_with_their_option_labels() {
        let labels = ["session-rename-notes".to_string(), "scratch".to_string()];
        assert_eq!(parse_answer("1", &labels), ["session-rename-notes"]);
        assert_eq!(
            parse_answer("1  2", &labels),
            ["session-rename-notes", "scratch"]
        );
        // Free text and numbers without an option stay exactly as typed.
        assert_eq!(parse_answer("keep-it-brief", &labels), ["keep-it-brief"]);
        assert_eq!(parse_answer("0 9", &labels), ["0", "9"]);
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
}
