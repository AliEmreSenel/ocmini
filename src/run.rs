// One-shot prompt: `ocmini run [MESSAGE..]`, modeled on `opencode run`.
// The turn runs on the same in-process engine as the TUI; events from the
// bus are rendered as text (agent header, tool lines, streamed answer) or
// as JSONL (`--format json`).

use crate::engine::{bus, App, EngineApi};
use crate::engine_impl::Engine;
use crate::sessions;
use crate::tui;
use crate::types::{self, BusEvent, PromptRequest};
use base64::Engine as Base64Engine;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::io::{IsTerminal, Read, Write};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::{broadcast, oneshot};

enum Pending {
    Permission(String),
    Question(String, usize),
}
pub async fn run(args: crate::RunArgs) -> Result<(), String> {
    let app = App::new()?;
    let mut session = if args.session.is_some() || args.continue_last {
        sessions::open_for_app(
            &app,
            args.dir.as_deref(),
            args.session.as_deref(),
            args.continue_last,
        )
        .await?
    } else {
        let directory = sessions::directory(args.dir.as_deref().unwrap_or("."))?;
        sessions::create_owned(
            &app,
            args.title.as_deref().unwrap_or("run"),
            &directory,
            None,
            None,
        )
        .await?
    };
    if args.fork {
        session = sessions::fork(&app, &session, None).await?;
    }
    sessions::reject_correlated_reentry(&session.id)?;
    let app = app.for_directory(&session.directory)?;

    let mut text = args.message.join(" ");
    if !std::io::stdin().is_terminal() {
        let mut piped = String::new();
        std::io::stdin()
            .read_to_string(&mut piped)
            .map_err(|error| format!("Cannot read stdin: {error}"))?;
        let piped = piped.trim();
        if !piped.is_empty() {
            text = if text.is_empty() {
                piped.to_owned()
            } else {
                format!("{text}\n\n{piped}")
            };
        }
    }
    let mut parts: Vec<Value> = Vec::new();
    if !text.trim().is_empty() {
        parts.push(json!({"type":"text","text":text}));
    }
    for path in &args.file {
        parts.push(file_part(path)?);
    }
    if parts.is_empty() && args.command.is_none() {
        return Err("No prompt: pass a message, pipe stdin, or attach a file".into());
    }

    let user_mid = types::hex_id("msg_");
    let mut request = PromptRequest {
        message_id: Some(user_mid.clone()),
        model: args.model.clone(),
        parts,
        agent: args.agent.clone(),
        variant: args.variant.clone(),
        ..Default::default()
    };
    if let Some(command) = &args.command {
        request = crate::commands::prepare(&app.config.lock().unwrap(), command, &text, request)?;
    }
    let json = args.format == "json";
    let tty = std::io::stdout().is_terminal();
    if !json {
        let agent = request.agent.as_deref().unwrap_or("build");
        let model = tui::display_model(request.model.as_ref(), &app.config.lock().unwrap().model);
        println!("> {agent} · {model}");
    }
    let engine = Engine::new();
    let mut events = bus().subscribe();
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let result = pump(
        &app,
        &args,
        json,
        tty,
        &session.id,
        &user_mid,
        engine
            .admit(app.clone(), session.id.clone(), request)
            .await?,
        &mut events,
        &mut lines,
    )
    .await;
    match result {
        Ok(true) if !json => println!(),
        Ok(_) => {}
        Err(error) => {
            engine.abort_and_wait(&session.id).await;
            return Err(error);
        }
    }

    if args.interactive {
        // Follow-up prompts share the session; the TUI is the full-featured
        // interactive mode, this loop covers terminal-only workflows.
        loop {
            let Some(line) = lines.next_line().await.map_err(|e| e.to_string())? else {
                return Ok(());
            };
            if line.trim().is_empty() {
                continue;
            }
            if line.trim() == "/exit" {
                return Ok(());
            }
            let user_mid = types::hex_id("msg_");
            let request = PromptRequest {
                message_id: Some(user_mid.clone()),
                model: args.model.clone(),
                parts: vec![json!({"type":"text","text":line})],
                agent: args.agent.clone(),
                variant: args.variant.clone(),
                ..Default::default()
            };
            let printed = pump(
                &app,
                &args,
                json,
                tty,
                &session.id,
                &user_mid,
                engine
                    .admit(app.clone(), session.id.clone(), request)
                    .await?,
                &mut events,
                &mut lines,
            )
            .await;
            let printed = match printed {
                Ok(printed) => printed,
                Err(error) => {
                    engine.abort_and_wait(&session.id).await;
                    return Err(error);
                }
            };
            if printed && !json {
                println!();
            }
        }
    }
    Ok(())
}

/// Streams one turn: renders bus events, answers permission prompts, and
/// resolves when the turn's completion receiver fires. The bus is drained
/// eagerly each iteration (a finished turn can queue events faster than any
/// poll cadence), so rendering never lags the completion signal.
#[allow(clippy::too_many_arguments)]
struct State {
    printed: HashMap<String, usize>,
    headered: HashSet<String>,
    printed_answer: bool,
    turn_error: Option<String>,
    pending: Option<Pending>,
    asking: bool,
}

impl State {
    fn set_pending(&mut self, request: Pending) {
        self.pending = Some(request);
        self.asking = true;
    }
}

#[allow(clippy::too_many_arguments)]
fn on_event(
    st: &mut State,
    app: &App,
    args: &crate::RunArgs,
    json: bool,
    tty: bool,
    session_id: &str,
    user_mid: &str,
    event: Arc<BusEvent>,
) {
    if event.properties["sessionID"]
        .as_str()
        .is_some_and(|id| id != session_id)
        || event.properties["part"]["sessionID"]
            .as_str()
            .is_some_and(|id| id != session_id)
    {
        return;
    }
    match event.r#type.as_str() {
        "message.part.updated" => {
            let part = &event.properties["part"];
            if part["messageID"].as_str() == Some(user_mid) {
                return;
            }
            let id = part["id"].as_str().unwrap_or_default().to_string();
            let full = part["text"].as_str().unwrap_or("");
            match part["type"].as_str() {
                Some("step-start") if json => {
                    println!("{}", json!({"type": "step_start"}));
                }
                Some("step-finish") if json => {
                    println!(
                        "{}",
                        json!({"type": "step_finish", "sessionID": session_id})
                    );
                }
                Some("tool") => {
                    if !json {
                        if st.headered.insert(id) {
                            println!(
                                "\u{2192} {} {}",
                                part["tool"].as_str().unwrap_or("tool"),
                                tui::summarize_input(&part["state"]["input"])
                            );
                        }
                    } else if part["state"]["status"].as_str() == Some("completed")
                        && !part["state"]["input"].is_null()
                    {
                        println!(
                            "{}",
                            json!({"type": "tool_use", "tool": part["tool"], "id": id, "input": part["state"]["input"]})
                        );
                    }
                }
                Some(kind @ ("text" | "reasoning")) => {
                    if json {
                        if part["time"]["end"].is_number()
                            && !full.is_empty()
                            && (kind == "text" || args.thinking)
                        {
                            let name = if kind == "text" { "text" } else { "reasoning" };
                            println!("{}", json!({"type": name, "text": full}));
                        }
                    } else {
                        let shown = st.printed.entry(id).or_insert(0);
                        let suffix = match full.get(*shown..) {
                            Some(suffix) => suffix,
                            None => {
                                // A failed attempt's streamed part is reset before
                                // retrying. Terminal output cannot be taken back.
                                if *shown > 0 && (kind == "text" || args.thinking) {
                                    println!("\n[partial response discarded; retrying]");
                                }
                                full
                            }
                        };
                        *shown = full.len();
                        if suffix.is_empty() {
                            return;
                        }
                        match kind {
                            "text" => {
                                print!("{suffix}");
                                st.printed_answer = true;
                            }
                            "reasoning" if args.thinking => {
                                print!("\x1b[2m{suffix}\x1b[0m");
                            }
                            _ => {}
                        }
                        let _ = std::io::stdout().flush();
                    }
                }
                _ => {}
            }
        }
        "permission.asked" => {
            let id = event.properties["id"].as_str().unwrap_or_default();
            if args.auto {
                app.perms.reply(id, "always");
            } else if tty {
                println!(
                    "\u{26a0} allow {}: {}? [y] once [a]lways [n]o",
                    event.properties["permission"].as_str().unwrap_or("tool"),
                    event.properties["patterns"]
                        .as_array()
                        .map(|list| list
                            .iter()
                            .filter_map(Value::as_str)
                            .collect::<Vec<_>>()
                            .join(", "))
                        .unwrap_or_default()
                );
                let _ = std::io::stdout().flush();
                st.set_pending(Pending::Permission(id.to_string()));
            } else {
                // Non-interactive: refuse rather than hang.
                app.perms.reply(id, "reject");
            }
        }
        "question.asked" => {
            if tty {
                if let Some(questions) = event.properties["questions"].as_array() {
                    for question in questions {
                        println!(
                            "? {} ({})",
                            question["question"].as_str().unwrap_or(""),
                            question["options"]
                                .as_array()
                                .map(|list| list
                                    .iter()
                                    .filter_map(|option| option["label"].as_str())
                                    .collect::<Vec<_>>()
                                    .join(" | "))
                                .unwrap_or_default()
                        );
                    }
                }
                let _ = std::io::stdout().flush();
                let count = event.properties["questions"].as_array().map_or(1, Vec::len);
                st.set_pending(Pending::Question(
                    event.properties["id"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string(),
                    count,
                ));
            } else {
                app.questions
                    .reject(event.properties["id"].as_str().unwrap_or_default());
            }
        }
        "session.error" => {
            st.turn_error = Some(
                event.properties["error"]["data"]["message"]
                    .as_str()
                    .unwrap_or("unknown error")
                    .to_string(),
            );
        }
        _ => {}
    }
}

/// Applies every event already queued; events emitted synchronously before
/// the completion receiver fires are processed before pump reports done.
#[allow(clippy::too_many_arguments)]
fn drain(
    st: &mut State,
    app: &App,
    args: &crate::RunArgs,
    json: bool,
    tty: bool,
    session_id: &str,
    user_mid: &str,
    events: &mut broadcast::Receiver<Arc<BusEvent>>,
) -> Result<(), String> {
    loop {
        match events.try_recv() {
            Ok(event) => on_event(st, app, args, json, tty, session_id, user_mid, event),
            Err(broadcast::error::TryRecvError::Lagged(_)) => continue,
            Err(broadcast::error::TryRecvError::Closed) => {
                return Err("Event bus closed".to_string());
            }
            Err(broadcast::error::TryRecvError::Empty) => return Ok(()),
        }
    }
}

/// Streams one turn: renders bus events, answers permission prompts, and
/// resolves when the turn's completion receiver fires.
#[allow(clippy::too_many_arguments)]
async fn pump(
    app: &App,
    args: &crate::RunArgs,
    json: bool,
    tty: bool,
    session_id: &str,
    user_mid: &str,
    done: oneshot::Receiver<Value>,
    events: &mut broadcast::Receiver<Arc<BusEvent>>,
    lines: &mut tokio::io::Lines<BufReader<tokio::io::Stdin>>,
) -> Result<bool, String> {
    let mut st = State {
        printed: HashMap::new(),
        headered: HashSet::new(),
        printed_answer: false,
        turn_error: None,
        pending: None,
        asking: false,
    };
    let mut done = Box::pin(done);
    let mut completion: Option<Result<Value, oneshot::error::RecvError>> = None;
    loop {
        drain(&mut st, app, args, json, tty, session_id, user_mid, events)?;
        if let Some(response) = completion.take() {
            response.map_err(|_| {
                st.turn_error
                    .take()
                    .unwrap_or_else(|| "Turn ended without a result".to_string())
            })?;
            // The engine emits the final part events right before completing
            // the receiver; render them before reporting done.
            drain(&mut st, app, args, json, tty, session_id, user_mid, events)?;
            return Ok(st.printed_answer);
        }
        tokio::select! {
            biased;
            response = &mut done => completion = Some(response),
            event = events.recv() => {
                match event {
                    Ok(event) => on_event(&mut st, app, args, json, tty, session_id, user_mid, event),
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(_) => return Err("Event bus closed".to_string()),
                }
            }
            line = lines.next_line(), if st.asking => {
                match line.map_err(|error| error.to_string())? {
                    Some(line) => {
                        match st.pending.take() {
                            Some(Pending::Permission(id)) => {
                                let reply = match line.trim() {
                                    "y" | "yes" => "once",
                                    "a" | "always" => "always",
                                    _ => "reject",
                                };
                                app.perms.reply(&id, reply);
                            }
                            Some(Pending::Question(id, count)) => {
                                let picks: Vec<Vec<String>> = std::iter::repeat_n(
                                    line.trim().split(',').map(str::to_string).collect(),
                                    count,
                                )
                                .collect();
                                app.questions.reply(&id, picks);
                            }
                            None => {}
                        }
                        st.asking = st.pending.is_some();
                    }
                    None => return Err("Prompt cancelled".to_string()),
                }
            }
        }
    }
}

fn file_part(path: &str) -> Result<Value, String> {
    let bytes = std::fs::read(path).map_err(|error| format!("{path}: {error}"))?;
    let mime = mime_guess(path);
    let name = std::path::Path::new(path)
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| path.to_string());
    Ok(json!({
        "type": "file",
        "url": format!("data:{mime};base64,{}", base64::engine::general_purpose::STANDARD.encode(bytes)),
        "mime": mime,
        "filename": name,
    }))
}

fn mime_guess(path: &str) -> &'static str {
    match path
        .rsplit_once('.')
        .map(|(_, extension)| extension.to_ascii_lowercase())
        .as_deref()
    {
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("pdf") => "application/pdf",
        Some("txt" | "md" | "markdown") => "text/plain",
        _ => "application/octet-stream",
    }
}
