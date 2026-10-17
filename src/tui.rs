// Terminal transcript and input over the shared session engine.
use crate::engine::{self, App, EngineApi};
use crate::engine_impl::Engine;
use crate::types::{MessageWithParts, PromptRequest};
use crossterm::{
    event::{
        DisableBracketedPaste, EnableBracketedPaste, Event, EventStream, KeyCode, KeyEventKind,
        KeyModifiers,
    },
    terminal,
};
use futures::{poll, StreamExt};
use serde_json::Value;
use std::collections::HashMap;
use std::io::{IsTerminal, Write};
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
    () => { print!("\n") };
    ($($arg:tt)*) => { print!("{}\n", format!($($arg)*)) };
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
    println!("{DIM}┌ model {model} · Enter send · paste inserts all lines · Ctrl+V paste image · Ctrl+C abort · Ctrl+D quit · Ctrl+L clear · Ctrl+R sessions{RESET}");
    print!("{BOLD}›{RESET} ");
    let _ = std::io::stdout().flush();
}

fn summarize_input(input: &Value) -> String {
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

fn render_event(ui: &mut Ui, model: &str, event: &crate::types::BusEvent, session_id: &str) {
    let properties = &event.properties;
    if properties["sessionID"]
        .as_str()
        .or_else(|| properties["part"]["sessionID"].as_str())
        .is_some_and(|id| id != session_id)
    {
        return;
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
            let _ = std::io::stdout().flush();
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
            print!("{BOLD}›{RESET} ");
            let _ = std::io::stdout().flush();
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
            print!("{BOLD}›{RESET} ");
            let _ = std::io::stdout().flush();
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
            println!("{DIM}└ idle{RESET}");
            print_prompt(model);
        }
        _ => {}
    }
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
            print_prompt("");
            false
        }
        Key::Backspace => {
            line.pop();
            print!("\x08 \x08");
            let _ = std::io::stdout().flush();
            false
        }
        Key::Char(ch) => {
            line.push(ch);
            print!("{ch}");
            let _ = std::io::stdout().flush();
            false
        }
        _ => false,
    }
}

pub(crate) enum Key {
    Char(char),
    Enter,
    Backspace,
    Esc,
    CtrlC,
    CtrlD,
    CtrlL,
    CtrlR,
    CtrlV,
    /// Move by N rows; page keys report a full page.
    Up(usize),
    Down(usize),
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
            (KeyCode::Char('v'), KeyModifiers::CONTROL) => Key::CtrlV,
            (KeyCode::Enter, _) => Key::Enter,
            (KeyCode::Backspace, _) => Key::Backspace,
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

/// A prompt part waiting in the input: an existing image file or clipboard
/// bytes copied to a temp file. The engine reads file: URLs when it builds the
/// request, so nothing is loaded until the prompt is sent.
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

/// Clipboard bytes become a file: attachment only when they are a supported
/// image and fit the engine's size limit.
fn image_attachment(bytes: Vec<u8>) -> Option<Attachment> {
    let mime = crate::tools::media_type(&bytes).filter(|mime| mime.starts_with("image/"))?;
    if bytes.len() as u64 > MAX_ATTACHMENT {
        return None;
    }
    let ext = mime.rsplit('/').next().unwrap_or("png");
    let path =
        std::env::temp_dir().join(format!("ocmini-paste-{}.{}", crate::types::hex_id(""), ext));
    std::fs::write(&path, &bytes).ok()?;
    Some(Attachment {
        filename: format!("clipboard.{ext}"),
        url: reqwest::Url::from_file_path(&path).ok()?.to_string(),
        mime,
    })
}

/// A paste that is only a quoted, file: or ~/ path to an image file. Many
/// terminals paste dragged or copied files as paths, so those become
/// attachments; anything else stays plain text.
fn pasted_image_path(text: &str) -> Option<PathBuf> {
    let trimmed = text.trim().trim_matches('"').trim_matches('\'');
    let path = trimmed.strip_prefix("file://").unwrap_or(trimmed);
    let expanded = match path.strip_prefix("~/") {
        Some(rest) => std::env::var_os("HOME").map(|home| PathBuf::from(home).join(rest))?,
        None => PathBuf::from(path),
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

fn print_attachment(attachment: &Attachment) {
    println!(
        "{DIM}● attached: {} · {}{RESET}",
        attachment.filename, attachment.mime
    );
}

#[cfg(target_os = "linux")]
fn clipboard_image() -> Result<Option<Attachment>, String> {
    use std::process::Command;
    let mut probed = false;
    if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        probed = true;
        // -n keeps binary payloads byte-exact; wl-paste appends a newline without it.
        if let Ok(out) = Command::new("wl-paste")
            .args(["-t", "image/png", "-n"])
            .output()
        {
            if out.status.success() {
                if let Some(attachment) = image_attachment(out.stdout) {
                    return Ok(Some(attachment));
                }
            }
        }
    }
    if std::env::var_os("DISPLAY").is_some() {
        probed = true;
        if let Ok(out) = Command::new("xclip")
            .args(["-selection", "clipboard", "-t", "image/png", "-o"])
            .output()
        {
            if out.status.success() {
                if let Some(attachment) = image_attachment(out.stdout) {
                    return Ok(Some(attachment));
                }
            }
        }
    }
    if !probed {
        return Err("Image paste needs wl-paste (Wayland) or xclip (X11)".into());
    }
    Ok(None)
}

#[cfg(target_os = "macos")]
fn clipboard_image() -> Result<Option<Attachment>, String> {
    use std::process::Command;
    let file = std::env::temp_dir().join(format!("ocmini-clip-{}.png", crate::types::hex_id("")));
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
        .output()
        .map_err(|e| format!("Cannot run osascript: {e}"))?;
    let bytes = std::fs::read(&file).ok();
    let _ = std::fs::remove_file(&file);
    if !out.status.success() {
        return Ok(None);
    }
    Ok(bytes.and_then(image_attachment))
}

#[cfg(windows)]
fn clipboard_image() -> Result<Option<Attachment>, String> {
    use std::process::Command;
    let file = std::env::temp_dir().join(format!("ocmini-paste-{}.png", crate::types::hex_id("")));
    let script = format!(
        "Add-Type -AssemblyName System.Windows.Forms; if ($i = [System.Windows.Forms.Clipboard]::GetImage()) {{ $i.Save('{}') }}",
        file.display()
    );
    let out = Command::new("powershell")
        .args(["-NoProfile", "-Command", &script])
        .output()
        .map_err(|e| format!("Cannot run powershell: {e}"))?;
    let bytes = std::fs::read(&file).ok();
    let _ = std::fs::remove_file(&file);
    if !out.status.success() {
        return Ok(None);
    }
    Ok(bytes.and_then(image_attachment))
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn clipboard_image() -> Result<Option<Attachment>, String> {
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

fn display_model(model_ref: Option<&crate::types::ModelRef>, config_model: &str) -> String {
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

    let mut line = String::new();
    let mut attachments: Vec<Attachment> = Vec::new();
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

    loop {
        tokio::select! {
            _ = &mut shutdown => break,
            event = keys.next() => {
                let Some(event) = event else { break; };
                let key = decode_event(event.map_err(|error| error.to_string())?);
                let pending_permission = matches!(ui.prompt, Some(PendingPrompt::Permission(_)));
                let pending_question = matches!(ui.prompt, Some(PendingPrompt::Question { .. }));
                match key {
                    Key::CtrlD => break,
                    Key::CtrlC if pending_permission => { if let Some(PendingPrompt::Permission(id)) = ui.prompt.take() { app.perms.reply(&id, "reject"); } }
                    Key::CtrlC => { engine.abort(&session.id); println!("\n{YELLOW}⏹ aborted{RESET}"); }
                    Key::CtrlL => print!("\x1b[2J\x1b[H"),
                    Key::CtrlV => {
                        match clipboard_image() {
                            Ok(Some(attachment)) => {
                                print_attachment(&attachment);
                                attachments.push(attachment);
                            }
                            Ok(None) => println!("{DIM}clipboard has no image{RESET}"),
                            Err(message) => println!("{YELLOW}{message}{RESET}"),
                        }
                        print_prompt(&model_display);
                    }
                    Key::CtrlR if !pending_permission && !pending_question => {
                        print!("\x1b[J");
                        let picked = crate::picker::select(&app.store, None, &mut keys).await?;
                        // The picker reads stdin through this same stream; keys typed
                        // while it was open are queued here and must not reach the prompt.
                        while let Poll::Ready(Some(_)) = poll!(keys.next()) {}
                        match picked {
                            None => print_prompt(&model_display),
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
                                        line.clear();
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
                                'o' | 'O' => { app.perms.reply(&id, "once"); println!("{DIM}once{RESET}"); }
                                'a' | 'A' => { app.perms.reply(&id, "always"); println!("{DIM}always{RESET}"); }
                                _ => { app.perms.reply(&id, "reject"); println!("{DIM}rejected{RESET}"); }
                            }
                        }
                    }
                    key @ (Key::Char(_) | Key::Enter | Key::Backspace | Key::Esc) if pending_question => {
                        if let Some(state) = ui.prompt.as_mut() {
                            if answer_question(&app, state, key) {
                                ui.prompt = None;
                                print_prompt(&model_display);
                            }
                        }
                    }
                    Key::Paste(text) if pending_question => {
                        if let Some(PendingPrompt::Question { line, .. }) = ui.prompt.as_mut() {
                            let flat = text.replace('\n', " ");
                            line.push_str(&flat);
                            print!("{flat}");
                            let _ = std::io::stdout().flush();
                        }
                    }
                    Key::Paste(_) if pending_permission => {}
                    Key::Paste(text) => {
                        // A pasted image file path becomes an attachment; a
                        // pasted screenshot can never arrive as text, so the
                        // clipboard reader above covers that case.
                        let pasted = pasted_image_path(&text);
                        let image = pasted.as_deref().map(file_attachment);
                        match image {
                            Some(Ok(attachment)) => {
                                print_attachment(&attachment);
                                attachments.push(attachment);
                                print_prompt(&model_display);
                            }
                            Some(Err(message)) => {
                                println!("{YELLOW}{message}{RESET}");
                                print_prompt(&model_display);
                            }
                            None => {
                                for ch in text.chars() {
                                    line.push(ch);
                                    // Continuation lines indent under "› ".
                                    if ch == '\n' { print!("\n  "); } else { print!("{ch}"); }
                                }
                            }
                        }
                        let _ = std::io::stdout().flush();
                    }
                    Key::Enter => {
                        let text = std::mem::take(&mut line);
                        let files = std::mem::take(&mut attachments);
                        println!();
                        if text.trim() == "/quit" { break; }
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
                        print_prompt(&model_display);
                    }
                    Key::Backspace => { line.pop(); print!("\x08 \x08"); let _ = std::io::stdout().flush(); }
                    Key::Char(ch) => { line.push(ch); print!("{ch}"); let _ = std::io::stdout().flush(); }
                    Key::Esc if pending_permission => { if let Some(PendingPrompt::Permission(id)) = ui.prompt.take() { app.perms.reply(&id, "reject"); } }
                    _ => {}
                }
            }
            event = events.recv() => {
                match event {
                    Ok(event) => render_event(&mut ui, &model_display, &event, &session.id),
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => println!("\n{YELLOW}⚠ some events were skipped{RESET}"),
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }
    engine.abort_and_wait(&session.id).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{file_attachment, image_attachment, parse_answer, pasted_image_path};

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR";

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
    fn clipboard_bytes_attach_only_as_images() {
        let attachment = image_attachment(PNG.to_vec()).unwrap();
        assert_eq!(attachment.filename, "clipboard.png");
        assert_eq!(attachment.mime, "image/png");
        std::fs::remove_file(std::path::Path::new(
            &attachment.url.strip_prefix("file://").unwrap(),
        ))
        .unwrap();
        assert!(image_attachment(b"not an image".to_vec()).is_none());
    }
}
