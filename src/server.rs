use crate::config::Config;
use crate::engine::{bus, App, EngineApi};
use crate::events as event;
use crate::types::*;
use async_stream::stream;
use axum::extract::{DefaultBodyLimit, Path, Query, Request, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Map, Value};
use std::sync::Arc;

#[derive(Clone)]
pub struct AppState {
    pub app: App,
    pub engine: Arc<dyn EngineApi>,
    pub shutdown: tokio_util::sync::CancellationToken,
    pub max_request_bytes: usize,
}

impl IntoResponse for OperationError {
    fn into_response(self) -> Response {
        let (status, name) = match self.kind {
            FailureKind::Invalid => (StatusCode::BAD_REQUEST, "ValidationError"),
            FailureKind::NotFound => (StatusCode::NOT_FOUND, "NotFoundError"),
            FailureKind::Busy => (StatusCode::CONFLICT, "SessionBusyError"),
            FailureKind::HookReentry => (StatusCode::CONFLICT, "HookReentryError"),
            _ => (StatusCode::BAD_GATEWAY, "UnknownError"),
        };
        (
            status,
            Json(json!({"name": name, "data": {"message": self.message}})),
        )
            .into_response()
    }
}

#[derive(Default, Deserialize)]
struct ScopeQuery {
    directory: Option<String>,
    limit: Option<usize>,
    search: Option<String>,
    roots: Option<bool>,
    types: Option<String>,
}

fn constant_time_equal(left: &[u8], right: &[u8]) -> bool {
    let mut difference = left.len() ^ right.len();
    for index in 0..left.len().max(right.len()) {
        difference |= usize::from(
            left.get(index).copied().unwrap_or(0) ^ right.get(index).copied().unwrap_or(0),
        );
    }
    difference == 0
}

async fn auth_gate(req: Request, next: Next) -> Response {
    if let Ok(password) = std::env::var("OPENCODE_SERVER_PASSWORD") {
        if !password.is_empty() {
            use base64::Engine;
            let expected = format!("opencode:{password}");
            let supplied = req
                .headers()
                .get(header::AUTHORIZATION)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.split_once(' '))
                .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("basic"))
                .and_then(|(_, data)| base64::engine::general_purpose::STANDARD.decode(data).ok())
                .unwrap_or_default();
            if !constant_time_equal(expected.as_bytes(), &supplied) {
                return (
                    StatusCode::UNAUTHORIZED,
                    [(header::WWW_AUTHENTICATE, "Basic realm=\"opencode\", charset=\"UTF-8\"")],
                    Json(json!({"name": "UnauthorizedError", "data": {"message": "Authentication required"}})),
                ).into_response();
            }
        }
    }
    next.run(req).await
}

async fn health() -> Json<Value> {
    Json(json!({"healthy": true, "version": crate::OPENCODE_COMPAT_VERSION}))
}

fn percent_decode(source: &str) -> Result<String, OperationError> {
    let output = crate::http::percent_bytes(source)
        .map_err(|_| OperationError::invalid("Malformed percent escape in directory header"))?;
    String::from_utf8(output)
        .map_err(|_| OperationError::invalid("Directory header must contain UTF-8"))
}

fn normalized_directory(directory: &str) -> Result<String, OperationError> {
    if directory.contains('\0') {
        return Err(OperationError::invalid(
            "Directory must not contain a null byte",
        ));
    }
    let source = std::path::Path::new(directory);
    if !source.is_absolute() {
        return Err(OperationError::invalid(
            "Directory must be an absolute path",
        ));
    }
    Ok(crate::config::directory_identity(source)
        .to_string_lossy()
        .into_owned())
}

fn requested_directory(
    headers: &HeaderMap,
    query: &ScopeQuery,
) -> Result<Option<String>, OperationError> {
    if let Some(directory) = query.directory.as_deref() {
        return normalized_directory(directory).map(Some);
    }
    let Some(header) = headers.get("x-opencode-directory") else {
        return Ok(None);
    };
    let raw = header
        .to_str()
        .map_err(|_| OperationError::invalid("Invalid directory header"))?;
    if raw.is_empty() {
        return Ok(None);
    }
    normalized_directory(&percent_decode(raw)?).map(Some)
}

fn directory_for(headers: &HeaderMap, query: &ScopeQuery) -> Result<String, OperationError> {
    match requested_directory(headers, query)? {
        Some(directory) => Ok(directory),
        None => std::env::current_dir()
            .map(|path| path.to_string_lossy().into_owned())
            .map_err(|error| {
                OperationError::from(format!("Cannot resolve working directory: {error}"))
            }),
    }
}

fn matches_directory(session: &Session, directory: Option<&str>) -> bool {
    matches_directory_str(&session.directory, directory)
}

fn matches_directory_str(stored: &str, directory: Option<&str>) -> bool {
    directory.is_none_or(|directory| {
        normalized_directory(stored).is_ok_and(|stored| stored == directory)
    })
}

fn scoped_session(
    st: &AppState,
    id: &str,
    headers: &HeaderMap,
    query: &ScopeQuery,
) -> Result<Session, OperationError> {
    let directory = requested_directory(headers, query)?;
    st.app
        .store
        .get_session(id)?
        .filter(|session| matches_directory(session, directory.as_deref()))
        .ok_or_else(|| {
            OperationError::not_found(format!("Session {id} was not found in this directory"))
        })
}

async fn list_sessions(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ScopeQuery>,
) -> Result<Json<Vec<Session>>, OperationError> {
    let directory = requested_directory(&headers, &query)?;
    let store = st.app.store.clone();
    let sessions = tokio::task::spawn_blocking(move || {
        let search = query.search.as_ref().map(|search| search.to_lowercase());
        store.list_sessions_filtered(
            directory.as_deref(),
            query.roots.unwrap_or(false),
            query.limit,
            |session| {
                search
                    .as_ref()
                    .is_none_or(|search| session.title.to_lowercase().contains(search))
            },
        )
    })
    .await
    .map_err(|e| OperationError::from(e.to_string()))??;
    Ok(Json(sessions))
}

fn optional_string<'a>(body: &'a Value, key: &str) -> Result<Option<&'a str>, OperationError> {
    body.get(key)
        .map(|value| {
            value
                .as_str()
                .ok_or_else(|| OperationError::invalid(format!("{key} must be a string")))
        })
        .transpose()
}

async fn session_create(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ScopeQuery>,
    body: Option<Json<Value>>,
) -> Result<Json<Session>, OperationError> {
    let body = body.map(|body| body.0).unwrap_or_else(|| json!({}));
    if !body.is_object() {
        return Err(OperationError::invalid("Session body must be an object"));
    }
    let directory = match body.get("directory") {
        Some(Value::String(directory)) => normalized_directory(directory)?,
        Some(_) => return Err(OperationError::invalid("directory must be a string")),
        None => directory_for(&headers, &query)?,
    };
    let title = optional_string(&body, "title")?.unwrap_or("New session");
    let permission = body
        .get("permission")
        .map(crate::permission::parse_rules)
        .transpose()
        .map_err(OperationError::invalid)?;
    let parent = optional_string(&body, "parentID")?;
    let mut session = Session::new(title, &directory);
    session.parent_id = parent.map(str::to_owned);
    session.permission = permission;
    if body.get("title").is_some() {
        session.title_source = Some(crate::types::TitleSource::Explicit);
    }
    let session = crate::sessions::create(&st.app, session, body.get("metadata")).await?;
    Ok(Json(session))
}

async fn session_get(
    State(st): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Query(query): Query<ScopeQuery>,
) -> Result<Json<Session>, OperationError> {
    Ok(Json(scoped_session(&st, &id, &headers, &query)?))
}

async fn session_patch(
    State(st): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Query(query): Query<ScopeQuery>,
    Json(body): Json<Value>,
) -> Result<Json<Session>, OperationError> {
    let source = scoped_session(&st, &id, &headers, &query)?;
    let session = crate::sessions::update(&st.app, &source, body).await?;
    Ok(Json(session))
}

async fn session_delete(
    State(st): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Query(query): Query<ScopeQuery>,
) -> Result<Json<bool>, OperationError> {
    let session = scoped_session(&st, &id, &headers, &query)?;
    crate::sessions::delete(&st.app, &session).await?;
    Ok(Json(true))
}

async fn session_children(
    State(st): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Query(query): Query<ScopeQuery>,
) -> Result<Json<Vec<Session>>, OperationError> {
    scoped_session(&st, &id, &headers, &query)?;
    Ok(Json(st.app.store.children(&id)?))
}

async fn session_status(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ScopeQuery>,
) -> Result<Json<Value>, OperationError> {
    let directory = requested_directory(&headers, &query)?;
    let statuses = st.app.statuses.lock().unwrap().clone();
    let mut status = Map::new();
    for id in st.app.store.busy_sessions()? {
        if st
            .app
            .store
            .directory_of(&id)
            .is_some_and(|stored| matches_directory_str(&stored, directory.as_deref()))
        {
            status.insert(
                id.clone(),
                json!(statuses
                    .get(&id)
                    .cloned()
                    .unwrap_or(event::SessionStatus::Busy)),
            );
        }
    }
    Ok(Json(Value::Object(status)))
}

async fn session_messages(
    State(st): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Query(query): Query<ScopeQuery>,
) -> Result<Json<Vec<MessageWithParts>>, OperationError> {
    scoped_session(&st, &id, &headers, &query)?;
    let store = st.app.store.clone();
    let messages = tokio::task::spawn_blocking(move || store.messages_limited(&id, query.limit))
        .await
        .map_err(|e| OperationError::from(e.to_string()))??;
    Ok(Json(messages))
}

async fn session_message(
    State(st): State<AppState>,
    Path((id, message_id)): Path<(String, String)>,
    headers: HeaderMap,
    Query(query): Query<ScopeQuery>,
) -> Result<Json<MessageWithParts>, OperationError> {
    scoped_session(&st, &id, &headers, &query)?;
    st.app
        .store
        .message(&id, &message_id)?
        .map(Json)
        .ok_or_else(|| OperationError::not_found("Message not found"))
}

async fn prompt_async(
    State(st): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Query(query): Query<ScopeQuery>,
    Json(request): Json<PromptRequest>,
) -> Result<StatusCode, OperationError> {
    scoped_session(&st, &id, &headers, &query)?;
    st.engine.admit(st.app.clone(), id, request).await?;
    Ok(StatusCode::NO_CONTENT)
}

fn reject_hook_reentry(
    st: &AppState,
    session: &Session,
    headers: &HeaderMap,
) -> Result<(), OperationError> {
    let Some(invocation) = headers.get("x-ocmini-hook-invocation-id") else {
        return Ok(());
    };
    let invocation = invocation
        .to_str()
        .map_err(|_| OperationError::invalid("Invalid hook invocation ID"))?;
    let app = st.app.for_directory(&session.directory)?;
    if app.hooks.is_active_invocation(invocation, &session.id) {
        return Err(OperationError::new(
            FailureKind::HookReentry,
            "A hook cannot synchronously wait for a prompt in its own session",
        ));
    }
    Ok(())
}

async fn prompt_sync(
    State(st): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Query(query): Query<ScopeQuery>,
    Json(request): Json<PromptRequest>,
) -> Result<Json<MessageWithParts>, OperationError> {
    let session = scoped_session(&st, &id, &headers, &query)?;
    reject_hook_reentry(&st, &session, &headers)?;
    st.engine
        .run_sync(st.app.clone(), id, request)
        .await
        .map(Json)
}

async fn command_run(
    State(st): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Query(query): Query<ScopeQuery>,
    Json(body): Json<Value>,
) -> Result<Json<MessageWithParts>, OperationError> {
    let session = scoped_session(&st, &id, &headers, &query)?;
    reject_hook_reentry(&st, &session, &headers)?;
    let name = body
        .get("command")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| OperationError::invalid("command must be a nonempty string"))?;
    let arguments = optional_string(&body, "arguments")?.unwrap_or("");
    let config = Config::load_for(&session.directory).map_err(OperationError::invalid)?;
    if !config.command.contains_key(name) {
        return Err(OperationError::not_found(format!(
            "Command {name} is not configured"
        )));
    }
    let mut parts = Vec::new();
    if let Some(files) = body.get("parts") {
        let files = files
            .as_array()
            .ok_or_else(|| OperationError::invalid("parts must be an array"))?;
        if files
            .iter()
            .any(|part| part.get("type").and_then(Value::as_str) != Some("file"))
        {
            return Err(OperationError::invalid(
                "Command parts must contain file attachments",
            ));
        }
        parts.extend(files.iter().cloned());
    }
    let request = crate::commands::prepare(
        &config,
        name,
        arguments,
        PromptRequest {
            model: optional_string(&body, "model")?
                .map(ModelRef::parse)
                .transpose()
                .map_err(OperationError::invalid)?,
            parts,
            message_id: optional_string(&body, "messageID")?.map(str::to_owned),
            agent: optional_string(&body, "agent")?.map(str::to_owned),
            variant: optional_string(&body, "variant")?.map(str::to_owned),
            ..Default::default()
        },
        None,
    )
    .map_err(OperationError::invalid)?;
    st.engine
        .run_sync(st.app.clone(), id, request)
        .await
        .map(Json)
}

async fn abort(
    State(st): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Query(query): Query<ScopeQuery>,
) -> Result<Json<bool>, OperationError> {
    scoped_session(&st, &id, &headers, &query)?;
    st.engine.abort(&id);
    Ok(Json(true))
}

async fn summarize(
    State(st): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Query(query): Query<ScopeQuery>,
    body: Option<Json<Value>>,
) -> Result<Json<bool>, OperationError> {
    let session = scoped_session(&st, &id, &headers, &query)?;
    reject_hook_reentry(&st, &session, &headers)?;
    let body = body.map(|body| body.0).unwrap_or_else(|| json!({}));
    if !body.is_object() {
        return Err(OperationError::invalid("Summarize body must be an object"));
    }
    let model = if body.get("providerID").is_some() || body.get("modelID").is_some() {
        let model: ModelRef = serde_json::from_value(body).map_err(|_| {
            OperationError::invalid("summarize requires both providerID and modelID")
        })?;
        model.validate().map_err(OperationError::invalid)?;
        Some(model)
    } else {
        None
    };
    let done = st.engine.summarize(st.app.clone(), id, model)?;
    let result = done
        .await
        .map_err(|_| OperationError::from("Compaction task ended before returning a result"))?;
    result?;
    Ok(Json(true))
}

async fn fork(
    State(st): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Query(query): Query<ScopeQuery>,
    body: Option<Json<Value>>,
) -> Result<Json<Session>, OperationError> {
    let session = scoped_session(&st, &id, &headers, &query)?;
    let body = body.map(|body| body.0).unwrap_or_else(|| json!({}));
    if !body.is_object() {
        return Err(OperationError::invalid("Fork body must be an object"));
    }
    let boundary = optional_string(&body, "messageID")?;
    let fork = crate::sessions::fork(&st.app, &session, boundary).await?;
    Ok(Json(fork))
}

fn explicit_event_directory(event: &BusEvent) -> Option<&str> {
    event
        .properties
        .get("directory")
        .and_then(Value::as_str)
        .or_else(|| {
            event
                .properties
                .get("info")
                .and_then(|info| info.get("directory"))
                .and_then(Value::as_str)
        })
}

fn event_types(csv: Option<&str>) -> Option<std::collections::HashSet<String>> {
    csv.map(|csv| {
        csv.split(',')
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_owned)
            .collect()
    })
}

fn event_in_directory(event: &BusEvent, app: &App, directory: Option<&str>) -> bool {
    let Some(directory) = directory else {
        return true;
    };
    if let Some(event_directory) = explicit_event_directory(event) {
        return normalized_directory(event_directory).is_ok_and(|value| value == directory);
    }
    crate::client::event_session(event).is_none_or(|id| {
        app.store
            .directory_of(id)
            .is_some_and(|stored| matches_directory_str(&stored, Some(directory)))
    })
}

fn event_frame<'a>(event: impl Into<event::Event<'a>>) -> Event {
    Event::default()
        .data(serde_json::to_string(&BusEvent::new(event, json!({}))).expect("serializable event"))
}

async fn event_stream(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ScopeQuery>,
) -> Result<Response, OperationError> {
    let directory = requested_directory(&headers, &query)?;
    let types = event_types(query.types.as_deref());
    let selected = move |name: &str| types.as_ref().is_none_or(|types| types.contains(name));
    let mut receiver = bus().subscribe();
    let events = stream! {
        if selected(event::ServerKind::Connected.as_str()) { yield Ok::<Event, std::convert::Infallible>(event_frame(event::Server::Connected {})); }
        let mut heartbeat = tokio::time::interval(std::time::Duration::from_secs(10));
        heartbeat.tick().await;
        loop {
            tokio::select! {
                _ = st.shutdown.cancelled() => break,
                event = receiver.recv() => match event {
                    Ok(event) => if selected(event.kind.as_str()) && event_in_directory(&event, &st.app, directory.as_deref()) { yield Ok(Event::default().data(serde_json::to_string(&*event).expect("serializable event"))); },
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(count)) => {
                        // Losing text or tool events must fail visibly instead of completing a partial turn.
                        for id in st.app.store.busy_sessions().unwrap_or_default() {
                            if selected(event::SessionKind::Error.as_str()) && st.app.store.get_session(&id).ok().flatten().is_some_and(|session| matches_directory(&session, directory.as_deref())) {
                                let error = json!({"name":"UnknownError","data":{"message":format!("Event subscriber fell behind by {count} events; reconnect and reload the session")}});
                                yield Ok(event_frame(event::Session::Error { session_id: &id, error: &error }));
                            }
                        }
                        break;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                },
                _ = heartbeat.tick() => { if selected(event::ServerKind::Heartbeat.as_str()) { yield Ok(event_frame(event::Server::Heartbeat {})); } }
            }
        }
    };
    Ok(Sse::new(events).into_response())
}

async fn permission_list(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ScopeQuery>,
) -> Result<Json<Value>, OperationError> {
    let directory = requested_directory(&headers, &query)?;
    let requests: Vec<_> = st
        .app
        .perms
        .list()
        .into_iter()
        .filter(|request| {
            st.app
                .store
                .directory_of(&request.session_id)
                .is_some_and(|stored| matches_directory_str(&stored, directory.as_deref()))
        })
        .collect();
    Ok(Json(json!(requests)))
}

fn reply_permission(
    st: &AppState,
    id: &str,
    session_id: Option<&str>,
    headers: &HeaderMap,
    query: &ScopeQuery,
    body: &Value,
) -> Result<Json<bool>, OperationError> {
    let reply = body
        .get("reply")
        .or_else(|| body.get("response"))
        .and_then(Value::as_str)
        .filter(|reply| matches!(*reply, "once" | "always" | "reject"))
        .ok_or_else(|| OperationError::invalid("reply must be once, always, or reject"))?;
    let request = st
        .app
        .perms
        .list()
        .into_iter()
        .find(|request| request.id == id)
        .ok_or_else(|| OperationError::not_found("Permission request not found"))?;
    if session_id.is_some_and(|session_id| request.session_id != session_id) {
        return Err(OperationError::not_found(
            "Permission request not found in this session",
        ));
    }
    scoped_session(st, &request.session_id, headers, query)?;
    if !st.app.perms.reply(id, reply) {
        return Err(OperationError::not_found(
            "Permission request has already been resolved",
        ));
    }
    Ok(Json(true))
}

async fn permission_reply(
    State(st): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Query(query): Query<ScopeQuery>,
    Json(body): Json<Value>,
) -> Result<Json<bool>, OperationError> {
    reply_permission(&st, &id, None, &headers, &query, &body)
}

async fn permission_reply_session(
    State(st): State<AppState>,
    Path((session_id, id)): Path<(String, String)>,
    headers: HeaderMap,
    Query(query): Query<ScopeQuery>,
    Json(body): Json<Value>,
) -> Result<Json<bool>, OperationError> {
    reply_permission(&st, &id, Some(&session_id), &headers, &query, &body)
}

async fn question_list(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ScopeQuery>,
) -> Result<Json<Value>, OperationError> {
    let directory = requested_directory(&headers, &query)?;
    let requests: Vec<_> = st
        .app
        .questions
        .list()
        .into_iter()
        .filter(|request| {
            st.app
                .store
                .directory_of(&request.session_id)
                .is_some_and(|stored| matches_directory_str(&stored, directory.as_deref()))
        })
        .collect();
    Ok(Json(json!(requests)))
}

async fn question_reply(
    State(st): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Query(query): Query<ScopeQuery>,
    Json(body): Json<QuestionReply>,
) -> Result<Json<bool>, OperationError> {
    let request = st
        .app
        .questions
        .list()
        .into_iter()
        .find(|request| request.id == id)
        .ok_or_else(|| OperationError::not_found("Question request not found"))?;
    scoped_session(&st, &request.session_id, &headers, &query)?;
    request
        .validate_answers(&body.answers)
        .map_err(OperationError::invalid)?;
    if !st.app.questions.reply(&id, body.answers) {
        return Err(OperationError::not_found(
            "Question request has already been resolved",
        ));
    }
    Ok(Json(true))
}

async fn question_reject(
    State(st): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Query(query): Query<ScopeQuery>,
) -> Result<Json<bool>, OperationError> {
    let request = st
        .app
        .questions
        .list()
        .into_iter()
        .find(|request| request.id == id)
        .ok_or_else(|| OperationError::not_found("Question request not found"))?;
    scoped_session(&st, &request.session_id, &headers, &query)?;
    if !st.app.questions.reject(&id) {
        return Err(OperationError::not_found(
            "Question request has already been resolved",
        ));
    }
    Ok(Json(true))
}

async fn mcp_app(
    st: &AppState,
    headers: &HeaderMap,
    query: &ScopeQuery,
) -> Result<App, OperationError> {
    let app = st.app.for_directory(&directory_for(headers, query)?)?;
    app.wait_mcp_ready().await;
    Ok(app)
}

async fn mcp_status(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ScopeQuery>,
) -> Result<Json<Value>, OperationError> {
    let app = mcp_app(&st, &headers, &query).await?;
    Ok(Json(app.mcp().status()))
}

async fn mcp_add(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ScopeQuery>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, OperationError> {
    let name = body
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
        .ok_or_else(|| OperationError::invalid("MCP name is required"))?;
    let config = body
        .get("config")
        .cloned()
        .ok_or_else(|| OperationError::invalid("MCP config is required"))?;
    let app = mcp_app(&st, &headers, &query).await?;
    app.mcp().add_async(name, config).await?;
    Ok(Json(app.mcp().status()))
}

async fn mcp_connect(
    State(st): State<AppState>,
    Path(name): Path<String>,
    headers: HeaderMap,
    Query(query): Query<ScopeQuery>,
) -> Result<Json<bool>, OperationError> {
    let app = mcp_app(&st, &headers, &query).await?;
    let status = app.mcp().connect_async(&name).await?;
    if status["status"] != "connected" {
        let message = status["error"]
            .as_str()
            .unwrap_or("MCP connection did not reach the connected state");
        return Err(OperationError::from(message.to_owned()));
    }
    Ok(Json(true))
}

async fn mcp_disconnect(
    State(st): State<AppState>,
    Path(name): Path<String>,
    headers: HeaderMap,
    Query(query): Query<ScopeQuery>,
) -> Result<Json<bool>, OperationError> {
    let app = mcp_app(&st, &headers, &query).await?;
    app.mcp().disconnect(&name)?;
    Ok(Json(true))
}

async fn providers(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ScopeQuery>,
) -> Result<Json<Value>, OperationError> {
    let config =
        Config::load_for(&directory_for(&headers, &query)?).map_err(OperationError::invalid)?;
    let (config, errors) = crate::provider::discover_models(&config, &st.app.auth).await;
    if !errors.is_empty() && config.catalog().is_empty() {
        return Err(OperationError::from(errors.join("; ")));
    }
    for error in errors {
        eprintln!("ocmini: model discovery: {error}");
    }
    Ok(Json(crate::inventory::provider_inventory(
        &config,
        &st.app.auth,
    )))
}

async fn agents(
    headers: HeaderMap,
    Query(query): Query<ScopeQuery>,
) -> Result<Json<Value>, OperationError> {
    let config =
        Config::load_for(&directory_for(&headers, &query)?).map_err(OperationError::invalid)?;
    Ok(Json(crate::inventory::agents_json(&config)))
}

async fn skills(
    headers: HeaderMap,
    Query(query): Query<ScopeQuery>,
) -> Result<Json<Value>, OperationError> {
    let directory = directory_for(&headers, &query)?;
    let config = Config::load_for(&directory).map_err(OperationError::invalid)?;
    Ok(Json(crate::inventory::skills_json_for(&directory, &config)))
}

async fn commands(
    headers: HeaderMap,
    Query(query): Query<ScopeQuery>,
) -> Result<Json<Value>, OperationError> {
    let config =
        Config::load_for(&directory_for(&headers, &query)?).map_err(OperationError::invalid)?;
    Ok(Json(crate::inventory::commands_json(&config)))
}

async fn access_log(request: Request, next: Next) -> Response {
    let method = request.method().clone();
    let path = request.uri().path().to_owned();
    let response = next.run(request).await;
    if std::env::var_os("OC_HTTP_LOG").is_some() {
        eprintln!("[http] {method} {path} {}", response.status().as_u16());
    }
    response
}

async fn not_found() -> OperationError {
    OperationError::not_found("Route not found")
}

pub fn router(state: AppState) -> Router {
    let body_limit = state.max_request_bytes;
    Router::new()
        .route("/global/health", get(health))
        .route("/session", get(list_sessions).post(session_create))
        .route("/session/status", get(session_status))
        .route(
            "/session/{id}",
            get(session_get).patch(session_patch).delete(session_delete),
        )
        .route("/session/{id}/children", get(session_children))
        .route(
            "/session/{id}/message",
            get(session_messages).post(prompt_sync),
        )
        .route("/session/{id}/message/{messageID}", get(session_message))
        .route("/session/{id}/prompt_async", post(prompt_async))
        .route("/session/{id}/command", post(command_run))
        .route("/session/{id}/abort", post(abort))
        .route("/session/{id}/summarize", post(summarize))
        .route("/session/{id}/fork", post(fork))
        .route("/event", get(event_stream))
        .route("/permission", get(permission_list))
        .route("/permission/{id}/reply", post(permission_reply))
        .route(
            "/session/{id}/permissions/{permissionID}",
            post(permission_reply_session),
        )
        .route("/question", get(question_list))
        .route("/question/{id}/reply", post(question_reply))
        .route("/question/{id}/reject", post(question_reject))
        .route("/mcp", get(mcp_status).post(mcp_add))
        .route("/mcp/{name}/connect", post(mcp_connect))
        .route("/mcp/{name}/disconnect", post(mcp_disconnect))
        .route("/provider", get(providers))
        .route("/agent", get(agents))
        .route("/skill", get(skills))
        .route("/command", get(commands))
        .fallback(not_found)
        .layer(DefaultBodyLimit::max(body_limit))
        .layer(middleware::from_fn(auth_gate))
        .layer(middleware::from_fn(access_log))
        .with_state(state)
}

pub async fn serve(app: App, hostname: String, port: u16) -> Result<(), String> {
    let max_request_bytes = match std::env::var("OCMINI_MAX_REQUEST_BYTES") {
        Ok(value) => value
            .parse::<usize>()
            .ok()
            .filter(|value| *value > 0)
            .ok_or("OCMINI_MAX_REQUEST_BYTES must be a positive integer")?,
        Err(std::env::VarError::NotPresent) => 128 * 1024 * 1024,
        Err(_) => return Err("OCMINI_MAX_REQUEST_BYTES must contain valid UTF-8".into()),
    };
    let directory = std::env::current_dir()
        .map_err(|error| format!("Cannot resolve working directory: {error}"))?;
    Config::load_for(&directory.to_string_lossy())?;
    let listener = tokio::net::TcpListener::bind((hostname.as_str(), port))
        .await
        .map_err(|error| format!("Cannot listen on {hostname}:{port}: {error}"))?;
    let state = AppState {
        app,
        engine: Arc::new(crate::engine_impl::Engine::new()),
        shutdown: tokio_util::sync::CancellationToken::new(),
        max_request_bytes,
    };
    state.engine.recover(state.app.clone());
    let address = listener.local_addr().map_err(|error| error.to_string())?;
    let announced_host = if address.is_ipv6() {
        format!("[{}]", address.ip())
    } else {
        address.ip().to_string()
    };
    println!(
        "opencode server listening on http://{announced_host}:{}",
        address.port()
    );
    use std::io::Write;
    std::io::stdout()
        .flush()
        .map_err(|error| error.to_string())?;
    let shutdown_state = state.clone();
    axum::serve(listener, router(state))
        .with_graceful_shutdown(async move {
            #[cfg(unix)]
            {
                if let Ok(mut terminate) =
                    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                {
                    tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = terminate.recv() => {} }
                } else {
                    let _ = tokio::signal::ctrl_c().await;
                }
            }
            #[cfg(not(unix))]
            {
                let _ = tokio::signal::ctrl_c().await;
            }
            for id in shutdown_state.app.store.busy_sessions().unwrap_or_default() {
                shutdown_state.engine.abort(&id);
            }
            shutdown_state.shutdown.cancel();
        })
        .await
        .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_filters_use_exact_csv_names_and_explicit_scope() {
        assert!(event_types(None).is_none());
        let types = event_types(Some(" file.edited,ocmini.task.ended,file.edited, ")).unwrap();
        assert_eq!(types.len(), 2);
        assert!(types.contains("file.edited"));
        assert!(!types.contains("file"));
        assert!(event_types(Some("")).unwrap().is_empty());
        let event = BusEvent {
            id: "event".into(),
            kind: event::EventKind::File(event::FileKind::Edited),
            properties: json!({"directory":"/one", "info":{"directory":"/two"}}),
        };
        assert_eq!(explicit_event_directory(&event), Some("/one"));
    }

    #[test]
    fn command_arguments_are_literal_and_last_position_collects_remainder() {
        assert_eq!(
            crate::commands::expand("First $1. Rest $2.", "\"alpha beta\" gamma delta"),
            "First alpha beta. Rest gamma delta."
        );
        assert_eq!(
            crate::commands::expand("$ARGUMENTS / $ARGUMENTS", "$& $$ $` $'"),
            "$& $$ $` $' / $& $$ $` $'"
        );
        assert_eq!(
            crate::commands::expand("Review.", "branch main"),
            "Review.\n\nbranch main"
        );
        assert_eq!(crate::commands::expand("$2", "only"), "");
    }

    #[test]
    fn directory_header_decodes_unicode() {
        assert_eq!(
            percent_decode("%2Ftmp%2Fcaf%C3%A9%20%25done").unwrap(),
            "/tmp/café %done"
        );
        assert!(percent_decode("%2G").is_err());
        assert!(percent_decode("%FF").is_err());
    }

    #[test]
    fn authorization_compares_full_credential() {
        assert!(constant_time_equal(b"opencode:abc", b"opencode:abc"));
        assert!(!constant_time_equal(b"opencode:abc", b"opencode:ab"));
        assert!(!constant_time_equal(b"opencode:abc", b"opencode:abc\0"));
    }
    #[test]
    fn provider_prose_cannot_change_the_http_failure_class() {
        for message in [
            "model not found",
            "provider is busy",
            "invalid provider response",
        ] {
            assert_eq!(
                OperationError::from(message).into_response().status(),
                StatusCode::BAD_GATEWAY
            );
        }
        assert_eq!(
            OperationError::not_found("resource absent")
                .into_response()
                .status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            OperationError::invalid("bad chosen settings")
                .into_response()
                .status(),
            StatusCode::BAD_REQUEST
        );
    }
}
