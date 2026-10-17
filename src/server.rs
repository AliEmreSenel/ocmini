use crate::engine::{bus, App, EngineApi};
use crate::types::*;
use async_stream::stream;
use axum::extract::{Path, Request, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, patch, post};
use axum::{Json, Router};
use serde_json::{json, Value};
use std::sync::Arc;

#[derive(Clone)]
pub struct AppState {
    pub app: App,
    pub engine: Arc<dyn EngineApi>,
}

async fn auth_gate(req: Request, next: Next) -> Response {
    if let Ok(pw) = std::env::var("OPENCODE_SERVER_PASSWORD") {
        if !pw.is_empty() {
            use base64::Engine;
            let want = format!(
                "Basic {}",
                base64::engine::general_purpose::STANDARD.encode(format!("opencode:{pw}"))
            );
            let given = req
                .headers()
                .get(header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");
            if given != want {
                return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
            }
        }
    }
    next.run(req).await
}

async fn health() -> Json<Value> {
    Json(json!({"healthy": true, "version": "1.19.2"}))
}

async fn api_info() -> StatusCode {
    StatusCode::NOT_FOUND
}

fn dir_of(headers: &HeaderMap) -> String {
    headers
        .get("x-opencode-directory")
        .and_then(|v| v.to_str().ok())
        .filter(|v| !v.is_empty())
        .map(String::from)
        .unwrap_or_else(|| std::env::current_dir().map(|p| p.display().to_string()).unwrap_or_default())
}

async fn list_sessions(State(st): State<AppState>, headers: HeaderMap) -> Json<Vec<Session>> {
    let dir = headers
        .get("x-opencode-directory")
        .and_then(|v| v.to_str().ok())
        .map(String::from);
    Json(st.app.store.list_sessions(dir.as_deref()))
}

async fn session_create(State(st): State<AppState>, headers: HeaderMap, Json(body): Json<Value>) -> Json<Session> {
    let now = now_ms();
    let directory = body
        .get("directory")
        .and_then(|d| d.as_str())
        .map(String::from)
        .unwrap_or_else(|| dir_of(&headers));
    let s = Session {
        id: hex_id("ses_"),
        slug: slug(),
        project_id: "global".into(),
        path: directory.trim_start_matches('/').to_string(),
        cost: 0.0,
        tokens: Tokens::default(),
        directory,
        title: body
            .get("title")
            .and_then(|t| t.as_str())
            .unwrap_or("New session")
            .to_string(),
        version: "1.19.2".into(),
        time: Time {
            created: now,
            updated: Some(now),
        },
        parent_id: body.get("parentID").and_then(|p| p.as_str()).map(String::from),
        permission: serde_json::from_value(body.get("permission").cloned().unwrap_or(Value::Null)).ok(),
    };
    st.app.store.create_session(&s);
    Json(s)
}

async fn session_get(State(st): State<AppState>, Path(id): Path<String>) -> Response {
    match st.app.store.get_session(&id) {
        Some(s) => Json(s).into_response(),
        None => (StatusCode::NOT_FOUND, Json(json!({"error": "session not found"}))).into_response(),
    }
}

async fn session_patch(State(st): State<AppState>, Path(id): Path<String>, Json(body): Json<Value>) -> Response {
    let Some(mut s) = st.app.store.get_session(&id) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if let Some(t) = body.get("title").and_then(|t| t.as_str()) {
        s.title = t.to_string();
    }
    if let Some(p) = body.get("permission") {
        s.permission = serde_json::from_value(p.clone()).ok();
    }
    s.time.updated = Some(now_ms());
    st.app.store.update_session(&s);
    Json(s).into_response()
}

async fn session_delete(State(st): State<AppState>, Path(id): Path<String>) -> StatusCode {
    st.app.store.delete_session(&id);
    StatusCode::OK
}

async fn session_children(State(st): State<AppState>, Path(id): Path<String>) -> Json<Vec<Session>> {
    Json(st.app.store.children(&id))
}

async fn session_status(State(st): State<AppState>) -> Json<Value> {
    let m: serde_json::Map<String, Value> = st
        .app
        .store
        .busy_sessions()
        .into_iter()
        .map(|id| (id, json!("busy")))
        .collect();
    Json(Value::Object(m))
}

async fn session_messages(State(st): State<AppState>, Path(id): Path<String>) -> Json<Vec<MessageWithParts>> {
    Json(st.app.store.messages(&id))
}

fn prompt_text(parts: &[PromptPart]) -> String {
    parts.iter().map(|p| p.text.as_str()).collect::<Vec<_>>().join("\n")
}

async fn prompt_async(
    State(st): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<PromptRequest>,
) -> StatusCode {
    let app = st.app.clone();
    let engine = st.engine.clone();
    tokio::spawn(async move {
        let _ = engine.start_turn(app, id, req.model.model_id, req.model.provider_id, prompt_text(&req.parts), req.agent);
    });
    StatusCode::OK
}

async fn prompt_sync(
    State(st): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<PromptRequest>,
) -> Response {
    match st
        .engine
        .run_sync(st.app.clone(), id, req.model.model_id, req.model.provider_id, prompt_text(&req.parts), req.agent)
        .await
    {
        Ok(v) => Json(v).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e}))).into_response(),
    }
}

async fn command_run(State(st): State<AppState>, Path(id): Path<String>, Json(body): Json<Value>) -> StatusCode {
    let model_id = body["modelID"].as_str().unwrap_or_default().to_string();
    let provider_id = body["providerID"].as_str().unwrap_or_default().to_string();
    let name = body["command"].as_str().unwrap_or_default().to_string();
    let args = body["arguments"].as_str().unwrap_or_default().to_string();
    let text = body["parts"]
        .as_array()
        .map(|parts| {
            parts
                .iter()
                .map(|p| p["text"].as_str().unwrap_or_default())
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default();
    let full = format!("{name} {args}\n{text}");
    let app = st.app.clone();
    let engine = st.engine.clone();
    let agent = body["agent"].as_str().map(str::to_string);
    tokio::spawn(async move {
        let _ = engine.start_turn(app, id, model_id, provider_id, full, agent);
    });
    StatusCode::OK
}

async fn abort(State(st): State<AppState>, Path(id): Path<String>) -> Json<bool> {
    st.engine.abort(&id);
    Json(true)
}

async fn summarize(State(st): State<AppState>, Path(id): Path<String>) -> StatusCode {
    match st.engine.summarize(st.app.clone(), id) {
        Ok(()) => StatusCode::OK,
        Err(_) => StatusCode::NOT_FOUND,
    }
}

async fn fork(State(st): State<AppState>, Path(id): Path<String>, Json(body): Json<Value>) -> Response {
    let Some(s) = st.app.store.get_session(&id) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let up_to = body.get("messageID").and_then(|m| m.as_str());
    Json(st.app.store.fork(&s, up_to)).into_response()
}

async fn event_stream() -> Sse<impl futures::Stream<Item = Result<Event, std::convert::Infallible>>> {
    let mut rx = bus().subscribe();
    Sse::new(stream! {
        yield Ok(Event::default().data(
            serde_json::to_string(&json!({"id":"evt_connected","type":"server.connected","properties":{}})).unwrap(),
        ));
        loop {
            tokio::select! {
                ev = rx.recv() => match ev {
                    Ok(e) => yield Ok(Event::default().data(serde_json::to_string(&e).unwrap())),
                    Err(_) => break,
                },
                _ = tokio::time::sleep(std::time::Duration::from_secs(10)) => {
                    yield Ok(Event::default().data(
                        serde_json::to_string(&json!({"id":"evt_hb","type":"server.heartbeat","properties":{}})).unwrap(),
                    ));
                }
            }
        }
    })
}

async fn perm_list(State(st): State<AppState>) -> Json<Value> {
    Json(serde_json::to_value(st.app.perms.list()).unwrap())
}

async fn perm_reply(
    State(st): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<PermissionReply>,
) -> StatusCode {
    if st.app.perms.reply(&id, &body.response) {
        StatusCode::OK
    } else {
        StatusCode::NOT_FOUND
    }
}

async fn q_list(State(st): State<AppState>) -> Json<Value> {
    Json(serde_json::to_value(st.app.questions.list()).unwrap())
}

async fn q_reply(State(st): State<AppState>, Path(id): Path<String>, Json(body): Json<QuestionReply>) -> StatusCode {
    if st.app.questions.reply(&id, body.answers) {
        StatusCode::OK
    } else {
        StatusCode::NOT_FOUND
    }
}

async fn q_reject(State(st): State<AppState>, Path(id): Path<String>) -> StatusCode {
    if st.app.questions.reject(&id) {
        StatusCode::OK
    } else {
        StatusCode::NOT_FOUND
    }
}

async fn mcp_status(State(st): State<AppState>) -> Json<Value> {
    Json(st.app.mcp.status())
}

async fn mcp_add(State(st): State<AppState>, Json(body): Json<Value>) -> StatusCode {
    let name = body["name"].as_str().unwrap_or_default().to_string();
    match st.app.mcp.add(&name, body.get("config").cloned().unwrap_or(Value::Null)) {
        Ok(_) => StatusCode::OK,
        Err(_) => StatusCode::BAD_REQUEST,
    }
}

async fn providers(State(st): State<AppState>) -> Json<Value> {
    let cfg = st.app.config.lock().unwrap();
    let all: Vec<Value> = cfg
        .provider
        .iter()
        .map(|(id, p)| {
            json!({
                "id": id,
                "name": if p.name.is_empty() { id } else { &p.name },
                "env": [],
                "source": "config",
                "options": {"baseURL": p.options.base_url},
                "models": p.models,
            })
        })
        .collect();
    let mut connected: Vec<String> = cfg.provider.keys().cloned().collect();
    for a in app_auth_connected(&st.app) {
        if !connected.contains(&a) {
            connected.push(a);
        }
    }
    Json(json!({"all": all, "default": {}, "connected": connected}))
}

fn app_auth_connected(app: &App) -> Vec<String> {
    app.auth.connected()
}

async fn agents() -> Json<Value> {
    let base = |name: &str, desc: &str, mode: &str, extra: Vec<Value>| -> Value {
        let mut perms = vec![
            json!({"permission": "*", "pattern": "*", "action": "allow"}),
            json!({"permission": "doom_loop", "pattern": "*", "action": "ask"}),
            json!({"permission": "external_directory", "pattern": "*", "action": "ask"}),
            json!({"permission": "read", "pattern": "*", "action": "allow"}),
            json!({"permission": "read", "pattern": "*.env", "action": "ask"}),
            json!({"permission": "read", "pattern": "*.env.*", "action": "ask"}),
        ];
        perms.extend(extra);
        json!({"name": name, "description": desc, "mode": mode, "native": true, "options": {}, "permission": perms})
    };
    Json(json!([
        base("build", "The default agent. Executes tools based on configured permissions.", "primary", vec![
            json!({"permission": "question", "pattern": "*", "action": "deny"}),
            json!({"permission": "plan_enter", "pattern": "*", "action": "deny"}),
            json!({"permission": "plan_exit", "pattern": "*", "action": "deny"}),
        ]),
        base("plan", "Plan mode agent. Read-only analysis, no mutations.", "primary", vec![]),
        base("general", "General-purpose agent for researching complex questions", "subagent", vec![]),
        base("subagent", "Subagent for delegating tasks", "subagent", vec![]),
    ]))
}

pub fn skills_json() -> Value {
    json!(crate::tools::skill_list())
}

async fn skills() -> Json<Value> {
    Json(skills_json())
}

async fn commands() -> Json<Value> {
    Json(json!([]))
}

pub async fn serve(hostname: String, port: u16) {
    let st = AppState {
        app: App::new(),
        engine: Arc::new(crate::engine_impl::Engine::new()),
    };
    let router = Router::new()
        .route("/global/health", get(health))
        .route("/api/info", get(api_info))
        .route("/session", get(list_sessions).post(session_create))
        .route(
            "/session/{id}",
            get(session_get).patch(session_patch).delete(session_delete),
        )
        .route("/session/{id}/children", get(session_children))
        .route("/session/status", get(session_status))
        .route("/session/{id}/message", get(session_messages).post(prompt_sync))
        .route("/session/{id}/prompt_async", post(prompt_async))
        .route("/session/{id}/command", post(command_run))
        .route("/session/{id}/abort", post(abort))
        .route("/session/{id}/summarize", post(summarize))
        .route("/session/{id}/fork", post(fork))
        .route("/event", get(event_stream))
        .route("/permission", get(perm_list))
        .route("/permission/{id}/reply", post(perm_reply))
        .route("/question", get(q_list))
        .route("/question/{id}/reply", post(q_reply))
        .route("/question/{id}/reject", post(q_reject))
        .route("/mcp", get(mcp_status).post(mcp_add))
        .route("/provider", get(providers))
        .route("/agent", get(agents))
        .route("/skill", get(skills))
        .route("/command", get(commands))
        .layer(middleware::from_fn(auth_gate))
        .with_state(st);
    let listener = tokio::net::TcpListener::bind((hostname.as_str(), port))
        .await
        .expect("bind");
    println!("opencode server listening on http://{hostname}:{port}");
    use std::io::Write;
    std::io::stdout().flush().ok();
    axum::serve(listener, router).await.expect("serve");
}
