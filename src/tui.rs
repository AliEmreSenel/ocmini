// The simplest possible terminal UI: an append-only ANSI transcript over the
// in-process engine. It subscribes to the same event bus T3 consumes over SSE,
// so prompts, streaming text, reasoning, tool states, permissions and
// questions all flow through the real runtime paths.
use crate::engine::{self, App, EngineApi};
use crate::engine_impl::Engine;
use crate::types::{ModelRef, PromptRequest, Session};
use serde_json::Value;
use std::collections::HashMap;
use std::io::Write;
use std::sync::Arc;
use tokio::sync::mpsc;

#[cfg(unix)]
struct RawMode {
    saved: libc::termios,
}

// Bracketed paste mode (DEC 2004): while on, terminals wrap pasted text in
// ESC [ 200 ~ … ESC [ 201 ~ so its newlines can be inserted as text instead
// of arriving as Enter keystrokes that send the prompt line by line.
const BRACKETED_PASTE_ON: &[u8] = b"\x1b[?2004h";
const BRACKETED_PASTE_OFF: &[u8] = b"\x1b[?2004l";

/// Raw mode must survive being killed: T3 terminates the TUI with signals,
/// and a terminal left in raw mode is broken for the user's shell.
#[cfg(unix)]
fn install_terminal_restore(saved: &libc::termios) {
    static SAVED: std::sync::OnceLock<libc::termios> = std::sync::OnceLock::new();
    extern "C" fn restore_exit(sig: libc::c_int) -> ! {
        if let Some(saved) = SAVED.get() {
            unsafe { libc::tcsetattr(0, libc::TCSANOW, saved) };
        }
        // write() is async-signal-safe; leave no paste brackets on a dead TUI.
        unsafe { libc::write(1, BRACKETED_PASTE_OFF.as_ptr() as *const libc::c_void, BRACKETED_PASTE_OFF.len()) };
        unsafe { libc::_exit(128 + sig) };
    }
    let _ = SAVED.set(*saved);
    for sig in [libc::SIGHUP, libc::SIGINT, libc::SIGQUIT, libc::SIGTERM] {
        unsafe { libc::signal(sig, restore_exit as libc::sighandler_t) };
    }
}

#[cfg(unix)]
impl RawMode {
    fn enter() -> Option<Self> {
        let mut saved = unsafe { std::mem::zeroed::<libc::termios>() };
        if unsafe { libc::tcgetattr(0, &mut saved) } != 0 { return None; }
        let mut raw = saved;
        unsafe { libc::cfmakeraw(&mut raw) };
        // cfmakeraw clears OPOST, so "\n" stops becoming CRLF and every
        // printed line walks down the screen in stairs. Keep output post-
        // processing on; input stays fully raw.
        raw.c_oflag |= libc::OPOST | libc::ONLCR;
        if unsafe { libc::tcsetattr(0, libc::TCSANOW, &raw) } != 0 { return None; }
        let _ = std::io::stdout().write_all(BRACKETED_PASTE_ON);
        let _ = std::io::stdout().flush();
        install_terminal_restore(&saved);
        Some(Self { saved })
    }
}

#[cfg(unix)]
impl Drop for RawMode {
    fn drop(&mut self) {
        let _ = std::io::stdout().write_all(BRACKETED_PASTE_OFF);
        let _ = std::io::stdout().flush();
        unsafe { libc::tcsetattr(0, libc::TCSANOW, &self.saved) };
        println!();
    }
}

#[cfg(not(unix))]
struct RawMode;
#[cfg(not(unix))]
impl RawMode {
    fn enter() -> Option<Self> { Some(Self) }
}

const RESET: &str = "\x1b[0m";
const DIM: &str = "\x1b[2m";
const BOLD: &str = "\x1b[1m";
const GREEN: &str = "\x1b[32m";
const RED: &str = "\x1b[31m";
const YELLOW: &str = "\x1b[33m";
const CYAN: &str = "\x1b[36m";

fn truncate(text: &str, max: usize) -> String {
    let mut out: String = text.chars().take(max).collect();
    if out.chars().count() < text.chars().count() { out.push('…'); }
    out.replace('\n', " ")
}

struct Ui {
    part_kind: HashMap<String, &'static str>,   // partID -> text | reasoning
    headered: std::collections::HashSet<String>, // part ids that printed their header
    tool_done: std::collections::HashSet<String>,
    busy: bool,
    // Prompt interaction routed from the terminal: lines when idle, keys while a request is pending.
    prompt: Option<PendingPrompt>,
}

enum PendingPrompt {
    Permission(String),
    Question { id: String, questions: usize, answers: Vec<Vec<String>>, line: String },
}

impl Drop for Ui {
    fn drop(&mut self) {
        use std::io::Write;
        let _ = std::io::stdout().flush();
    }
}

fn print_prompt(model: &str) {
    println!("{DIM}┌ model {model} · agent build · Enter send · paste inserts all lines · Ctrl+C abort · Ctrl+D quit · Ctrl+L clear{RESET}");
    print!("{BOLD}›{RESET} ");
    let _ = std::io::stdout().flush();
}

fn summarize_input(input: &Value) -> String {
    let mut parts = Vec::new();
    if let Some(command) = input["command"].as_str() { parts.push(command.to_string()); }
    if let Some(path) = input["filePath"].as_str() { parts.push(path.to_string()); }
    if let Some(pattern) = input["pattern"].as_str() { parts.push(pattern.to_string()); }
    if let Some(url) = input["url"].as_str() { parts.push(url.to_string()); }
    if let Some(prompt) = input["prompt"].as_str() { parts.push(prompt.to_string()); }
    if parts.is_empty() { parts.push(input.to_string()); }
    truncate(&parts.join(" "), 120)
}

fn render_event(ui: &mut Ui, model: &str, event: &crate::types::BusEvent, session_id: &str) {
    let properties = &event.properties;
    if properties["sessionID"].as_str().is_some_and(|id| id != session_id) { return; }
    match event.r#type.as_str() {
        "message.part.updated" => {
            let part = &properties["part"];
            let part_id = part["id"].as_str().unwrap_or_default().to_string();
            match part["type"].as_str() {
                Some("text") | Some("reasoning") => {
                    let kind = if part["type"] == "text" { "text" } else { "reasoning" };
                    let is_new = ui.part_kind.insert(part_id.clone(), kind).is_none();
                    if is_new && ui.headered.insert(part_id.clone()) {
                        if kind == "reasoning" { println!("\n{DIM}◦ thinking{RESET}"); }
                        else { println!("\n{BOLD}assistant{RESET}"); }
                    }
                }
                Some("tool") => {
                    let tool = part["tool"].as_str().unwrap_or("tool");
                    let state = &part["state"];
                    if ui.headered.insert(format!("{part_id}:start")) {
                        println!("\n{CYAN}● {tool}{RESET} {DIM}{}{RESET}", summarize_input(&state["input"]));
                    }
                    let status = state["status"].as_str().unwrap_or_default();
                    if (status == "completed" || status == "error") && ui.tool_done.insert(part_id) {
                        if status == "completed" {
                            let output = state["output"].as_str().unwrap_or_default();
                            println!("  {GREEN}✓{RESET} {DIM}{}{RESET}", truncate(output, 160));
                        } else {
                            println!("  {RED}✗{RESET} {}", truncate(state["error"].as_str().unwrap_or("failed"), 160));
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
            println!("\n{YELLOW}⚠ permission{}: {}{RESET} {DIM}[o]nce [a]lways [r]eject{RESET}",
                properties["permission"].as_str().map(|p| format!(" {p}")).unwrap_or_default(),
                truncate(properties["patterns"].as_array().map(|list| list.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", ")).unwrap_or_default().as_str(), 120));
            print!("{BOLD}›{RESET} ");
            let _ = std::io::stdout().flush();
            ui.prompt = Some(PendingPrompt::Permission(id));
        }
        "question.asked" => {
            let id = properties["id"].as_str().unwrap_or_default().to_string();
            let mut questions = 0;
            if let Some(list) = properties["questions"].as_array() {
                questions = list.len();
                for question in list {
                    println!("\n{YELLOW}? {}{RESET}", question["question"].as_str().unwrap_or_default());
                    for (index, option) in question["options"].as_array().map(|o| o.as_slice()).unwrap_or(&[][..]).iter().enumerate() {
                        println!("  {CYAN}{}{RESET}. {} {DIM}{}{RESET}", index + 1, option["label"].as_str().unwrap_or_default(), option["description"].as_str().unwrap_or_default());
                    }
                }
                println!("{DIM}type number(s), Enter to answer, Esc to dismiss{RESET}");
            }
            print!("{BOLD}›{RESET} ");
            let _ = std::io::stdout().flush();
            ui.prompt = Some(PendingPrompt::Question { id, questions, answers: Vec::new(), line: String::new() });
        }
        "session.error" => {
            let message = properties["error"]["data"]["message"].as_str().unwrap_or("unknown error");
            println!("\n{RED}error:{RESET} {message}");
        }
        "session.idle" => {
            println!("{DIM}└ idle{RESET}");
            print_prompt(model);
        }
        _ => {}
    }
}

fn answer_question(app: &App, state: &mut PendingPrompt, key: Key) -> bool {
    // True when the prompt finished and the normal input prompt should return.
    let PendingPrompt::Question { id, questions, answers, line } = state else { return false; };
    match key {
        Key::Esc => { app.questions.reject(id); *state = PendingPrompt::Permission(String::new()); return true; }
        Key::Enter => {
            let choices: Vec<String> = line.split_ascii_whitespace().map(|token| {
                match token.parse::<usize>() {
                    Ok(number) if number >= 1 => (number - 1).to_string(),
                    _ => token.to_string(),
                }
            }).collect();
            line.clear();
            if !choices.is_empty() { answers.push(choices); }
            if answers.len() >= *questions {
                let payload = std::mem::take(answers);
                let _ = app.questions.reply(id, payload);
                *state = PendingPrompt::Permission(String::new());
                return true;
            }
            print_prompt("");
            false
        }
        Key::Backspace => { line.pop(); print!("\x08 \x08"); let _ = std::io::stdout().flush(); false }
        Key::Char(ch) => { line.push(ch); print!("{ch}"); let _ = std::io::stdout().flush(); false }
        _ => false,
    }
}

enum Key { Char(char), Enter, Backspace, Esc, CtrlC, CtrlD, CtrlL, Paste(String), Other }

const PASTE_START: &[u8] = b"\x1b[200~";
const PASTE_END: &[u8] = b"\x1b[201~";

/// When `body` contains the closing paste marker, return the payload with
/// CRLF/CR normalized to LF. Lone escapes that merely prefix the terminator
/// (e.g. "\x1b[0m" typed into a paste) stay part of the text.
fn finished_paste(body: &[u8]) -> Option<String> {
    let end = body.windows(PASTE_END.len()).position(|w| w == PASTE_END)?;
    let text = String::from_utf8_lossy(&body[..end]).into_owned();
    Some(text.replace("\r\n", "\n").replace('\r', "\n"))
}

fn read_paste(stdin: &mut std::io::Stdin) -> Option<String> {
    let mut body: Vec<u8> = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        if std::io::Read::read(stdin, &mut byte).is_err() { return None; }
        body.push(byte[0]);
        if let Some(text) = finished_paste(&body) { return Some(text); }
    }
}

fn decode_key(byte: u8) -> Key {
    match byte {
        3 => Key::CtrlC,
        4 => Key::CtrlD,
        12 => Key::CtrlL,
        10 | 13 => Key::Enter,
        8 | 127 => Key::Backspace,
        27 => Key::Esc,
        32..=126 => Key::Char(byte as char),
        _ => Key::Other,
    }
}

fn stdin_readable(timeout_ms: i32) -> bool {
    let mut fd = libc::pollfd { fd: 0, events: libc::POLLIN, revents: 0 };
    unsafe { libc::poll(&mut fd, 1, timeout_ms) > 0 }
}

/// Raw mode delivers raw bytes: multibyte UTF-8 must be reassembled and
/// arrow/home/pgup escape sequences must be consumed whole, or their
/// "[A" tails get typed into the prompt as text.
fn read_stdin_keys(tx: mpsc::UnboundedSender<Key>) {
    let mut stdin = std::io::stdin();
    let mut byte = [0u8; 1];
    loop {
        match std::io::Read::read(&mut stdin, &mut byte) {
            Ok(0) | Err(_) => { let _ = tx.send(Key::CtrlD); return; }
            Ok(_) => {}
        }
        match byte[0] {
            27 => {
                // Escape only starts a sequence if bytes follow it; a lone
                // Esc (dismiss prompts) must not swallow the next keystroke.
                if !stdin_readable(30) { let _ = tx.send(Key::Esc); continue; }
                if std::io::Read::read(&mut stdin, &mut byte).is_err() { continue; }
                if matches!(byte[0], b'[' | b'O') {
                    let mut seq = vec![27u8, byte[0]];
                    for _ in 0..8 {
                        if std::io::Read::read(&mut stdin, &mut byte).is_err() { break; }
                        seq.push(byte[0]);
                        if (0x40..=0x7e).contains(&byte[0]) { break; }
                    }
                    if seq == PASTE_START {
                        // A whole paste arrives as one Key: its newlines are
                        // text, never Enter keystrokes that send early.
                        if let Some(text) = read_paste(&mut stdin) {
                            let _ = tx.send(Key::Paste(text));
                        }
                    }
                } else {
                    let _ = tx.send(Key::Esc);
                    let _ = tx.send(decode_key(byte[0]));
                }
            }
            lead @ 0xC2..=0xF4 => {
                let extra = match lead { 0xC2..=0xDF => 1, 0xE0..=0xEF => 2, _ => 3 };
                let mut seq = vec![lead];
                for _ in 0..extra {
                    if std::io::Read::read(&mut stdin, &mut byte).is_err() { break; }
                    seq.push(byte[0]);
                }
                if let Ok(text) = String::from_utf8(seq) {
                    for ch in text.chars() { let _ = tx.send(Key::Char(ch)); }
                }
            }
            other => { let _ = tx.send(decode_key(other)); }
        }
    }
}

pub async fn run(model: Option<String>, directory: Option<String>) -> Result<(), String> {
    let app = App::new()?;
    let directory = directory.unwrap_or_else(|| std::env::current_dir().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default());
    let app = app.for_directory(&directory)?;
    let engine: Arc<dyn EngineApi> = Arc::new(Engine::new());
    let session: Session = app.store.try_create_session_owned("tui session", &directory)?;
    let model_display = model.clone().or_else(|| Some(app.config.lock().unwrap().model.clone())).filter(|m| !m.is_empty()).unwrap_or_else(|| "unconfigured".into());
    let model_ref = model.as_deref().and_then(|text| text.split_once('/')).map(|(provider, id)| ModelRef { provider_id: provider.into(), model_id: id.into() });

    let raw = RawMode::enter();
    let mut events = engine::bus().subscribe();
    let (keys_tx, mut keys_rx) = mpsc::unbounded_channel::<Key>();
    std::thread::spawn(move || read_stdin_keys(keys_tx));

    let mut line = String::new();
    let mut ui = Ui { part_kind: HashMap::new(), headered: std::collections::HashSet::new(), tool_done: std::collections::HashSet::new(), busy: false, prompt: None };
    println!("{BOLD}ocmini{RESET} {DIM}{}{RESET}", crate::OPENCODE_COMPAT_VERSION);
    print_prompt(&model_display);

    loop {
        tokio::select! {
            key = keys_rx.recv() => {
                let Some(key) = key else { break; };
                let pending_permission = matches!(ui.prompt, Some(PendingPrompt::Permission(_)));
                let pending_question = matches!(ui.prompt, Some(PendingPrompt::Question { .. }));
                match key {
                    Key::CtrlD => break,
                    Key::CtrlC if pending_permission => { if let Some(PendingPrompt::Permission(id)) = ui.prompt.take() { app.perms.reply(&id, "reject"); } }
                    Key::CtrlC => { engine.abort(&session.id); println!("\n{YELLOW}⏹ aborted{RESET}"); }
                    Key::CtrlL => print!("\x1b[2J\x1b[H"),
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
                        for ch in text.chars() {
                            line.push(ch);
                            // Continuation lines indent under "› ".
                            if ch == '\n' { print!("\n  "); } else { print!("{ch}"); }
                        }
                        let _ = std::io::stdout().flush();
                    }
                    Key::Enter => {
                        let text = std::mem::take(&mut line);
                        println!();
                        if text.trim() == "/quit" { break; }
                        if !text.trim().is_empty() {
                            ui.busy = true;
                            let request = PromptRequest {
                                message_id: Some(crate::types::hex_id("msg_")),
                                model: model_ref.clone(),
                                agent: Some("build".into()),
                                parts: vec![serde_json::json!({"type": "text", "text": text})],
                                ..Default::default()
                            };
                            if let Err(error) = engine.start_turn(app.clone(), session.id.clone(), request) {
                                println!("{RED}error:{RESET} {error}");
                                ui.busy = false;
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
    drop(raw);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::finished_paste;

    #[test]
    fn paste_payload_stops_at_the_closing_bracket_and_normalizes_newlines() {
        // The trailing bytes belong to later keystrokes, not the payload.
        assert_eq!(
            finished_paste(b"first\r\nsecond\rthird\x1b[201~\x1b[3~").as_deref(),
            Some("first\nsecond\nthird")
        );
    }

    #[test]
    fn paste_is_incomplete_until_the_closing_bracket_arrives() {
        assert_eq!(finished_paste(b"first\r"), None);
        assert_eq!(finished_paste(b"a\x1b[201"), None);
    }

    #[test]
    fn lone_escapes_inside_a_paste_stay_in_the_payload() {
        assert_eq!(finished_paste(b"a\x1b[0mb\x1b[201~").as_deref(), Some("a\x1b[0mb"));
    }
}
