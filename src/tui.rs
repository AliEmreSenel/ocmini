use crate::engine::{bus, App, EngineApi};
use crate::engine_impl::Engine;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use serde_json::Value;
use std::io::{stdout, Write};
use std::sync::Arc;
use tokio::sync::mpsc;

enum In {
    Char(char),
    Enter,
    Backspace,
    CtrlC,
}

fn prompt() {
    out("\x1b[36m> \x1b[0m");
}

fn out(s: &str) {
    let mut o = stdout();
    // raw mode disables ONLCR: bare \n moves the cursor down without CR,
    // which cascades all output diagonally. Always emit CRLF.
    let _ = o.write_all(s.replace('\n', "\r\n").as_bytes());
    let _ = o.flush();
}

fn print_tool(part: &Value) {
    let tool = part["tool"].as_str().unwrap_or("?");
    let st = part["state"]["status"].as_str().unwrap_or("");
    let (icon, col) = match st {
        "running" => ("⚙", "\x1b[33m"),
        "completed" => ("✔", "\x1b[32m"),
        "error" => ("✖", "\x1b[31m"),
        _ => ("•", "\x1b[2m"),
    };
    let mut detail = String::new();
    for key in ["command", "filePath", "url", "pattern", "path", "subagent_type"] {
        if let Some(v) = part["state"]["input"][key].as_str() {
            detail = v.chars().take(90).collect();
            break;
        }
    }
    out(&format!("{col}{icon} {tool}\x1b[0m \x1b[2m{detail}\x1b[0m\n"));
}

pub fn run() {
    setup();
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let app = App::new();
        let engine: Arc<dyn EngineApi> = Arc::new(Engine::new());

        let cwd = std::env::current_dir().map(|p| p.display().to_string()).unwrap_or_else(|_| ".".into());
        let mut session = app.store.create_session_owned(&format!("tui"), &cwd);
        let default_model = {
            let cfg = app.config.lock().unwrap();
            cfg.model.clone()
        };
        let (mut provider, mut model) = match default_model.split_once('/') {
            Some((p, m)) => (p.to_string(), m.to_string()),
            None => ("pllm5".to_string(), default_model.clone()),
        };

        let (tx, mut rx) = mpsc::unbounded_channel::<In>();
        std::thread::spawn(move || loop {
            if event::poll(std::time::Duration::from_millis(50)).unwrap_or(false) {
                if let Ok(Event::Key(KeyEvent { code, kind, modifiers, .. })) = event::read() {
                    if kind == KeyEventKind::Press {
                        let i = match code {
                            KeyCode::Enter => In::Enter,
                            KeyCode::Backspace => In::Backspace,
                            KeyCode::Char('c') if modifiers.contains(crossterm::event::KeyModifiers::CONTROL) => In::CtrlC,
                            KeyCode::Char(c) => In::Char(c),
                            _ => continue,
                        };
                        if tx.send(i).is_err() {
                            break;
                        }
                    }
                }
            }
        });

        let mut rx_bus = bus().subscribe();
        let mut shown_reason: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        let mut pending_perm: Option<String> = None;
        let mut input = String::new();
        let mut agent: Option<String> = None;
        let mut busy: Option<tokio::task::JoinHandle<()>> = None;

        let perm_mode = {
            let cfg = app.config.lock().unwrap();
            let rules = crate::permission::default_rules(&cfg.permission);
            let strict = rules.iter().any(|r| r.action == "ask" || r.action == "deny");
            if strict { "supervised" } else { "full-access" }
        };
        out(&format!(
            "\x1b[1mocmini\x1b[0m 1.19.2 · {provider}/{model} · {cwd} · \x1b[33m{perm_mode}\x1b[0m\n\x1b[2m/quit /model <prov/id> /new /abort · Enter sends\x1b[0m\n"
        ));
        out("\x1b[36m> \x1b[0m");

        loop {
            while let Ok(ev) = rx_bus.try_recv() {
                let p = &ev.properties;
                // sessionID is top-level for most events, nested in .part for
                // message.part.updated (tool/reasoning parts) — check both.
                let ev_sid = p["sessionID"]
                    .as_str()
                    .or_else(|| p["part"]["sessionID"].as_str())
                    .unwrap_or_default();
                if ev_sid != session.id {
                    continue;
                }
                match ev.r#type.as_str() {
                    "message.part.delta" => {
                        if p["field"] == "text" {
                            if let Some(d) = p["delta"].as_str() {
                                out(d);
                            }
                        }
                    }
                    "message.part.updated" => {
                        let part = &p["part"];
                        if part["type"] == "tool" {
                            print_tool(part);
                        } else if part["type"] == "reasoning" {
                            let id = part["id"].as_str().unwrap_or_default().to_string();
                            let txt = part["text"].as_str().unwrap_or_default();
                            let seen = shown_reason.get(&id).copied().unwrap_or(0);
                            if txt.len() > seen {
                                out(&format!("\x1b[2m{}\x1b[0m", &txt[seen..]));
                                *shown_reason.entry(id).or_insert(0) = txt.len();
                            }
                        }
                    }
                    "permission.asked" => {
                        let id = p["id"].as_str().unwrap_or_default().to_string();
                        let perm = p["permission"].as_str().unwrap_or("?");
                        let pat = p["patterns"][0].as_str().unwrap_or("");
                        out(&format!("\n\x1b[33m⚠ {perm}: {}\x1b[0m\n\x1b[2m[y] once  [a] always  [n] reject\x1b[0m\n> ", pat.chars().take(100).collect::<String>()));
                        pending_perm = Some(id);
                    }
                    "permission.replied" => {
                        pending_perm = None;
                    }
                    "session.error" => {
                        out(&format!("\x1b[31merror: {}\x1b[0m\n", p["error"]["data"]["message"]));
                    }
                    "session.idle" => {
                        out("\n");
                    }
                    _ => {}
                }
            }

            let ev = match rx.try_recv() {
                Ok(e) => Some(e),
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                    continue;
                }
                Err(_) => None,
            };
            let Some(ev) = ev else { break };

            match ev {
                In::CtrlC => {
                    if busy.is_some() {
                        engine.abort(&session.id);
                        out("\x1b[2m(aborting…)\x1b[0m");
                        prompt();
                    }
                }
                In::Backspace => {
                    if busy.is_none() && input.pop().is_some() {
                        out("\u{8} \u{8}");
                    }
                }
                In::Char(c) => {
                    if let Some(id) = pending_perm.take() {
                        let reply = match c {
                            'y' => "once",
                            'a' => "always",
                            'n' => "reject",
                            _ => "reject",
                        };
                        app.perms.reply(&id, reply);
                        out(&format!("\x1b[2m{reply}\x1b[0m\n"));
                    } else if busy.is_none() {
                        input.push(c);
                        out(&c.to_string());
                    }
                }
                In::Enter => {
                    if busy.is_some() {
                        continue;
                    }
                    let text = std::mem::take(&mut input);
                    out("\n");
                    let text = text.trim().to_string();
                    if text.is_empty() {
                        prompt();
                        continue;
                    }
                    if text == "/quit" || text == "/exit" {
                        break;
                    }
                    if text == "/abort" {
                        engine.abort(&session.id);
                        prompt();
                        continue;
                    }
                    if text == "/plan" {
                        agent = Some("plan".into());
                        out("\x1b[2mplan mode: read-only\x1b[0m\n");
                        prompt();
                        continue;
                    }
                    if text == "/build" {
                        agent = None;
                        out("\x1b[2mbuild mode\x1b[0m\n");
                        prompt();
                        continue;
                    }
                    if text == "/new" {
                        session = app.store.create_session_owned("tui", &cwd);
                        out("\x1b[2mnew session\x1b[0m\n");
                        prompt();
                        continue;
                    }
                    if let Some(rest) = text.strip_prefix("/model ") {
                        match rest.trim().split_once('/') {
                            Some((p, m)) => {
                                provider = p.to_string();
                                model = m.to_string();
                                out(&format!("\x1b[2mmodel: {provider}/{model}\x1b[0m\n"));
                                prompt();
                            }
                            None => {
                                out("\x1b[2musage: /model provider/modelID\x1b[0m\n");
                                prompt()
                            },
                        }
                        continue;
                    }
                    if let Some(rest) = text.strip_prefix('/').map(str::to_string) {
                        out(&format!("\x1b[2munknown: /{rest}\x1b[0m\n"));
                        prompt();
                        continue;
                    }

                    let app2 = app.clone();
                    let engine2 = engine.clone();
                    let sid = session.id.clone();
                    let (p2, m2) = (provider.clone(), model.clone());
                    let agent = agent.clone();
                    let h = tokio::spawn(async move {
                        if let Err(e) = engine2
                            .run_sync(app2, sid, m2, p2, text, agent.clone())
                            .await
                        {
                            out(&format!("\x1b[31m{e}\x1b[0m\n"));
                        }
                    });
                    busy = Some(h);
                }
            }

            if let Some(h) = &busy {
                if h.is_finished() {
                    busy = None;
                    out("\x1b[36m> \x1b[0m");
                }
            }
        }
        cleanup();
    });
}

pub fn setup() {
    enable_raw_mode().ok();
}

pub fn cleanup() {
    disable_raw_mode().ok();
    out("\x1b[0m\n");
}
