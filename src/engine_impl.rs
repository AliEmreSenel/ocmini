use crate::engine::{bus, App, EngineApi};
use crate::provider::{self, ChatRequest};
use crate::tools;
use crate::types::*;
use futures::StreamExt;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use tokio::sync::oneshot;

static ENGINE: OnceLock<Arc<dyn EngineApi>> = OnceLock::new();
static CANCELS: OnceLock<Mutex<HashMap<String, Arc<AtomicBool>>>> = OnceLock::new();

fn cancels() -> &'static Mutex<HashMap<String, Arc<AtomicBool>>> {
    CANCELS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub struct Engine;

impl Engine {
    pub fn new() -> Self {
        Self
    }
}

impl Default for Engine {
    fn default() -> Self {
        Self::new()
    }
}

fn system_prompt(app: &App, dir: &str) -> String {
    let mut s = String::from(
        "You are ocmini, an interactive coding agent. You help users with software engineering tasks. \
         Use tools to read, search and edit code. Be concise. Prefer existing codebase conventions.",
    );
    let cfg = app.config.lock().unwrap();
    let mut files: Vec<String> = cfg.instructions.clone();
    if !files.iter().any(|f| f == "AGENTS.md") {
        files.push("AGENTS.md".into());
    }
    for f in files {
        for base in [dir.to_string(), std::env::var("HOME").unwrap_or_default()] {
            if let Ok(txt) = std::fs::read_to_string(format!("{base}/{f}")) {
                s.push_str(&format!("\n\n# Instructions from {base}/{f}\n{txt}"));
                break;
            }
        }
    }
    s
}

fn build_messages(app: &App, session_id: &str) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    for m in app.store.messages(session_id) {
        let role = m.info.role.as_str();
        if role == "user" {
            let text = m
                .parts
                .iter()
                .filter(|p| p["type"] == "text")
                .map(|p| p["text"].as_str().unwrap_or_default())
                .collect::<Vec<_>>()
                .join("\n");
            out.push(json!({"role": "user", "content": text}));
        } else if role == "assistant" {
            let text = m
                .parts
                .iter()
                .filter(|p| p["type"] == "text")
                .map(|p| p["text"].as_str().unwrap_or_default())
                .collect::<Vec<_>>()
                .join("");
            let mut msg = json!({"role": "assistant", "content": text});
            let calls: Vec<Value> = m
                .parts
                .iter()
                .filter(|p| p["type"] == "tool")
                .map(|p| {
                    json!({"id": p["callID"], "type": "function", "function": {
                        "name": p["tool"],
                        "arguments": serde_json::to_string(&p["state"]["input"]).unwrap_or_default()
                    }})
                })
                .collect();
            if !calls.is_empty() {
                msg["tool_calls"] = json!(calls);
                msg["content"] = json!(if text.is_empty() { Value::Null } else { Value::String(text) });
            }
            out.push(msg);
            for p in &m.parts {
                if p["type"] == "tool" {
                    out.push(json!({
                        "role": "tool",
                        "tool_call_id": p["callID"],
                        "content": p["state"]["output"].as_str().unwrap_or("no output")
                    }));
                }
            }
        }
    }
    out
}

fn part_text(id: &str, sid: &str, mid: &str, text: &str) -> Value {
    json!({"id": id, "sessionID": sid, "messageID": mid, "type": "text", "text": text})
}

fn part_reasoning(id: &str, sid: &str, mid: &str, text: &str) -> Value {
    json!({"id": id, "sessionID": sid, "messageID": mid, "type": "reasoning", "text": text})
}

async fn run_turn(
    app: App,
    engine: Arc<dyn EngineApi>,
    session_id: String,
    model_id: String,
    provider_id: String,
    user_text: String,
    agent: Option<String>,
    variant: Option<String>,
) -> Result<Value, String> {
    let is_plan = agent.as_deref() == Some("plan");
    let session = app.store.get_session(&session_id).ok_or("no session")?;
    app.store.set_busy(&session_id, true);
    bus().emit("session.status", json!({"sessionID": session_id, "status": {"type": "busy"}}));

    let now = now_ms();
    let uid = hex_id("msg_");
    let u_part = hex_id("prt_");
    let user_msg = MessageWithParts {
        info: MessageInfo {
            id: uid.clone(),
            session_id: session_id.clone(),
            role: "user".into(),
            time: MessageTime { created: now, completed: Some(now) },
            model_id: Some(model_id.clone()),
            provider_id: Some(provider_id.clone()),
            cost: None,
            tokens: None,
            finish: None,
        },
        parts: vec![part_text(&u_part, &session_id, &uid, &user_text)],
    };
    app.store.add_message(&user_msg);
    bus().emit("message.updated", json!({"sessionID": session_id, "info": user_msg.info}));
    bus().emit("message.part.updated", json!({"sessionID": session_id, "part": user_msg.parts[0], "time": {"created": now_ms()}}));

    let cancel = Arc::new(AtomicBool::new(false));
    cancels().lock().unwrap().insert(session_id.clone(), cancel.clone());

    let mut system = system_prompt(&app, &session.directory);
    if is_plan {
        system.push_str(
            "\n\nPLAN MODE (READ-ONLY): You must NOT modify the codebase. Analyze the code, \
             read files, search, and produce a detailed implementation plan with file paths, \
             function names and ordered steps. Do not run shell commands that mutate state \
             (no installs, writes, git mutations). Only present the plan; the user will \
             switch to build mode to execute it.",
        );
    }
    let adapter = {
        let cfg = app.config.lock().unwrap();
        provider::build(&provider_id, cfg.provider(&provider_id), &app.auth)
    };
    const PLAN_BLOCKED: [&str; 3] = ["write", "edit", "task"];
    let mut tool_defs: Vec<Value> = tools::tool_defs()
        .into_iter()
        .filter(|d| !(is_plan && PLAN_BLOCKED.contains(&d["function"]["name"].as_str().unwrap_or_default())))
        .collect();
    tool_defs.extend(app.mcp.tool_defs());

    for _iter in 0..100 {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let messages = build_messages(&app, &session_id);
        let req = ChatRequest {
            model: model_id.clone(),
            system: system.clone(),
            messages,
            tools: tool_defs.clone(),
            variant: variant.clone(),
        };
        let mut stream = adapter.stream(req).await.map_err(|e| {
            bus().emit("session.error", json!({"sessionID": session_id, "error": {"name": "ProviderAuthError", "data": {"message": e.clone()}}}));
            app.store.set_busy(&session_id, false);
            bus().emit("session.idle", json!({"sessionID": session_id}));
            e
        })?;
        let mut stream: std::pin::Pin<Box<dyn futures::Stream<Item = ProviderEvent> + Send>> =
            Box::pin(stream);

        let created = now_ms();
        let amid = hex_id("msg_");
        let text_part = hex_id("prt_");
        let mut text_acc = String::new();
        let reason_part = hex_id("prt_");
        let mut reason_acc = String::new();
        let mut tool_parts: Vec<Value> = Vec::new();
        let mut finished: Option<(String, Tokens)> = None;
        let mut info_emitted = false;
        let mut ensure_info = || {
            if !info_emitted {
                info_emitted = true;
                bus().emit(
                    "message.updated",
                    json!({"sessionID": session_id, "info": assistant_info(&amid, &session_id, &model_id, &provider_id, created)}),
                );
            }
        };

        while let Some(ev) = stream.as_mut().next().await {
            match ev {
                ProviderEvent::TextDelta(d) => {
                    ensure_info();
                    text_acc.push_str(&d);
                    bus().emit("message.part.delta", json!({"partID": text_part, "messageID": amid, "sessionID": session_id, "field": "text", "delta": d}));
                }
                ProviderEvent::ReasoningDelta(d) => {
                    ensure_info();
                    reason_acc.push_str(&d);
                    bus().emit("message.part.updated", json!({"sessionID": session_id, "part": part_reasoning(&reason_part, &session_id, &amid, &reason_acc), "time": {"created": now_ms()}}));
                }
                ProviderEvent::ToolCallStart { call_id, name, args } => {
                    ensure_info();
                    let pid = hex_id("prt_");
                    let input: Value = serde_json::from_str(&args).unwrap_or(json!({}));
                    let part = json!({
                        "id": pid, "sessionID": session_id, "messageID": amid, "type": "tool",
                        "tool": name, "callID": call_id,
                        "state": {"status": "running", "input": input, "time": {"start": now_ms()}}
                    });
                    app.store.update_part(&session_id, &part);
                    bus().emit("message.part.updated", json!({"sessionID": session_id, "part": part, "time": {"created": now_ms()}}));
                    tool_parts.push(part);
                }
                ProviderEvent::Finished { finish, usage } => {
                    finished = Some((finish, usage));
                }
                ProviderEvent::Error(e) => {
                    bus().emit("session.error", json!({"sessionID": session_id, "error": {"name": "UnknownError", "data": {"message": e}}}));
                }
            }
            if cancel.load(Ordering::Relaxed) {
                break;
            }
        }

        if text_acc.is_empty() && reason_acc.is_empty() && tool_parts.is_empty() {
            break;
        }

        let mut parts: Vec<Value> = Vec::new();
        if !reason_acc.is_empty() {
            let p = part_reasoning(&reason_part, &session_id, &amid, &reason_acc);
            app.store.update_part(&session_id, &p);
            bus().emit("message.part.updated", json!({"sessionID": session_id, "part": p.clone(), "time": {"created": now_ms()}}));
            parts.push(p);
        }
        if !text_acc.is_empty() {
            let p = part_text(&text_part, &session_id, &amid, &text_acc);
            app.store.update_part(&session_id, &p);
            bus().emit("message.part.updated", json!({"sessionID": session_id, "part": p.clone(), "time": {"created": now_ms()}}));
            parts.push(p);
        }
        parts.extend(tool_parts.iter().cloned());
        let (finish, usage) = finished.unwrap_or(("stop".into(), Tokens::default()));
        let mut info = assistant_info(&amid, &session_id, &model_id, &provider_id, created);
        info.time.completed = Some(now_ms());
        info.finish = Some(finish);
        info.tokens = Some(usage.clone());
        app.store.add_message(&MessageWithParts { info: info.clone(), parts });
        app.usage.record(&amid, &session_id, &provider_id, &model_id, &usage, created);
        bus().emit("message.updated", json!({"sessionID": session_id, "info": info}));

        if tool_parts.is_empty() {
            break;
        }
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let ctx = tools::ToolCtx {
            dir: session.directory.clone(),
            app: app.clone(),
            session_id: session_id.clone(),
            engine: engine.clone(),
            cancel: cancel.clone(),
        };
        for part in &tool_parts {
            let name = part["tool"].as_str().unwrap_or_default().to_string();
            let input = part["state"]["input"].clone();
            let start = part["state"]["time"]["start"].as_i64().unwrap_or_else(now_ms);
            let (perm_name, pattern) = perm_for(&name, &input, &session.directory);
            let action = {
                let mut rules = crate::permission::default_rules(&app.config.lock().unwrap().permission);
                if let Some(sp) = &session.permission {
                    rules.extend(sp.iter().cloned());
                }
                if let Some(a) = app.always.lock().unwrap().get(&session_id) {
                    rules.extend(a.iter().cloned());
                }
                crate::permission::decide(&rules, &perm_name, &pattern)
            };
            let out = if is_plan && PLAN_BLOCKED.contains(&name.as_str()) {
                Err(ToolError { message: format!("Tool '{name}' is not available in plan mode"), denied: true })
            } else if action == "deny" {
                Err(ToolError { message: format!("Permission '{perm_name}' denied by rules"), denied: true })
            } else if action == "ask" {
                let (req, rx) = app.perms.ask(
                    &session_id,
                    &perm_name,
                    vec![pattern.clone()],
                    json!({"tool": name, "input": input, "patterns": [pattern.clone()]}),
                    vec![pattern.clone()],
                );
                let mut asked = serde_json::to_value(&req).unwrap();
                asked["tool"] = json!(name);
                bus().emit("permission.asked", asked);
                let reply = tokio::select! {
                    r = rx => r.unwrap_or_else(|_| "reject".into()),
                    _ = poll_cancel(cancel.clone()) => "reject".into(),
                };
                if reply == "reject" {
                    Err(ToolError { message: format!("User rejected permission '{perm_name}' ({pattern})"), denied: true })
                } else {
                    if reply == "always" {
                        app.always.lock().unwrap().entry(session_id.clone()).or_default().push(
                            PermissionRule { permission: perm_name.clone(), pattern: pattern.clone(), action: "allow".into() },
                        );
                    }
                    tools::run_tool(&ctx, &name, &input).await
                }
            } else {
                tools::run_tool(&ctx, &name, &input).await
            };
            let end = now_ms();
            let mut updated = part.clone();
            match out {
                Ok(o) => {
                    updated["state"] = json!({"status": "completed", "input": input, "output": o.output, "title": o.title, "time": {"start": start, "end": end}});
                }
                Err(e) => {
                    updated["state"] = json!({"status": "error", "input": input, "error": e.message, "time": {"start": start, "end": end}});
                }
            }
            app.store.update_part(&session_id, &updated);
            bus().emit("message.part.updated", json!({"sessionID": session_id, "part": updated, "time": {"created": now_ms()}}));
        }
    }

    if cancel.load(Ordering::Relaxed) {
        bus().emit("session.error", json!({"sessionID": session_id, "error": {"name": "MessageAbortedError", "data": {"message": "Aborted"}}}));
    }
    app.store.set_busy(&session_id, false);
    cancels().lock().unwrap().remove(&session_id);
    bus().emit("session.status", json!({"sessionID": session_id, "status": {"type": "idle"}}));
    bus().emit("session.idle", json!({"sessionID": session_id}));
    let msgs = app.store.messages(&session_id);
    Ok(json!({"info": msgs.last().map(|m| m.info.clone()), "parts": msgs.last().map(|m| m.parts.clone()).unwrap_or_default()}))
}


fn perm_for(name: &str, input: &Value, dir: &str) -> (String, String) {
    let abs = |p: &str| -> String {
        if p.is_empty() {
            return "*".into();
        }
        if p.starts_with('/') {
            p.to_string()
        } else {
            format!("{}/{}", dir.trim_end_matches('/'), p.trim_start_matches("./"))
        }
    };
    match name {
        "bash" => ("bash".into(), input["command"].as_str().unwrap_or_default().to_string()),
        "write" | "edit" | "read" => (name.into(), abs(input["filePath"].as_str().unwrap_or_default())),
        "glob" => ("glob".into(), abs(input["path"].as_str().unwrap_or_default())),
        "grep" => ("grep".into(), input["pattern"].as_str().unwrap_or_default().to_string()),
        "webfetch" => ("webfetch".into(), input["url"].as_str().unwrap_or_default().to_string()),
        "task" => ("task".into(), input["subagent_type"].as_str().unwrap_or_default().to_string()),
        "todowrite" => ("todowrite".into(), "*".into()),
        "question" => ("question".into(), "*".into()),
        _ if name.starts_with("mcp_") => ("mcp".into(), name.to_string()),
        _ => (name.into(), "*".into()),
    }
}

async fn poll_cancel(c: std::sync::Arc<std::sync::atomic::AtomicBool>) {
    loop {
        if c.load(std::sync::atomic::Ordering::Relaxed) {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
}

fn assistant_info(mid: &str, sid: &str, model: &str, provider: &str, created: i64) -> MessageInfo {
    MessageInfo {
        id: mid.to_string(),
        session_id: sid.to_string(),
        role: "assistant".into(),
        time: MessageTime { created, completed: None },
        model_id: Some(model.to_string()),
        provider_id: Some(provider.to_string()),
        cost: Some(0.0),
        tokens: None,
        finish: None,
    }
}

impl EngineApi for Engine {
    fn start_turn(
        &self,
        app: App,
        session_id: String,
        model_id: String,
        provider_id: String,
        text: String,
        agent: Option<String>,
        variant: Option<String>,
    ) -> oneshot::Receiver<Value> {
        let (tx, rx) = oneshot::channel();
        let engine = ENGINE.get().cloned();
        tokio::spawn(async move {
            let engine = engine.unwrap_or_else(|| Arc::new(Engine::new()));
            let res = run_turn(app, engine, session_id, model_id, provider_id, text, agent, variant).await;
            let _ = tx.send(res.unwrap_or(json!({"error": "turn failed"})));
        });
        rx
    }

    fn abort(&self, session_id: &str) {
        if let Some(c) = cancels().lock().unwrap().get(session_id) {
            c.store(true, Ordering::Relaxed);
        }
    }

    fn run_sync(
        &self,
        app: App,
        session_id: String,
        model_id: String,
        provider_id: String,
        text: String,
        agent: Option<String>,
        variant: Option<String>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Value, String>> + Send>> {
        let engine = ENGINE.get().cloned();
        Box::pin(async move {
            let engine = engine.unwrap_or_else(|| Arc::new(Engine::new()));
            run_turn(app, engine, session_id, model_id, provider_id, text, agent, variant).await
        })
    }

    fn summarize(&self, app: App, session_id: String) -> Result<(), String> {
        if app.store.get_session(&session_id).is_none() {
            return Err("no session".into());
        }
        let mid = hex_id("msg_");
        let pid = hex_id("prt_");
        app.store.add_message(&MessageWithParts {
            info: MessageInfo {
                id: mid.clone(),
                session_id: session_id.clone(),
                role: "assistant".into(),
                time: MessageTime { created: now_ms(), completed: Some(now_ms()) },
                model_id: None,
                provider_id: None,
                cost: None,
                tokens: None,
                finish: None,
            },
            parts: vec![json!({"id": pid, "sessionID": session_id, "messageID": mid, "type": "compaction"})],
        });
        bus().emit("session.compacted", json!({"sessionID": session_id}));
        Ok(())
    }
}
