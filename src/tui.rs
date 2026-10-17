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

pub struct RawTerminal;

impl RawTerminal {
    pub fn available() -> bool {
        unsafe { libc::isatty(0) == 1 && libc::isatty(1) == 1 }
    }
}

#[cfg(unix)]
struct RawMode {
    saved: libc::termios,
}

#[cfg(unix)]
impl RawMode {
    fn enter() -> Option<Self> {
        let mut saved = unsafe { std::mem::zeroed::<libc::termios>() };
        if unsafe { libc::tcgetattr(0, &mut saved) } != 0 { return None; }
        let mut raw = saved;
        unsafe { libc::cfmakeraw(&mut raw) };
        if unsafe { libc::tcsetattr(0, libc::TCSANOW, &raw) } != 0 { return None; }
        Some(Self { saved })
    }
}

#[cfg(unix)]
impl Drop for RawMode {
    fn drop(&mut self) {
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
    println!("{DIM}┌ model {model} · agent build · Enter send · Ctrl+C abort · Ctrl+D quit · Ctrl+L clear{RESET}");
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

enum Key { Char(char), Enter, Backspace, Esc, CtrlC, CtrlD, CtrlL, Other }

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
    let (bytes_tx, mut bytes_rx) = mpsc::unbounded_channel::<u8>();
    std::thread::spawn(move || {
        let mut byte = [0u8; 1];
        loop {
            match std::io::Read::read(&mut std::io::stdin(), &mut byte) {
                Ok(0) | Err(_) => { let _ = bytes_tx.send(4); return; }
                Ok(_) => { if bytes_tx.send(byte[0]).is_err() { return; } }
            }
        }
    });

    let mut line = String::new();
    let mut ui = Ui { part_kind: HashMap::new(), headered: std::collections::HashSet::new(), tool_done: std::collections::HashSet::new(), busy: false, prompt: None };
    println!("{BOLD}ocmini{RESET} {DIM}{}{RESET}", crate::OPENCODE_COMPAT_VERSION);
    print_prompt(&model_display);

    loop {
        tokio::select! {
            byte = bytes_rx.recv() => {
                let Some(byte) = byte else { break; };
                let pending_permission = matches!(ui.prompt, Some(PendingPrompt::Permission(_)));
                let pending_question = matches!(ui.prompt, Some(PendingPrompt::Question { .. }));
                match decode_key(byte) {
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
