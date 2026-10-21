use crate::auth::AuthStore;
use crate::config::Config;
use crate::engine::{bus, App, EngineApi};
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
use std::collections::BTreeMap;
use std::path::{Component, PathBuf};
use std::sync::Arc;

#[derive(Clone)]
pub struct AppState {
    pub app: App,
    pub engine: Arc<dyn EngineApi>,
    pub shutdown: tokio_util::sync::CancellationToken,
    pub max_request_bytes: usize,
}

#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    name: &'static str,
    message: String,
}

impl ApiError {
    fn invalid(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            name: "ValidationError",
            message: message.into(),
        }
    }

    fn not_found(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            name: "NotFoundError",
            message: message.into(),
        }
    }

    fn busy() -> Self {
        Self {
            status: StatusCode::CONFLICT,
            name: "SessionBusyError",
            message: "The session is busy; abort or finish its turn first".into(),
        }
    }

    fn runtime(message: String) -> Self {
        let lower = message.to_ascii_lowercase();
        if lower.contains("not found") || lower == "no session" {
            return Self::not_found(message);
        }
        if lower.contains("busy") {
            return Self::busy();
        }
        if lower.contains("invalid")
            || lower.contains("unknown")
            || lower.contains("no model")
            || lower.contains("not configured")
            || lower.contains("requires")
            || lower.contains("unsupported")
            || lower.contains("must ")
            || lower.contains("already belongs")
            || lower.contains("duplicate")
            || lower.contains(" is disabled")
        {
            return Self::invalid(message);
        }
        Self {
            status: StatusCode::BAD_GATEWAY,
            name: "UnknownError",
            message,
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(json!({"name": self.name, "data": {"message": self.message}})),
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

fn percent_decode(source: &str) -> Result<String, ApiError> {
    let mut output = Vec::with_capacity(source.len());
    let bytes = source.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let Some(pair) = bytes.get(index + 1..index + 3) else {
                return Err(ApiError::invalid(
                    "Malformed percent escape in directory header",
                ));
            };
            match (
                (pair[0] as char).to_digit(16),
                (pair[1] as char).to_digit(16),
            ) {
                (Some(high), Some(low)) => output.push((high * 16 + low) as u8),
                _ => {
                    return Err(ApiError::invalid(
                        "Malformed percent escape in directory header",
                    ))
                }
            }
            index += 3;
        } else {
            output.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(output).map_err(|_| ApiError::invalid("Directory header must contain UTF-8"))
}

fn normalized_directory(directory: &str) -> Result<String, ApiError> {
    if directory.contains('\0') {
        return Err(ApiError::invalid("Directory must not contain a null byte"));
    }
    let source = std::path::Path::new(directory);
    if !source.is_absolute() {
        return Err(ApiError::invalid("Directory must be an absolute path"));
    }
    let mut path = PathBuf::new();
    for component in source.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                path.pop();
            }
            component => path.push(component.as_os_str()),
        }
    }
    Ok(std::fs::canonicalize(&path)
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned())
}

fn requested_directory(
    headers: &HeaderMap,
    query: &ScopeQuery,
) -> Result<Option<String>, ApiError> {
    if let Some(directory) = query.directory.as_deref() {
        return normalized_directory(directory).map(Some);
    }
    let Some(header) = headers.get("x-opencode-directory") else {
        return Ok(None);
    };
    let raw = header
        .to_str()
        .map_err(|_| ApiError::invalid("Invalid directory header"))?;
    if raw.is_empty() {
        return Ok(None);
    }
    normalized_directory(&percent_decode(raw)?).map(Some)
}

fn directory_for(headers: &HeaderMap, query: &ScopeQuery) -> Result<String, ApiError> {
    match requested_directory(headers, query)? {
        Some(directory) => Ok(directory),
        None => std::env::current_dir()
            .map(|path| path.to_string_lossy().into_owned())
            .map_err(|error| {
                ApiError::runtime(format!("Cannot resolve working directory: {error}"))
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
) -> Result<Session, ApiError> {
    let directory = requested_directory(headers, query)?;
    st.app
        .store
        .get_session(id)
        .map_err(ApiError::runtime)?
        .filter(|session| matches_directory(session, directory.as_deref()))
        .ok_or_else(|| ApiError::not_found(format!("Session {id} was not found in this directory")))
}

fn decode_rules(value: &Value) -> Result<Vec<PermissionRule>, ApiError> {
    let rules: Vec<PermissionRule> = serde_json::from_value(value.clone())
        .map_err(|error| ApiError::invalid(format!("Invalid permission rules: {error}")))?;
    if rules.iter().any(|rule| {
        rule.permission.is_empty() || !matches!(rule.action.as_str(), "allow" | "ask" | "deny")
    }) {
        return Err(ApiError::invalid(
            "Permission rules require a name and allow, ask, or deny action",
        ));
    }
    Ok(rules)
}

async fn list_sessions(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ScopeQuery>,
) -> Result<Json<Vec<Session>>, ApiError> {
    let directory = requested_directory(&headers, &query)?;
    let store = st.app.store.clone();
    let sessions = tokio::task::spawn_blocking(move || {
        let search = query.search.as_ref().map(|search| search.to_lowercase());
        store.list_sessions_filtered(query.limit, |session| {
            matches_directory(session, directory.as_deref())
                && (!query.roots.unwrap_or(false) || session.parent_id.is_none())
                && search
                    .as_ref()
                    .is_none_or(|search| session.title.to_lowercase().contains(search))
        })
    })
    .await
    .map_err(|e| ApiError::runtime(e.to_string()))?
    .map_err(ApiError::runtime)?;
    Ok(Json(sessions))
}

async fn session_create(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ScopeQuery>,
    body: Option<Json<Value>>,
) -> Result<Json<Session>, ApiError> {
    let body = body.map(|body| body.0).unwrap_or_else(|| json!({}));
    if !body.is_object() {
        return Err(ApiError::invalid("Session body must be an object"));
    }
    let directory = match body.get("directory") {
        Some(Value::String(directory)) => normalized_directory(directory)?,
        Some(_) => return Err(ApiError::invalid("directory must be a string")),
        None => directory_for(&headers, &query)?,
    };
    Config::load_for(&directory).map_err(ApiError::invalid)?;
    let title = body
        .get("title")
        .map(|value| {
            value
                .as_str()
                .ok_or_else(|| ApiError::invalid("title must be a string"))
        })
        .transpose()?
        .unwrap_or("New session");
    let permission = body.get("permission").map(decode_rules).transpose()?;
    let parent = body
        .get("parentID")
        .map(|value| {
            value
                .as_str()
                .ok_or_else(|| ApiError::invalid("parentID must be a string"))
        })
        .transpose()?;
    if let Some(parent) = parent {
        let parent_session = st
            .app
            .store
            .get_session(parent)
            .map_err(ApiError::runtime)?
            .ok_or_else(|| ApiError::not_found("Parent session not found"))?;
        if !matches_directory(&parent_session, Some(&directory)) {
            return Err(ApiError::not_found(
                "Parent session not found in this directory",
            ));
        }
    }
    let mut session = st
        .app
        .store
        .try_create_session_owned(title, &directory)
        .map_err(ApiError::runtime)?;
    session.permission = permission;
    session.parent_id = parent.map(str::to_owned);
    st.app
        .store
        .update_session(&session)
        .map_err(ApiError::runtime)?;
    bus().emit("session.created", json!({"info": session}));
    Ok(Json(session))
}

async fn session_get(
    State(st): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Query(query): Query<ScopeQuery>,
) -> Result<Json<Session>, ApiError> {
    Ok(Json(scoped_session(&st, &id, &headers, &query)?))
}

async fn session_patch(
    State(st): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Query(query): Query<ScopeQuery>,
    Json(body): Json<Value>,
) -> Result<Json<Session>, ApiError> {
    let mut session = scoped_session(&st, &id, &headers, &query)?;
    if !body.is_object() {
        return Err(ApiError::invalid("Session body must be an object"));
    }
    if let Some(title) = body.get("title") {
        session.title = title
            .as_str()
            .ok_or_else(|| ApiError::invalid("title must be a string"))?
            .to_owned();
    }
    if let Some(permission) = body.get("permission") {
        session.permission = Some(decode_rules(permission)?);
    }
    session.time.updated = Some(now_ms());
    st.app
        .store
        .update_session(&session)
        .map_err(ApiError::runtime)?;
    bus().emit("session.updated", json!({"info": session}));
    Ok(Json(session))
}

async fn session_delete(
    State(st): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Query(query): Query<ScopeQuery>,
) -> Result<Json<bool>, ApiError> {
    let session = scoped_session(&st, &id, &headers, &query)?;
    if st.app.store.is_busy(&id).map_err(ApiError::runtime)? {
        return Err(ApiError::busy());
    }
    st.app
        .store
        .delete_session(&id)
        .map_err(ApiError::runtime)?;
    crate::tools::cancel_background_jobs(&id);
    bus().emit("session.deleted", json!({"info": session}));
    Ok(Json(true))
}

async fn session_children(
    State(st): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Query(query): Query<ScopeQuery>,
) -> Result<Json<Vec<Session>>, ApiError> {
    scoped_session(&st, &id, &headers, &query)?;
    Ok(Json(st.app.store.children(&id).map_err(ApiError::runtime)?))
}

async fn session_status(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ScopeQuery>,
) -> Result<Json<Value>, ApiError> {
    let directory = requested_directory(&headers, &query)?;
    let statuses = st.app.statuses.lock().unwrap().clone();
    let mut status = Map::new();
    for id in st.app.store.busy_sessions().map_err(ApiError::runtime)? {
        if st
            .app
            .store
            .get_session(&id)
            .map_err(ApiError::runtime)?
            .is_some_and(|session| matches_directory(&session, directory.as_deref()))
        {
            status.insert(
                id.clone(),
                statuses
                    .get(&id)
                    .cloned()
                    .unwrap_or_else(|| json!({"type": "busy"})),
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
) -> Result<Json<Vec<MessageWithParts>>, ApiError> {
    scoped_session(&st, &id, &headers, &query)?;
    let store = st.app.store.clone();
    let messages = tokio::task::spawn_blocking(move || store.messages_limited(&id, query.limit))
        .await
        .map_err(|e| ApiError::runtime(e.to_string()))?
        .map_err(ApiError::runtime)?;
    Ok(Json(messages))
}

async fn session_message(
    State(st): State<AppState>,
    Path((id, message_id)): Path<(String, String)>,
    headers: HeaderMap,
    Query(query): Query<ScopeQuery>,
) -> Result<Json<MessageWithParts>, ApiError> {
    scoped_session(&st, &id, &headers, &query)?;
    st.app
        .store
        .message(&id, &message_id)
        .map_err(ApiError::runtime)?
        .map(Json)
        .ok_or_else(|| ApiError::not_found("Message not found"))
}

fn validate_prompt(request: &PromptRequest) -> Result<(), ApiError> {
    if request.parts.is_empty() {
        return Err(ApiError::invalid("parts must contain text or a file"));
    }
    if let Some(model) = &request.model {
        if model.provider_id.trim().is_empty() || model.model_id.trim().is_empty() {
            return Err(ApiError::invalid(
                "model requires nonempty providerID and modelID",
            ));
        }
    }
    if request.message_id.as_ref().is_some_and(|id| id.is_empty()) {
        return Err(ApiError::invalid("messageID must not be empty"));
    }
    for part in &request.parts {
        match part.get("type").and_then(Value::as_str) {
            Some("text") if part.get("text").is_some_and(Value::is_string) => {}
            Some("file")
                if part.get("mime").is_some_and(Value::is_string)
                    && part.get("url").is_some_and(Value::is_string) => {}
            _ => {
                return Err(ApiError::invalid(
                    "Prompt parts must be text with text, or file with mime and url",
                ))
            }
        }
    }
    Ok(())
}

async fn prompt_async(
    State(st): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Query(query): Query<ScopeQuery>,
    Json(request): Json<PromptRequest>,
) -> Result<StatusCode, ApiError> {
    scoped_session(&st, &id, &headers, &query)?;
    validate_prompt(&request)?;
    st.engine
        .start_turn(st.app.clone(), id, request)
        .map_err(ApiError::runtime)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn prompt_sync(
    State(st): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Query(query): Query<ScopeQuery>,
    Json(request): Json<PromptRequest>,
) -> Result<Json<Value>, ApiError> {
    scoped_session(&st, &id, &headers, &query)?;
    validate_prompt(&request)?;
    st.engine
        .run_sync(st.app.clone(), id, request)
        .await
        .map(Json)
        .map_err(ApiError::runtime)
}

pub fn expand_command(template: &str, arguments: &str) -> String {
    let argument_pattern = regex::Regex::new(r#"(?:\[Image\s+\d+\]|"[^"]*"|'[^']*'|[^\s"']+)"#)
        .expect("constant regex");
    let arguments_list: Vec<String> = argument_pattern
        .find_iter(arguments)
        .map(|item| item.as_str().trim_matches(['\'', '"']).to_owned())
        .collect();
    let placeholder = regex::Regex::new(r"\$(\d+)").expect("constant regex");
    let has_numbered = placeholder.is_match(template);
    let last = placeholder
        .captures_iter(template)
        .filter_map(|capture| capture[1].parse::<usize>().ok())
        .max()
        .unwrap_or(0);
    let expanded = placeholder.replace_all(template, |capture: &regex::Captures<'_>| {
        let position = capture[1].parse::<usize>().unwrap_or(0);
        let Some(index) = position.checked_sub(1) else {
            return String::new();
        };
        if index >= arguments_list.len() {
            return String::new();
        }
        if position == last {
            arguments_list[index..].join(" ")
        } else {
            arguments_list[index].clone()
        }
    });
    let expanded = expanded.replace("$ARGUMENTS", arguments);
    if !has_numbered && !template.contains("$ARGUMENTS") && !arguments.trim().is_empty() {
        format!("{}\n\n{}", expanded.trim(), arguments)
            .trim()
            .to_owned()
    } else {
        expanded.trim().to_owned()
    }
}

fn model_from_slug(slug: &str) -> Result<ModelRef, ApiError> {
    let (provider, model) = slug
        .split_once('/')
        .ok_or_else(|| ApiError::invalid("Command model must use provider/model format"))?;
    if provider.is_empty() || model.is_empty() {
        return Err(ApiError::invalid(
            "Command model must use provider/model format",
        ));
    }
    Ok(ModelRef {
        provider_id: provider.to_owned(),
        model_id: model.to_owned(),
    })
}

async fn command_run(
    State(st): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Query(query): Query<ScopeQuery>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    let session = scoped_session(&st, &id, &headers, &query)?;
    let name = body
        .get("command")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| ApiError::invalid("command must be a nonempty string"))?;
    let arguments = body
        .get("arguments")
        .map(|value| {
            value
                .as_str()
                .ok_or_else(|| ApiError::invalid("arguments must be a string"))
        })
        .transpose()?
        .unwrap_or("");
    let config = Config::load_for(&session.directory).map_err(ApiError::invalid)?;
    let command = config
        .command
        .get(name)
        .ok_or_else(|| ApiError::not_found(format!("Command {name} is not configured")))?;
    let template = command
        .get("template")
        .and_then(Value::as_str)
        .or_else(|| command.as_str())
        .ok_or_else(|| ApiError::invalid(format!("Command {name} has no template")))?;
    let model = body
        .get("model")
        .or_else(|| command.get("model"))
        .map(|value| {
            value
                .as_str()
                .ok_or_else(|| ApiError::invalid("model must be a provider/model string"))
                .and_then(model_from_slug)
        })
        .transpose()?;
    let optional_string = |key: &str| -> Result<Option<String>, ApiError> {
        body.get(key)
            .or_else(|| {
                if key == "agent" {
                    command.get(key)
                } else {
                    None
                }
            })
            .map(|value| {
                value
                    .as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| ApiError::invalid(format!("{key} must be a string")))
            })
            .transpose()
    };
    let mut parts = vec![json!({"type": "text", "text": expand_command(template, arguments)})];
    if let Some(files) = body.get("parts") {
        let files = files
            .as_array()
            .ok_or_else(|| ApiError::invalid("parts must be an array"))?;
        if files
            .iter()
            .any(|part| part.get("type").and_then(Value::as_str) != Some("file"))
        {
            return Err(ApiError::invalid(
                "Command parts must contain file attachments",
            ));
        }
        parts.extend(files.iter().cloned());
    }
    let request = PromptRequest {
        model,
        parts,
        message_id: optional_string("messageID")?,
        agent: optional_string("agent")?,
        variant: optional_string("variant")?,
        command: Some(name.to_owned()),
        ..Default::default()
    };
    validate_prompt(&request)?;
    st.engine
        .run_sync(st.app.clone(), id, request)
        .await
        .map(Json)
        .map_err(ApiError::runtime)
}

async fn abort(
    State(st): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Query(query): Query<ScopeQuery>,
) -> Result<Json<bool>, ApiError> {
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
) -> Result<Json<bool>, ApiError> {
    scoped_session(&st, &id, &headers, &query)?;
    let body = body.map(|body| body.0).unwrap_or_else(|| json!({}));
    if !body.is_object() {
        return Err(ApiError::invalid("Summarize body must be an object"));
    }
    let model = match (body.get("providerID"), body.get("modelID")) {
        (None, None) => None,
        (Some(Value::String(provider)), Some(Value::String(model)))
            if !provider.is_empty() && !model.is_empty() =>
        {
            Some(ModelRef {
                provider_id: provider.clone(),
                model_id: model.clone(),
            })
        }
        _ => {
            return Err(ApiError::invalid(
                "summarize requires both providerID and modelID",
            ))
        }
    };
    let done = st
        .engine
        .summarize(st.app.clone(), id, model)
        .map_err(ApiError::runtime)?;
    let result = done
        .await
        .map_err(|_| ApiError::runtime("Compaction task ended before returning a result".into()))?;
    if let Some(error) = result.get("error") {
        return Err(ApiError::runtime(
            error
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| error.to_string()),
        ));
    }
    Ok(Json(true))
}

async fn fork(
    State(st): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Query(query): Query<ScopeQuery>,
    body: Option<Json<Value>>,
) -> Result<Json<Session>, ApiError> {
    let session = scoped_session(&st, &id, &headers, &query)?;
    if st.app.store.is_busy(&id).map_err(ApiError::runtime)? {
        return Err(ApiError::busy());
    }
    let body = body.map(|body| body.0).unwrap_or_else(|| json!({}));
    if !body.is_object() {
        return Err(ApiError::invalid("Fork body must be an object"));
    }
    let boundary = body
        .get("messageID")
        .map(|value| {
            value
                .as_str()
                .ok_or_else(|| ApiError::invalid("messageID must be a string"))
        })
        .transpose()?;
    if let Some(boundary) = boundary {
        if !st
            .app
            .store
            .messages(&id)
            .map_err(ApiError::runtime)?
            .iter()
            .any(|message| message.info.id == boundary)
        {
            return Err(ApiError::not_found("Fork boundary message not found"));
        }
    }
    let fork = st
        .app
        .store
        .try_fork(&session, boundary)
        .map_err(ApiError::runtime)?;
    bus().emit("session.created", json!({"info": fork}));
    Ok(Json(fork))
}

fn event_in_directory(event: &BusEvent, app: &App, directory: Option<&str>) -> bool {
    let Some(directory) = directory else {
        return true;
    };
    if let Some(event_directory) = event
        .properties
        .get("info")
        .and_then(|info| info.get("directory"))
        .and_then(Value::as_str)
    {
        return normalized_directory(event_directory).is_ok_and(|value| value == directory);
    }
    let session_id = event
        .properties
        .get("sessionID")
        .and_then(Value::as_str)
        .or_else(|| {
            event
                .properties
                .get("info")
                .and_then(|info| info.get("sessionID"))
                .and_then(Value::as_str)
        })
        .or_else(|| {
            event
                .properties
                .get("part")
                .and_then(|part| part.get("sessionID"))
                .and_then(Value::as_str)
        });
    session_id.is_none_or(|id| {
        app.store
            .directory_of(id)
            .is_some_and(|stored| matches_directory_str(&stored, Some(directory)))
    })
}

async fn event_stream(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ScopeQuery>,
) -> Result<Response, ApiError> {
    let directory = requested_directory(&headers, &query)?;
    let mut receiver = bus().subscribe();
    let events = stream! {
        yield Ok::<Event, std::convert::Infallible>(Event::default().data(json!({"id": hex_id("evt_"), "type": "server.connected", "properties": {}}).to_string()));
        let mut heartbeat = tokio::time::interval(std::time::Duration::from_secs(10));
        heartbeat.tick().await;
        loop {
            tokio::select! {
                _ = st.shutdown.cancelled() => break,
                event = receiver.recv() => match event {
                    Ok(event) => if event_in_directory(&event, &st.app, directory.as_deref()) { yield Ok(Event::default().data(serde_json::to_string(&*event).expect("serializable event"))); },
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(count)) => {
                        // Losing text or tool events must fail visibly instead of completing a partial turn.
                        for id in st.app.store.busy_sessions().unwrap_or_default() {
                            if st.app.store.get_session(&id).ok().flatten().is_some_and(|session| matches_directory(&session, directory.as_deref())) {
                                yield Ok(Event::default().data(json!({"id": hex_id("evt_"), "type": "session.error", "properties": {"sessionID": id, "error": {"name": "UnknownError", "data": {"message": format!("Event subscriber fell behind by {count} events; reconnect and reload the session")}}}}).to_string()));
                            }
                        }
                        break;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                },
                _ = heartbeat.tick() => { yield Ok(Event::default().data(json!({"id": hex_id("evt_"), "type": "server.heartbeat", "properties": {}}).to_string())); }
            }
        }
    };
    Ok(Sse::new(events).into_response())
}

async fn permission_list(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ScopeQuery>,
) -> Result<Json<Value>, ApiError> {
    let directory = requested_directory(&headers, &query)?;
    let requests: Vec<_> = st
        .app
        .perms
        .list()
        .into_iter()
        .filter(|request| {
            st.app
                .store
                .get_session(&request.session_id)
                .ok()
                .flatten()
                .is_some_and(|session| matches_directory(&session, directory.as_deref()))
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
) -> Result<Json<bool>, ApiError> {
    let reply = body
        .get("reply")
        .or_else(|| body.get("response"))
        .and_then(Value::as_str)
        .filter(|reply| matches!(*reply, "once" | "always" | "reject"))
        .ok_or_else(|| ApiError::invalid("reply must be once, always, or reject"))?;
    let request = st
        .app
        .perms
        .list()
        .into_iter()
        .find(|request| request.id == id)
        .ok_or_else(|| ApiError::not_found("Permission request not found"))?;
    if session_id.is_some_and(|session_id| request.session_id != session_id) {
        return Err(ApiError::not_found(
            "Permission request not found in this session",
        ));
    }
    scoped_session(st, &request.session_id, headers, query)?;
    if !st.app.perms.reply(id, reply) {
        return Err(ApiError::not_found(
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
) -> Result<Json<bool>, ApiError> {
    reply_permission(&st, &id, None, &headers, &query, &body)
}

async fn permission_reply_session(
    State(st): State<AppState>,
    Path((session_id, id)): Path<(String, String)>,
    headers: HeaderMap,
    Query(query): Query<ScopeQuery>,
    Json(body): Json<Value>,
) -> Result<Json<bool>, ApiError> {
    reply_permission(&st, &id, Some(&session_id), &headers, &query, &body)
}

async fn question_list(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ScopeQuery>,
) -> Result<Json<Value>, ApiError> {
    let directory = requested_directory(&headers, &query)?;
    let requests: Vec<_> = st
        .app
        .questions
        .list()
        .into_iter()
        .filter(|request| {
            st.app
                .store
                .get_session(&request.session_id)
                .ok()
                .flatten()
                .is_some_and(|session| matches_directory(&session, directory.as_deref()))
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
) -> Result<Json<bool>, ApiError> {
    let request = st
        .app
        .questions
        .list()
        .into_iter()
        .find(|request| request.id == id)
        .ok_or_else(|| ApiError::not_found("Question request not found"))?;
    scoped_session(&st, &request.session_id, &headers, &query)?;
    if body.answers.len() != request.questions.len() {
        return Err(ApiError::invalid(
            "answers must have one array for each question",
        ));
    }
    if body
        .answers
        .iter()
        .zip(&request.questions)
        .any(|(answers, question)| question["multiple"] != true && answers.len() > 1)
    {
        return Err(ApiError::invalid(
            "A question without multiple=true accepts at most one answer",
        ));
    }
    if !st.app.questions.reply(&id, body.answers) {
        return Err(ApiError::not_found(
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
) -> Result<Json<bool>, ApiError> {
    let request = st
        .app
        .questions
        .list()
        .into_iter()
        .find(|request| request.id == id)
        .ok_or_else(|| ApiError::not_found("Question request not found"))?;
    scoped_session(&st, &request.session_id, &headers, &query)?;
    if !st.app.questions.reject(&id) {
        return Err(ApiError::not_found(
            "Question request has already been resolved",
        ));
    }
    Ok(Json(true))
}

async fn mcp_status(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ScopeQuery>,
) -> Result<Json<Value>, ApiError> {
    let app = st
        .app
        .for_directory(&directory_for(&headers, &query)?)
        .map_err(ApiError::invalid)?;
    app.wait_mcp_ready().await;
    Ok(Json(app.mcp.status()))
}

async fn mcp_add(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ScopeQuery>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    let name = body
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
        .ok_or_else(|| ApiError::invalid("MCP name is required"))?;
    let config = body
        .get("config")
        .cloned()
        .ok_or_else(|| ApiError::invalid("MCP config is required"))?;
    let app = st
        .app
        .for_directory(&directory_for(&headers, &query)?)
        .map_err(ApiError::invalid)?;
    app.wait_mcp_ready().await;
    app.mcp
        .add_async(name, config)
        .await
        .map_err(ApiError::invalid)?;
    Ok(Json(app.mcp.status()))
}

async fn mcp_connect(
    State(st): State<AppState>,
    Path(name): Path<String>,
    headers: HeaderMap,
    Query(query): Query<ScopeQuery>,
) -> Result<Json<bool>, ApiError> {
    let app = st
        .app
        .for_directory(&directory_for(&headers, &query)?)
        .map_err(ApiError::invalid)?;
    app.wait_mcp_ready().await;
    let status = app
        .mcp
        .connect_async(&name)
        .await
        .map_err(ApiError::runtime)?;
    if status["status"] != "connected" {
        let message = status["error"]
            .as_str()
            .unwrap_or("MCP connection did not reach the connected state");
        return Err(ApiError::runtime(message.to_owned()));
    }
    Ok(Json(true))
}

async fn mcp_disconnect(
    State(st): State<AppState>,
    Path(name): Path<String>,
    headers: HeaderMap,
    Query(query): Query<ScopeQuery>,
) -> Result<Json<bool>, ApiError> {
    let app = st
        .app
        .for_directory(&directory_for(&headers, &query)?)
        .map_err(ApiError::invalid)?;
    app.wait_mcp_ready().await;
    app.mcp.disconnect(&name).map_err(ApiError::runtime)?;
    Ok(Json(true))
}

pub fn provider_inventory(config: &Config, auth: &AuthStore) -> Value {
    let mut catalog: BTreeMap<String, Map<String, Value>> = BTreeMap::new();
    for mut model in config.catalog() {
        let Some(provider) = model
            .get("providerID")
            .and_then(Value::as_str)
            .map(str::to_owned)
        else {
            continue;
        };
        let Some(id) = model
            .get("modelID")
            .and_then(Value::as_str)
            .map(str::to_owned)
        else {
            continue;
        };
        model["id"] = json!(id);
        if let Some(fields) = model.as_object_mut() {
            fields.remove("modelID");
        }
        catalog.entry(provider).or_default().insert(id, model);
    }
    let all: Vec<Value> = catalog.into_iter().map(|(id, models)| {
        let provider = config.provider.get(&id);
        let name = provider.map(|provider| provider.name.as_str()).filter(|name| !name.is_empty()).unwrap_or(&id);
        let options = provider.map(|provider| json!({"baseURL": provider.options.base_url})).unwrap_or_else(|| json!({}));
        json!({"id": id, "name": name, "env": provider.map(|provider| provider.env.clone()).unwrap_or_default(), "source": "config", "options": options, "models": models})
    }).collect();
    let mut defaults = Map::new();
    for provider in &all {
        if let (Some(id), Some(models)) = (provider["id"].as_str(), provider["models"].as_object())
        {
            let prefix = format!("{id}/");
            let configured = config
                .model
                .strip_prefix(prefix.as_str())
                .filter(|model| models.contains_key(*model));
            if let Some(model) = configured.or_else(|| models.keys().next().map(String::as_str)) {
                defaults.insert(id.to_owned(), json!(model));
            }
        }
    }
    json!({"all": all, "connected": auth.connected_for(config), "default": defaults})
}

async fn providers(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ScopeQuery>,
) -> Result<Json<Value>, ApiError> {
    let config = Config::load_for(&directory_for(&headers, &query)?).map_err(ApiError::invalid)?;
    let (config, errors) = crate::provider::discover_models(&config, &st.app.auth).await;
    if !errors.is_empty() && config.catalog().is_empty() {
        return Err(ApiError::runtime(errors.join("; ")));
    }
    for error in errors {
        eprintln!("ocmini: model discovery: {error}");
    }
    Ok(Json(provider_inventory(&config, &st.app.auth)))
}

fn agent_rules(value: Option<&Value>) -> Vec<Value> {
    let Some(value) = value else {
        return Vec::new();
    };
    if let Some(rules) = value.as_array() {
        return rules.clone();
    }
    let mut rules = Vec::new();
    if let Some(action) = value.as_str() {
        rules.push(json!({"permission": "*", "pattern": "*", "action": action}));
    }
    if let Some(permissions) = value.as_object() {
        for (permission, value) in permissions {
            if let Some(action) = value.as_str() {
                rules.push(json!({"permission": permission, "pattern": "*", "action": action}));
            }
            if let Some(patterns) = value.as_object() {
                for (pattern, action) in patterns {
                    rules.push(
                        json!({"permission": permission, "pattern": pattern, "action": action}),
                    );
                }
            }
        }
    }
    rules
}

pub fn agents_json(config: &Config) -> Value {
    let mut definitions = BTreeMap::new();
    for (name, description, mode) in [
        (
            "build",
            "Coding agent with the configured tool permissions.",
            "primary",
        ),
        (
            "plan",
            "Analyze and plan changes without editing the workspace.",
            "primary",
        ),
        (
            "general",
            "Delegate multi-step research and implementation tasks.",
            "subagent",
        ),
        (
            "explore",
            "Read and search a codebase without editing it.",
            "subagent",
        ),
    ] {
        let mut rules = vec![
            json!({"permission": "*", "pattern": "*", "action": "allow"}),
            json!({"permission": "doom_loop", "pattern": "*", "action": "ask"}),
            json!({"permission": "external_directory", "pattern": "*", "action": "ask"}),
            json!({"permission": "read", "pattern": "*.env", "action": "ask"}),
            json!({"permission": "read", "pattern": "*.env.*", "action": "ask"}),
        ];
        rules.extend(config.permission_rules().iter().map(|rule| json!(rule)));
        if name == "plan" || name == "explore" {
            rules.push(json!({"permission": "edit", "pattern": "*", "action": "deny"}));
        }
        definitions.insert(name.to_owned(), json!({"name": name, "description": description, "mode": mode, "native": true, "hidden": false, "options": {}, "permission": rules}));
    }
    for (name, definition) in &config.agent {
        if definition.get("disable").and_then(Value::as_bool) == Some(true) {
            definitions.remove(name);
            continue;
        }
        let mut agent = definitions.remove(name).unwrap_or_else(|| json!({"name": name, "mode": "all", "native": false, "hidden": false, "options": {}, "permission": config.permission_rules()}));
        if let Some(properties) = definition.as_object() {
            for (key, value) in properties {
                if key != "permission" && key != "disable" {
                    agent[key] = value.clone();
                }
            }
        }
        if let Some(rules) = agent["permission"].as_array_mut() {
            rules.extend(agent_rules(definition.get("permission")));
        }
        agent["name"] = json!(name);
        definitions.insert(name.clone(), agent);
    }
    json!(definitions.into_values().collect::<Vec<_>>())
}

async fn agents(
    headers: HeaderMap,
    Query(query): Query<ScopeQuery>,
) -> Result<Json<Value>, ApiError> {
    let config = Config::load_for(&directory_for(&headers, &query)?).map_err(ApiError::invalid)?;
    Ok(Json(agents_json(&config)))
}

pub fn skills_json_for(directory: &str, config: &Config) -> Value {
    json!(crate::tools::skill_list_with_paths(
        Some(directory),
        &config.skill_paths()
    ))
}

async fn skills(
    headers: HeaderMap,
    Query(query): Query<ScopeQuery>,
) -> Result<Json<Value>, ApiError> {
    let directory = directory_for(&headers, &query)?;
    let config = Config::load_for(&directory).map_err(ApiError::invalid)?;
    Ok(Json(skills_json_for(&directory, &config)))
}

pub fn commands_json(config: &Config) -> Value {
    let placeholder = regex::Regex::new(r"\$ARGUMENTS|\$\d+").expect("constant regex");
    let commands: BTreeMap<_, _> = config
        .command
        .iter()
        .filter(|(_, entry)| {
            entry.is_string() || entry.get("template").is_some_and(Value::is_string)
        })
        .map(|(name, entry)| {
            let mut entry = if entry.is_string() {
                json!({"template": entry})
            } else {
                entry.clone()
            };
            entry["name"] = json!(name);
            if entry.get("source").is_none() {
                entry["source"] = json!("command");
            }
            let template = entry
                .get("template")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let mut hints = Vec::new();
            for matched in placeholder.find_iter(template) {
                let hint = matched.as_str().to_owned();
                if !hints.contains(&hint) {
                    hints.push(hint);
                }
            }
            entry["hints"] = json!(hints);
            (name.clone(), entry)
        })
        .collect();
    json!(commands.into_values().collect::<Vec<_>>())
}

async fn commands(
    headers: HeaderMap,
    Query(query): Query<ScopeQuery>,
) -> Result<Json<Value>, ApiError> {
    let config = Config::load_for(&directory_for(&headers, &query)?).map_err(ApiError::invalid)?;
    Ok(Json(commands_json(&config)))
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

async fn not_found() -> ApiError {
    ApiError::not_found("Route not found")
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

pub async fn serve(hostname: String, port: u16) -> Result<(), String> {
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
        app: App::new()?,
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
    fn command_arguments_are_literal_and_last_position_collects_remainder() {
        assert_eq!(
            expand_command("First $1. Rest $2.", "\"alpha beta\" gamma delta"),
            "First alpha beta. Rest gamma delta."
        );
        assert_eq!(
            expand_command("$ARGUMENTS / $ARGUMENTS", "$& $$ $` $'"),
            "$& $$ $` $' / $& $$ $` $'"
        );
        assert_eq!(
            expand_command("Review.", "branch main"),
            "Review.\n\nbranch main"
        );
        assert_eq!(expand_command("$2", "only"), "");
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
}
