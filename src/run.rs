use crate::events as event;
// One-shot prompt: `ocmini run [MESSAGE..]`, modeled on `opencode run`.
// The turn runs on the same in-process engine as the TUI; events from the
// bus are rendered as text (agent header, tool lines, streamed answer) or
// as JSONL (`--format json`).

use crate::client::{parse_answer, TextKind, TextState};
use crate::engine::{bus, App, EngineApi};
use crate::engine_impl::Engine;
use crate::sessions;
use crate::tui;
use crate::types::{self, BusEvent, PromptRequest, TurnResult};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::io::{IsTerminal, Read, Write};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::{broadcast, oneshot};

enum Pending {
    Permission(String),
    Question {
        id: String,
        questions: Vec<Value>,
        answers: Vec<Vec<String>>,
    },
}
pub async fn run(app: App, args: crate::RunArgs) -> Result<(), String> {
    let existing = if args.session.is_some() || args.continue_last {
        Some(
            sessions::open_for_app(
                &app,
                args.dir.as_deref(),
                args.session.as_deref(),
                args.continue_last,
            )
            .await?,
        )
    } else {
        None
    };
    let directory = existing
        .as_ref()
        .map(|s| Ok(s.directory.clone()))
        .unwrap_or_else(|| sessions::directory(args.dir.as_deref().unwrap_or(".")))?;
    let app = app.for_directory(&directory)?;
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
        parts.push(crate::attachments::from_file(std::path::Path::new(path))?.part());
    }
    if parts.is_empty() && args.command.is_none() {
        return Err("No prompt: pass a message, pipe stdin, or attach a file".into());
    }

    let mut user_mid = types::hex_id("msg_");
    let mut request = PromptRequest {
        message_id: Some(user_mid.clone()),
        model: args.model.clone(),
        parts,
        agent: args.agent.clone(),
        variant: args.variant.clone(),
        ..Default::default()
    };
    if let Some(command) = &args.command {
        let argv = if text == args.message.join(" ")
            && !(args.message.len() == 1 && text.contains(['\'', '"']))
        {
            Some(args.message.as_slice())
        } else {
            None
        };
        request = crate::commands::prepare(&app.config, command, &text, request, argv)?;
    }
    let mut session = match existing {
        Some(session) => session,
        None => {
            let mut session =
                types::Session::new(args.title.as_deref().unwrap_or("run"), &directory);
            if args.title.is_some() {
                session.title_source = Some(types::TitleSource::Explicit);
            }
            sessions::create(&app, session, None).await?
        }
    };
    if args.fork {
        session = sessions::fork(&app, &session, None).await?;
    }
    let (app, _) = crate::client::activate(&app, &session, &mut request)?;
    let json = args.format == "json";
    let tty = std::io::stdout().is_terminal();
    if !json {
        let agent = request.agent.as_deref().unwrap_or("build");
        let model = tui::display_model(request.model.as_ref(), &app.config.model);
        println!("> {agent} · {model}");
    }
    let selection = (
        request.model.clone(),
        request.agent.clone(),
        request.variant.clone(),
    );
    let engine = Engine::new();
    let mut events = bus().subscribe();
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    loop {
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
        if !args.interactive {
            return Ok(());
        }
        // Follow-up prompts reuse the same turn and cleanup path.
        let line = loop {
            let Some(line) = lines.next_line().await.map_err(|e| e.to_string())? else {
                return Ok(());
            };
            if line.trim() == "/exit" {
                return Ok(());
            }
            if !line.trim().is_empty() {
                break line;
            }
        };
        user_mid = types::hex_id("msg_");
        request = PromptRequest {
            message_id: Some(user_mid.clone()),
            parts: vec![json!({"type":"text","text":line})],
            model: selection.0.clone(),
            agent: selection.1.clone(),
            variant: selection.2.clone(),
            ..Default::default()
        };
    }
}

/// Streams one turn: renders bus events, answers permission prompts, and
/// resolves when the turn's completion receiver fires. The bus is drained
/// eagerly each iteration (a finished turn can queue events faster than any
/// poll cadence), so rendering never lags the completion signal.
#[derive(Default)]
struct State {
    interactions: crate::client::Interactions,
    text: TextState,
    headered: HashSet<String>,
    printed_answer: bool,
    turn_error: Option<String>,
    pending: Option<Pending>,
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
    if !crate::client::accepts(&app.store, session_id, &event) {
        return;
    }
    if st.interactions.apply(&event) {
        st.pending = None;
        if let Some(request) = st.interactions.current().cloned() {
            show_interaction(st, app, args, tty, &request);
        }
    }
    // JSON reads accepted final parts from storage; it needs no live text buffer.
    let update = if json { None } else { st.text.apply(&event) };
    if let Some(update) = update {
        if update.kind == TextKind::Text || args.thinking {
            if update.reset {
                if update.text.is_empty() {
                    println!("\n[partial response discarded; retrying]");
                } else {
                    println!("\n[response updated]");
                }
            }
            print!("{}", update.text);
            st.printed_answer |= update.kind == TextKind::Text && !update.text.is_empty();
            let _ = std::io::stdout().flush();
        }
    }
    match event.kind {
        event::EventKind::Part(event::PartKind::Updated) => {
            let part = &event.properties["part"];
            if part["messageID"].as_str() == Some(user_mid) {
                return;
            }
            let id = part["id"].as_str().unwrap_or_default().to_string();
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
                _ => {}
            }
        }
        event::EventKind::Message(event::MessageKind::Updated)
            if json
                && event.properties["info"]["role"] == "assistant"
                && event.properties["info"]["time"]["completed"].is_number()
                && event.properties["info"]["error"].is_null() =>
        {
            let id = event.properties["info"]["id"].as_str().unwrap_or_default();
            if st.headered.insert(format!("{id}:complete")) {
                if let Ok(Some(message)) = app.store.message(session_id, id) {
                    for part in message.parts {
                        let kind = part["type"].as_str().unwrap_or_default();
                        if (kind == "text" || kind == "reasoning" && args.thinking)
                            && part["text"].as_str().is_some_and(|text| !text.is_empty())
                        {
                            println!("{}", json!({"type":kind,"text":part["text"]}));
                        }
                    }
                }
            }
        }
        event::EventKind::Session(event::SessionKind::Error) => {
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

fn show_interaction(st: &mut State, app: &App, args: &crate::RunArgs, tty: bool, event: &BusEvent) {
    match event.kind {
        event::EventKind::Permission(event::PermissionKind::Asked) => {
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
                st.pending = Some(Pending::Permission(id.to_string()));
            } else {
                // Non-interactive: refuse rather than hang.
                app.perms.reply(id, "reject");
            }
        }
        event::EventKind::Question(event::QuestionKind::Asked) => {
            if tty {
                let questions = event.properties["questions"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default();
                if let Some(question) = questions.first() {
                    show_question(question);
                }
                st.pending = Some(Pending::Question {
                    id: event.properties["id"]
                        .as_str()
                        .unwrap_or_default()
                        .to_owned(),
                    questions,
                    answers: Vec::new(),
                });
            } else {
                app.questions
                    .reject(event.properties["id"].as_str().unwrap_or_default());
            }
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
            Err(broadcast::error::TryRecvError::Lagged(_)) => {
                return Err("Event stream lagged; output may be incomplete".into())
            }
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
    done: oneshot::Receiver<TurnResult>,
    events: &mut broadcast::Receiver<Arc<BusEvent>>,
    lines: &mut tokio::io::Lines<BufReader<tokio::io::Stdin>>,
) -> Result<bool, String> {
    let mut st = State::default();
    let shutdown = crate::client::shutdown_signal().map_err(|e| e.to_string())?;
    tokio::pin!(shutdown);
    let mut done = Box::pin(done);
    loop {
        drain(&mut st, app, args, json, tty, session_id, user_mid, events)?;
        tokio::select! {
            biased;
            _ = &mut shutdown => return Err("Aborted".into()),
            response = &mut done => {
                drain(&mut st, app, args, json, tty, session_id, user_mid, events)?;
                let value = response.map_err(|_| {
                    st.turn_error
                        .take()
                        .unwrap_or_else(|| "Turn ended without a result".to_string())
                })?;
                value.map_err(String::from)?;
                if let Some(error) = st.turn_error.take() {
                    return Err(error);
                }
                return Ok(st.printed_answer);
            }
            event = events.recv() => {
                match event {
                    Ok(event) => on_event(&mut st, app, args, json, tty, session_id, user_mid, event),
                    Err(broadcast::error::RecvError::Lagged(_)) => return Err("Event stream lagged; output may be incomplete".into()),
                    Err(_) => return Err("Event bus closed".to_string()),
                }
            }
            line = lines.next_line(), if st.pending.is_some() => {
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
                            Some(Pending::Question { id, questions, mut answers }) => {
                                if let Some(question) = questions.get(answers.len()) {
                                    let labels: Vec<String> = question["options"].as_array().map(|options| options.iter()
                                        .filter_map(|o| o["label"].as_str().map(str::to_owned)).collect()).unwrap_or_default();
                                    let answer = parse_answer(&line, &labels);
                                    if crate::types::validate_answer(question, &answer).is_err() {
                                        println!("Choose one answer.");
                                    } else { answers.push(answer); }
                                }
                                if answers.len() == questions.len() && app.questions.reply(&id, answers.clone()) {
                                } else {
                                    if let Some(question) = questions.get(answers.len()) { show_question(question); }
                                    st.pending = Some(Pending::Question { id, questions, answers });
                                }
                            }
                            None => {}
                        }
                    }
                    None => return Err("Prompt cancelled".to_string()),
                }
            }
        }
    }
}

fn show_question(question: &Value) {
    let labels = question["options"]
        .as_array()
        .map(|options| {
            options
                .iter()
                .enumerate()
                .map(|(i, o)| format!("{}: {}", i + 1, o["label"].as_str().unwrap_or_default()))
                .collect::<Vec<_>>()
                .join(" | ")
        })
        .unwrap_or_default();
    println!(
        "? {} ({labels})",
        question["question"].as_str().unwrap_or_default()
    );
    let _ = std::io::stdout().flush();
}
