use super::{wire, Input};
use crate::{config::Config, engine::App, server::AppState, types::*};
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse, Response,
    },
    routing::{delete, get, post, put},
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;

pub struct Error {
    status: StatusCode,
    value: Value,
}
type Result<T> = std::result::Result<T, Error>;
impl From<OperationError> for Error {
    fn from(error: OperationError) -> Self {
        let (status, tag) = match error.kind {
            FailureKind::Invalid => (StatusCode::BAD_REQUEST, "InvalidRequestError"),
            FailureKind::NotFound => (StatusCode::NOT_FOUND, "SessionNotFoundError"),
            FailureKind::Busy => (StatusCode::CONFLICT, "SessionBusyError"),
            _ => (StatusCode::INTERNAL_SERVER_ERROR, "UnknownError"),
        };
        Self {
            status,
            value: json!({"_tag":tag,"message":error.message,"sessionID":""}),
        }
    }
}
impl IntoResponse for Error {
    fn into_response(self) -> Response {
        (self.status, Json(self.value)).into_response()
    }
}
fn invalid(message: impl Into<String>) -> Error {
    OperationError::invalid(message).into()
}
fn missing(tag: &str, sid: &str, key: &str, id: &str) -> Error {
    Error {
        status: StatusCode::NOT_FOUND,
        value: json!({"_tag":tag,"sessionID":sid,key:id,"message":format!("{tag}: {id}")}),
    }
}
fn required(st: &AppState, id: &str) -> Result<Session> {
    let s = st
        .app
        .store
        .get_session(id)?
        .ok_or_else(|| missing("SessionNotFoundError", id, "sessionID", id))?;
    st.app.v2.enable(id)?;
    Ok(s)
}
fn directory(query: &HashMap<String, String>) -> Result<String> {
    let directory = query
        .get("location[directory]")
        .or_else(|| query.get("directory"))
        .map(String::as_str)
        .unwrap_or(".");
    crate::sessions::directory(directory).map_err(Into::into)
}
fn data(value: Value) -> Json<Value> {
    Json(json!({"data":value}))
}
fn located(directory: &str, value: Value) -> Json<Value> {
    Json(json!({"location":{"directory":directory},"data":value}))
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/info", get(info))
        .route("/api/event", get(events))
        .route("/api/model", get(models))
        .route("/api/provider", get(providers))
        .route("/api/agent", get(agents))
        .route("/api/skill", get(skills))
        .route("/api/command", get(commands))
        .route("/api/session", get(list).post(create))
        .route("/api/session/active", get(active))
        .route(
            "/api/session/{sid}",
            get(session).patch(update).delete(remove),
        )
        .route("/api/session/{sid}/model", post(select_model))
        .route("/api/session/{sid}/agent", post(select_agent))
        .route("/api/session/{sid}/move", post(relocate))
        .route("/api/session/{sid}/prompt", post(prompt))
        .route("/api/session/{sid}/synthetic", post(synthetic))
        .route("/api/session/{sid}/command", post(command))
        .route("/api/session/{sid}/compact", post(compact))
        .route("/api/session/{sid}/interrupt", post(interrupt))
        .route("/api/session/{sid}/message", get(messages))
        .route("/api/session/{sid}/fork", post(fork))
        .route("/api/session/{sid}/revert/stage", post(revert_stage))
        .route("/api/session/{sid}/revert/commit", post(revert_commit))
        .route("/api/session/{sid}/revert", delete(revert_clear))
        .route("/api/session/{sid}/inbox", get(inbox))
        .route("/api/session/{sid}/inbox/{id}", delete(cancel_input))
        .route("/api/session/{sid}/permission", get(permissions))
        .route(
            "/api/session/{sid}/permission/{id}/reply",
            post(permission_reply),
        )
        .route("/api/session/{sid}/form", get(forms))
        .route("/api/session/{sid}/form/{id}", delete(form_cancel))
        .route("/api/session/{sid}/form/{id}/reply", post(form_reply))
        .route(
            "/api/experimental/session/{sid}/instructions/entries",
            get(instructions),
        )
        .route(
            "/api/experimental/session/{sid}/instructions/entries/{key}",
            put(instruction_put).delete(instruction_remove),
        )
        .route(
            "/api/experimental/mcp/{name}",
            put(mcp_add).delete(mcp_remove),
        )
}
async fn info() -> Json<Value> {
    Json(
        json!({"version":crate::OPENCODE_COMPAT_VERSION,"pid":std::process::id(),"urls":[],"paths":{"tmp":std::env::temp_dir()},"capabilities":{"persistentPty":false}}),
    )
}
async fn events(
    State(st): State<AppState>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let mut rx = st.app.v2.subscribe();
    let types: Option<Vec<_>> = query
        .get("types")
        .map(|v| v.split(',').map(str::to_owned).collect());
    let stream = async_stream::stream! {
        yield Ok::<Event,std::io::Error>(Event::default().data(json!({"id":hex_id("evt_"),"created":now_ms(),"type":"server.connected","data":{}}).to_string()));
        loop {
            tokio::select! {
                _=st.shutdown.cancelled()=>break,
                value=rx.recv()=>match value {
                    Ok(Ok(frame))=>{
                        let selected=types.as_ref().is_none_or(|types|serde_json::from_str::<Value>(&frame).ok().is_some_and(|v|types.iter().any(|t|v["type"]==t.as_str())));
                        if selected {yield Ok(Event::default().data(frame.as_ref()));}
                    },
                    Ok(Err(error))=>{yield Err(std::io::Error::other(error.to_string()));break;},
                    Err(error)=>{yield Err(std::io::Error::other(error.to_string()));break;}
                }
            }
        }
    };
    Sse::new(stream)
        .keep_alive(KeepAlive::new().interval(std::time::Duration::from_secs(15)))
        .into_response()
}
async fn catalog(st: &AppState, dir: &str) -> Result<Config> {
    let config = Config::load_for(dir).map_err(OperationError::invalid)?;
    let (config, errors) = crate::provider::discover_models(&config, &st.app.auth).await;
    if config.catalog().is_empty() && !errors.is_empty() {
        return Err(OperationError::from(errors.join("; ")).into());
    }
    Ok(config)
}
async fn models(
    State(st): State<AppState>,
    Query(query): Query<HashMap<String, String>>,
) -> Result<Json<Value>> {
    let dir = directory(&query)?;
    let cfg = catalog(&st, &dir).await?;
    let connected = st.app.auth.connected_for(&cfg);
    let models:Vec<_>=cfg.catalog().into_iter().filter(|m|connected.iter().any(|p|m["providerID"]==p.as_str())).map(|m|{
        let variants:Vec<_>=m["variants"].as_object().into_iter().flatten().map(|(id,_)|json!({"id":id})).collect();
        let mut model=json!({"id":m["modelID"],"modelID":m["modelID"],"providerID":m["providerID"],"name":m["name"],"variants":variants,"time":{"released":m["time"]["released"].as_i64().unwrap_or(0)},"cost":[],"status":"active","enabled":true,"capabilities":{"tools":m["capabilities"]["tools"].as_bool().unwrap_or(true),"input":m["modalities"]["input"].as_array().cloned().unwrap_or_else(||vec![json!("text")]),"output":["text"]},"limit":{"context":m["limit"]["context"].as_i64().unwrap_or(0),"output":m["limit"]["output"].as_i64().unwrap_or(0)}});
        if let Some(input)=m["limit"].get("input"){model["limit"]["input"]=input.clone();}
        for key in ["family","type"]{if let Some(v)=m.get(key){model[key]=v.clone();}}
        model
    }).collect();
    Ok(located(&dir, json!(models)))
}
async fn providers(
    State(st): State<AppState>,
    Query(query): Query<HashMap<String, String>>,
) -> Result<Json<Value>> {
    let dir = directory(&query)?;
    let cfg = catalog(&st, &dir).await?;
    let list:Vec<_>=cfg.provider.iter().filter(|(id,_)|cfg.provider_enabled(id)).map(|(id,p)|json!({"id":id,"name":p.name,"package":p.npm,"env":p.env,"models":{},"enabled":true,"activation":"auto","settings":{},"headers":{},"body":{}})).collect();
    Ok(located(&dir, json!(list)))
}
async fn agents(Query(query): Query<HashMap<String, String>>) -> Result<Json<Value>> {
    let dir = directory(&query)?;
    let cfg = Config::load_for(&dir).map_err(OperationError::invalid)?;
    let list:Vec<_>=crate::inventory::agents_json(&cfg).as_array().into_iter().flatten().map(|a|json!({"id":a["name"],"name":a["name"],"mode":a["mode"],"hidden":a["hidden"].as_bool().unwrap_or(false),"description":a["description"].as_str().unwrap_or_default(),"request":{"settings":{},"headers":{},"body":{}},"permissions":wire::rules_out(&cfg.agent_permission_rules(a["name"].as_str().unwrap_or_default()))})).collect();
    Ok(located(&dir, json!(list)))
}
async fn skills(Query(query): Query<HashMap<String, String>>) -> Result<Json<Value>> {
    let dir = directory(&query)?;
    let cfg = Config::load_for(&dir).map_err(OperationError::invalid)?;
    let mut list = Vec::new();
    for skill in crate::inventory::skills_json_for(&dir, &cfg)
        .as_array()
        .into_iter()
        .flatten()
    {
        let path = skill["location"]
            .as_str()
            .ok_or_else(|| invalid("Skill location missing"))?;
        let content = tokio::fs::read_to_string(path)
            .await
            .map_err(|e| OperationError::from(e.to_string()))?;
        list.push(json!({"id":skill["name"],"name":skill["name"],"description":skill["description"].as_str().unwrap_or_default(),"path":path,"content":content}));
    }
    Ok(located(&dir, json!(list)))
}
async fn commands(Query(query): Query<HashMap<String, String>>) -> Result<Json<Value>> {
    let dir = directory(&query)?;
    let cfg = Config::load_for(&dir).map_err(OperationError::invalid)?;
    Ok(located(&dir, crate::inventory::commands_json(&cfg)))
}
async fn list(
    State(st): State<AppState>,
    Query(query): Query<HashMap<String, String>>,
) -> Result<Json<Value>> {
    let dir = query.get("directory").map(String::as_str);
    let sessions = st.app.store.list_sessions(dir)?;
    let list = sessions
        .into_iter()
        .map(|s| Ok(wire::session(&s, &st.app.store.v2_state(&s.id)?)))
        .collect::<Result<Vec<_>>>()?;
    Ok(Json(json!({"data":list,"cursor":{}})))
}
async fn create(State(st): State<AppState>, Json(body): Json<Value>) -> Result<Json<Value>> {
    if !body.is_object() {
        return Err(invalid("Session body must be an object"));
    }
    let parent = body
        .get("parentID")
        .map(|v| {
            v.as_str()
                .ok_or_else(|| invalid("parentID must be a string"))
        })
        .transpose()?;
    let dir = if let Some(parent) = parent {
        required(&st, parent)?.directory
    } else {
        crate::sessions::directory(body["location"]["directory"].as_str().unwrap_or("."))?
    };
    let mut s = Session::new(body["title"].as_str().unwrap_or("New session"), &dir);
    s.parent_id = parent.map(str::to_owned);
    if let Some(id) = body.get("id") {
        let id = id
            .as_str()
            .filter(|s| s.starts_with("ses_"))
            .ok_or_else(|| invalid("Invalid session id"))?;
        if let Some(existing) = st.app.store.get_session(id)? {
            st.app.v2.enable(id)?;
            return Ok(data(wire::session(&existing, &st.app.store.v2_state(id)?)));
        }
        s.id = id.into();
    }
    if body.get("title").is_some() {
        s.title_source = Some(TitleSource::Explicit);
    }
    s.permission = body.get("permissions").map(wire::rules).transpose()?;
    if let Some(model) = body.get("model") {
        wire::model_in(model)?;
    }
    let s = crate::sessions::create(&st.app, s, body.get("metadata")).await?;
    st.app.v2.enable(&s.id)?;
    let mut state = json!({"agent":body["agent"].as_str().unwrap_or("build")});
    if let Some(model) = body.get("model") {
        state["model"] = model.clone();
    }
    let state = st.app.store.v2_update(&s.id, &state)?;
    let mut event = json!({"sessionID":s.id,"projectID":s.project_id,"slug":s.slug,"version":crate::OPENCODE_COMPAT_VERSION,"location":{"directory":s.directory},"title":s.title,"agent":state["agent"]});
    if let Some(model) = state.get("model") {
        event["model"] = model.clone();
    }
    if let Some(parent) = &s.parent_id {
        event["parentID"] = json!(parent);
    }
    // Child creation is already announced by the shared session event bridge.
    if s.parent_id.is_none() {
        st.app.v2.emit(&s.id, "session.created", event, true);
    }
    Ok(data(wire::session(&s, &state)))
}
async fn session(State(st): State<AppState>, Path(sid): Path<String>) -> Result<Json<Value>> {
    let s = required(&st, &sid)?;
    Ok(data(wire::session(&s, &st.app.store.v2_state(&sid)?)))
}
async fn active(State(st): State<AppState>) -> Json<Value> {
    let statuses = st.app.statuses.lock().unwrap();
    let active: serde_json::Map<_, _> = statuses
        .keys()
        .map(|s| (s.clone(), json!({"type":"running"})))
        .collect();
    data(json!(active))
}
async fn update(
    State(st): State<AppState>,
    Path(sid): Path<String>,
    Json(body): Json<Value>,
) -> Result<StatusCode> {
    if !body.is_object() {
        return Err(invalid("Session patch must be an object"));
    }
    let s = required(&st, &sid)?;
    let mut patch = json!({});
    for key in ["title", "metadata"] {
        if let Some(v) = body.get(key) {
            patch[key] = v.clone();
        }
    }
    if let Some(rules) = body.get("permissions") {
        patch["permission"] = json!(wire::rules(rules)?);
    }
    crate::sessions::update(&st.app, &s, patch).await?;
    if let Some(rules) = body.get("permissions") {
        st.app.v2.emit(
            &sid,
            "session.permissions",
            json!({"sessionID":sid,"permissions":rules}),
            true,
        );
    }
    Ok(StatusCode::NO_CONTENT)
}
async fn remove(State(st): State<AppState>, Path(sid): Path<String>) -> Result<StatusCode> {
    let session = required(&st, &sid)?;
    let mut ids = vec![sid];
    let mut pos = 0;
    while pos < ids.len() {
        ids.extend(st.app.store.children(&ids[pos])?.into_iter().map(|c| c.id));
        pos += 1;
    }
    for id in &ids {
        st.engine.abort(id);
    }
    for id in ids.into_iter().rev() {
        // Runtime cancellation owns tool cleanup; do not erase active effects.
        st.engine.wait_idle(&id).await;
        for pending in st.app.store.v2_inbox(&id)? {
            if let Some(mid) = pending["id"].as_str() {
                st.app.store.v2_cancel_input(&id, mid)?;
            }
        }
    }
    crate::sessions::delete(&st.app, &session).await?;
    Ok(StatusCode::NO_CONTENT)
}
async fn select_model(
    State(st): State<AppState>,
    Path(sid): Path<String>,
    Json(body): Json<Value>,
) -> Result<StatusCode> {
    required(&st, &sid)?;
    wire::model_in(&body["model"])?;
    st.app
        .store
        .v2_update(&sid, &json!({"model":body["model"]}))?;
    st.app
        .store
        .v2_control(&sid, json!({"type":"model-switched","model":body["model"]}))?;
    st.app.v2.emit(
        &sid,
        "session.model.selected",
        json!({"sessionID":sid,"model":body["model"]}),
        true,
    );
    Ok(StatusCode::NO_CONTENT)
}
async fn select_agent(
    State(st): State<AppState>,
    Path(sid): Path<String>,
    Json(body): Json<Value>,
) -> Result<StatusCode> {
    let s = required(&st, &sid)?;
    let agent = wire::string(&body, "agent")?;
    let cfg = Config::load_for(&s.directory).map_err(OperationError::invalid)?;
    if !["build", "plan", "general", "explore"].contains(&agent) && !cfg.agent.contains_key(agent) {
        return Err(invalid("Unknown agent"));
    }
    st.app.store.v2_update(&sid, &json!({"agent":agent}))?;
    st.app
        .store
        .v2_control(&sid, json!({"type":"agent-switched","agent":agent}))?;
    st.app.v2.emit(
        &sid,
        "session.agent.selected",
        json!({"sessionID":sid,"agent":agent}),
        true,
    );
    Ok(StatusCode::NO_CONTENT)
}
fn input(body: &Value, kind: &str, payload: Value) -> Result<Input> {
    let delivery = body
        .get("delivery")
        .map(|v| {
            v.as_str()
                .filter(|s| matches!(*s, "steer" | "queue"))
                .ok_or_else(|| invalid("delivery must be steer or queue"))
        })
        .transpose()?
        .unwrap_or("steer");
    let resume = body
        .get("resume")
        .map(|v| v.as_bool().ok_or_else(|| invalid("resume must be boolean")))
        .transpose()?
        .unwrap_or(true);
    Ok(Input {
        delivery: delivery.into(),
        resume,
        kind: kind.into(),
        payload,
    })
}
async fn submit(st: &AppState, sid: &str, body: &Value, kind: &str) -> Result<Json<Value>> {
    let s = required(st, sid)?;
    let state = st.app.store.v2_state(sid)?;
    if let Some(boundary) = state["revert"]["messageID"].as_str() {
        st.app.store.v2_revert(sid, Some(boundary))?;
    }
    if let Some(id) = body["id"].as_str() {
        if let Some(existing) = st.app.store.message(sid, id)? {
            if existing.info.role != "user"
                || existing
                    .info
                    .extra
                    .get("ocminiV2Input")
                    .is_some_and(|v| v["kind"] != kind)
            {
                return Err(Error {
                    status: StatusCode::CONFLICT,
                    value: json!({"_tag":"ConflictError","message":"Inbox id belongs to another message type"}),
                });
            }
            return Ok(data(wire::receipt(sid, &existing)?));
        }
    }
    let text = wire::string(body, "text")?;
    let mut parts = vec![json!({"type":"text","text":text})];
    if let Some(files) = body.get("files") {
        for file in files
            .as_array()
            .ok_or_else(|| invalid("files must be an array"))?
        {
            let uri = wire::string(file, "uri")?;
            let name = file["name"].as_str().unwrap_or("attachment");
            let mut part = if uri.starts_with("file:") {
                let path = reqwest::Url::parse(uri)
                    .map_err(|_| invalid("Invalid attachment URI"))?
                    .to_file_path()
                    .map_err(|_| invalid("Invalid local file URI"))?;
                crate::blocking::run(|| crate::attachments::from_file(&path))
                    .map_err(OperationError::invalid)?
                    .part()
            } else {
                let mime = uri
                    .strip_prefix("data:")
                    .and_then(|s| s.split_once(';'))
                    .map(|s| s.0)
                    .unwrap_or("application/octet-stream");
                json!({"type":"file","url":uri,"filename":name,"mime":mime})
            };
            part["filename"] = json!(name);
            parts.push(part);
        }
    }
    if let Some(skills) = body.get("skills") {
        let cfg = Config::load_for(&s.directory).map_err(OperationError::invalid)?;
        let available = crate::inventory::skills_json_for(&s.directory, &cfg);
        for skill in skills
            .as_array()
            .ok_or_else(|| invalid("skills must be an array"))?
        {
            let id = wire::string(skill, "id")?;
            let selected = available
                .as_array()
                .into_iter()
                .flatten()
                .find(|v| v["name"] == id)
                .ok_or_else(|| missing("SkillNotFoundError", sid, "skillID", id))?;
            let content = tokio::fs::read_to_string(
                selected["location"]
                    .as_str()
                    .ok_or_else(|| invalid("Skill path missing"))?,
            )
            .await
            .map_err(|e| OperationError::from(e.to_string()))?;
            parts.push(json!({"type":"text","text":content,"synthetic":true}));
        }
    }
    let id = body
        .get("id")
        .map(|v| v.as_str().ok_or_else(|| invalid("id must be a string")))
        .transpose()?
        .map(str::to_owned)
        .unwrap_or_else(|| hex_id("msg_"));
    let request = PromptRequest {
        message_id: Some(id.clone()),
        parts,
        v2: Some(input(
            body,
            kind,
            json!({"text":text,"metadata":body.get("metadata").cloned().unwrap_or_else(||json!({}))}),
        )?),
        ..Default::default()
    };
    let receiver = st
        .engine
        .admit(st.app.clone(), sid.into(), request.clone())
        .await?;
    drop(receiver);
    let message = st
        .app
        .store
        .message(sid, &id)?
        .ok_or_else(|| invalid("Prompt receipt missing"))?;
    Ok(data(wire::receipt(sid, &message)?))
}
async fn prompt(
    State(st): State<AppState>,
    Path(sid): Path<String>,
    Json(body): Json<Value>,
) -> Result<Json<Value>> {
    submit(&st, &sid, &body, "user").await
}
async fn synthetic(
    State(st): State<AppState>,
    Path(sid): Path<String>,
    Json(body): Json<Value>,
) -> Result<Json<Value>> {
    submit(&st, &sid, &body, "synthetic").await
}
async fn command(
    State(st): State<AppState>,
    Path(sid): Path<String>,
    Json(body): Json<Value>,
) -> Result<StatusCode> {
    let s = required(&st, &sid)?;
    let cfg = Config::load_for(&s.directory).map_err(OperationError::invalid)?;
    let name = wire::string(&body, "name")?;
    let request = crate::commands::prepare(
        &cfg,
        name,
        wire::string(&body, "text")?,
        PromptRequest {
            v2: Some(input(&body, "user", json!({"text":body["text"]}))?),
            ..Default::default()
        },
        None,
    )
    .map_err(OperationError::invalid)?;
    drop(st.engine.admit(st.app.clone(), sid, request).await?);
    Ok(StatusCode::NO_CONTENT)
}
async fn control(
    st: &AppState,
    sid: &str,
    body: &Value,
    kind: &str,
    payload: Value,
) -> Result<Json<Value>> {
    required(st, sid)?;
    let request = PromptRequest {
        message_id: Some(
            body["id"]
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| hex_id("msg_")),
        ),
        parts: vec![json!({"type":"compaction","auto":false})],
        command: Some("__ocmini_compact".into()),
        v2: Some(input(body, kind, payload)?),
        ..Default::default()
    };
    drop(
        st.engine
            .admit(st.app.clone(), sid.into(), request.clone())
            .await?,
    );
    let m = st
        .app
        .store
        .message(sid, request.message_id.as_deref().unwrap_or_default())?
        .ok_or_else(|| invalid("Control receipt missing"))?;
    Ok(data(wire::inbox(sid, &request, m.info.time.created)))
}
async fn compact(
    State(st): State<AppState>,
    Path(sid): Path<String>,
    body: Option<Json<Value>>,
) -> Result<Json<Value>> {
    control(
        &st,
        &sid,
        &body.map(|b| b.0).unwrap_or_else(|| json!({})),
        "compaction",
        json!({}),
    )
    .await
}
async fn relocate(
    State(st): State<AppState>,
    Path(sid): Path<String>,
    Json(body): Json<Value>,
) -> Result<StatusCode> {
    let dir = crate::sessions::directory(wire::string(&body, "directory")?)?;
    let _ = control(
        &st,
        &sid,
        &body,
        "move",
        json!({"location":{"directory":dir},"projectID":"global"}),
    )
    .await?;
    Ok(StatusCode::NO_CONTENT)
}
async fn interrupt(State(st): State<AppState>, Path(sid): Path<String>) -> Result<Json<Value>> {
    required(&st, &sid)?;
    Ok(Json(json!({"interrupted":st.engine.interrupt(&sid)})))
}
async fn inbox(State(st): State<AppState>, Path(sid): Path<String>) -> Result<Json<Value>> {
    required(&st, &sid)?;
    Ok(data(json!(st.app.store.v2_inbox(&sid)?)))
}
async fn cancel_input(
    State(st): State<AppState>,
    Path((sid, id)): Path<(String, String)>,
) -> Result<StatusCode> {
    required(&st, &sid)?;
    st.app.store.v2_cancel_input(&sid, &id)?;
    st.app.v2.emit(
        &sid,
        "session.inbox.cancelled",
        json!({"sessionID":sid,"inboxID":id}),
        true,
    );
    Ok(StatusCode::NO_CONTENT)
}
#[derive(Deserialize, serde::Serialize)]
struct Cursor {
    session: String,
    after: i64,
    descending: bool,
}
async fn messages(
    State(st): State<AppState>,
    Path(sid): Path<String>,
    Query(query): Query<HashMap<String, String>>,
) -> Result<Json<Value>> {
    use base64::Engine;
    required(&st, &sid)?;
    let mut descending = query.get("order").is_some_and(|v| v == "desc");
    let mut after = None;
    if let Some(cursor) = query.get("cursor") {
        let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(cursor)
            .map_err(|_| invalid("Invalid message cursor"))?;
        let cursor: Cursor =
            serde_json::from_slice(&bytes).map_err(|_| invalid("Invalid message cursor"))?;
        if cursor.session != sid {
            return Err(invalid("Cursor belongs to another session"));
        }
        after = Some(cursor.after);
        descending = cursor.descending;
    }
    let limit = query
        .get("limit")
        .map(|v| {
            v.parse::<usize>()
                .ok()
                .filter(|v| *v > 0)
                .ok_or_else(|| invalid("Invalid limit"))
        })
        .transpose()?
        .unwrap_or(50);
    let rows = st.app.store.v2_page(&sid, after, descending, limit)?;
    let last = rows.last().map(|r| r.0);
    let len = rows.len();
    let values: Vec<_> = rows
        .into_iter()
        .filter_map(|(_, m)| wire::message(&m))
        .collect();
    let next = if len == limit {
        last.map(|after| {
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(
                serde_json::to_vec(&Cursor {
                    session: sid,
                    after,
                    descending,
                })
                .expect("cursor JSON"),
            )
        })
    } else {
        None
    };
    let mut cursor = json!({});
    if let Some(next) = next {
        cursor["next"] = json!(next);
    }
    Ok(Json(json!({"data":values,"cursor":cursor})))
}
async fn fork(
    State(st): State<AppState>,
    Path(sid): Path<String>,
    body: Option<Json<Value>>,
) -> Result<Json<Value>> {
    let s = required(&st, &sid)?;
    let body = body.map(|b| b.0).unwrap_or_else(|| json!({}));
    let mut fork = crate::sessions::fork(&st.app, &s, body["before"].as_str()).await?;
    fork.cost = 0.0;
    fork.tokens = Tokens::default();
    st.app.store.update_session(&fork)?;
    st.app.v2.enable(&fork.id)?;
    let mut state = st.app.store.v2_state(&sid)?;
    state.as_object_mut().unwrap().remove("revert");
    state.as_object_mut().unwrap().remove("outcome");
    state.as_object_mut().unwrap().remove("busy");
    state.as_object_mut().unwrap().remove("idle");
    state["fork"] = json!({"sessionID":sid,"boundary":{"type":if body["before"].is_string(){"before"}else{"through"},"messageID":body["before"].as_str().map(str::to_owned).or(st.app.store.messages_limited(&sid,Some(1))?.last().map(|m|m.info.id.clone())).unwrap_or_default()}});
    st.app.store.v2_update(&fork.id, &state)?;
    st.app.v2.emit(
        &fork.id,
        "session.forked",
        json!({"sessionID":fork.id,"parentID":sid,"boundary":state["fork"]["boundary"]}),
        true,
    );
    Ok(data(wire::session(&fork, &state)))
}
async fn revert_stage(
    State(st): State<AppState>,
    Path(sid): Path<String>,
    Json(body): Json<Value>,
) -> Result<Json<Value>> {
    required(&st, &sid)?;
    if st.app.store.is_busy(&sid)? {
        return Err(OperationError::busy("Session is working").into());
    }
    if body["files"] != false {
        return Err(invalid(
            "File-restoring revert is not supported; set files:false",
        ));
    }
    let id = wire::string(&body, "messageID")?;
    if st.app.store.message(&sid, id)?.is_none() {
        return Err(missing("MessageNotFoundError", &sid, "messageID", id));
    }
    let revert = json!({"messageID":id,"files":[]});
    st.app.store.v2_update(&sid, &json!({"revert":revert}))?;
    st.app.v2.emit(
        &sid,
        "session.revert.staged",
        json!({"sessionID":sid,"revert":revert}),
        true,
    );
    Ok(data(revert))
}
async fn revert_commit(State(st): State<AppState>, Path(sid): Path<String>) -> Result<StatusCode> {
    required(&st, &sid)?;
    if st.app.store.is_busy(&sid)? {
        return Err(OperationError::busy("Session is working").into());
    }
    let state = st.app.store.v2_state(&sid)?;
    let boundary = state["revert"]["messageID"].as_str();
    st.app.store.v2_revert(&sid, boundary)?;
    if let Some(to) = boundary {
        st.app.v2.emit(
            &sid,
            "session.revert.committed",
            json!({"sessionID":sid,"to":to}),
            true,
        );
    }
    Ok(StatusCode::NO_CONTENT)
}
async fn revert_clear(State(st): State<AppState>, Path(sid): Path<String>) -> Result<StatusCode> {
    let session = required(&st, &sid)?;
    if st.app.store.is_busy(&sid)? {
        return Err(OperationError::busy("Session is working").into());
    }
    st.app.store.v2_revert(&sid, None)?;
    st.app.v2.emit(
        &sid,
        "session.revert.cleared",
        json!({"sessionID":sid}),
        true,
    );
    // Upstream wakes execution even when there was no staged revert.
    st.engine
        .resume(st.app.for_directory(&session.directory)?, sid)?;
    Ok(StatusCode::NO_CONTENT)
}
async fn permissions(State(st): State<AppState>, Path(sid): Path<String>) -> Result<Json<Value>> {
    required(&st, &sid)?;
    Ok(data(json!(st
        .app
        .perms
        .list()
        .iter()
        .filter(|p| p.session_id == sid)
        .map(wire::permission)
        .collect::<Vec<_>>())))
}
async fn permission_reply(
    State(st): State<AppState>,
    Path((sid, id)): Path<(String, String)>,
    Json(body): Json<Value>,
) -> Result<StatusCode> {
    required(&st, &sid)?;
    let request = st
        .app
        .perms
        .list()
        .into_iter()
        .find(|p| p.id == id && p.session_id == sid)
        .ok_or_else(|| missing("PermissionNotFoundError", &sid, "requestID", &id))?;
    let decision = wire::string(&body, "decision")?;
    if !matches!(decision, "once" | "always" | "reject") {
        return Err(invalid("Invalid decision"));
    }
    if !st
        .app
        .perms
        .reply_with_message(&request.id, decision, body["message"].as_str())
    {
        return Err(missing("PermissionNotFoundError", &sid, "requestID", &id));
    }
    if decision == "reject" && !body["message"].is_string() {
        st.engine.interrupt(&sid);
    }
    Ok(StatusCode::NO_CONTENT)
}
async fn forms(State(st): State<AppState>, Path(sid): Path<String>) -> Result<Json<Value>> {
    required(&st, &sid)?;
    Ok(data(json!(st
        .app
        .questions
        .list()
        .iter()
        .filter(|p| p.session_id == sid)
        .map(wire::form)
        .collect::<Vec<_>>())))
}
fn question(st: &AppState, sid: &str, id: &str) -> Result<QuestionRequest> {
    required(st, sid)?;
    let legacy = format!("que_{}", id.trim_start_matches("frm_"));
    st.app
        .questions
        .list()
        .into_iter()
        .find(|p| p.session_id == sid && p.id == legacy)
        .ok_or_else(|| missing("FormNotFoundError", sid, "formID", id))
}
async fn form_reply(
    State(st): State<AppState>,
    Path((sid, id)): Path<(String, String)>,
    Json(body): Json<Value>,
) -> Result<StatusCode> {
    let q = question(&st, &sid, &id)?;
    let answer = body["answer"]
        .as_object()
        .ok_or_else(|| invalid("answer must be an object"))?;
    let answers = q
        .questions
        .iter()
        .enumerate()
        .map(|(n, _)| {
            let v = answer
                .get(&n.to_string())
                .ok_or_else(|| invalid("Missing form field"))?;
            if let Some(s) = v.as_str() {
                Ok(vec![s.into()])
            } else {
                v.as_array()
                    .ok_or_else(|| invalid("Invalid answer"))?
                    .iter()
                    .map(|v| {
                        v.as_str()
                            .map(str::to_owned)
                            .ok_or_else(|| invalid("Invalid answer"))
                    })
                    .collect()
            }
        })
        .collect::<Result<Vec<Vec<String>>>>()?;
    q.validate_answers(&answers)
        .map_err(OperationError::invalid)?;
    if !st.app.questions.reply(&q.id, answers) {
        return Err(missing("FormAlreadySettledError", &sid, "formID", &id));
    }
    Ok(StatusCode::NO_CONTENT)
}
async fn form_cancel(
    State(st): State<AppState>,
    Path((sid, id)): Path<(String, String)>,
) -> Result<StatusCode> {
    let q = question(&st, &sid, &id)?;
    st.app.questions.reject(&q.id);
    Ok(StatusCode::NO_CONTENT)
}
async fn instructions(State(st): State<AppState>, Path(sid): Path<String>) -> Result<Json<Value>> {
    required(&st, &sid)?;
    let state = st.app.store.v2_state(&sid)?;
    Ok(data(json!(state["instructions"]
        .as_object()
        .into_iter()
        .flatten()
        .map(|(key, value)| json!({"key":key,"value":value}))
        .collect::<Vec<_>>())))
}
async fn instruction_put(
    State(st): State<AppState>,
    Path((sid, key)): Path<(String, String)>,
    Json(body): Json<Value>,
) -> Result<StatusCode> {
    required(&st, &sid)?;
    if key.is_empty()
        || !key
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b))
    {
        return Err(invalid("Invalid instruction key"));
    }
    let value = body
        .get("value")
        .ok_or_else(|| invalid("value is required"))?;
    st.app.store.v2_instruction(&sid, &key, Some(value))?;
    Ok(StatusCode::NO_CONTENT)
}
async fn instruction_remove(
    State(st): State<AppState>,
    Path((sid, key)): Path<(String, String)>,
) -> Result<StatusCode> {
    required(&st, &sid)?;
    st.app.store.v2_instruction(&sid, &key, None)?;
    Ok(StatusCode::NO_CONTENT)
}
async fn mcp(st: &AppState, query: &HashMap<String, String>) -> Result<App> {
    let app = st.app.for_directory(&directory(query)?)?;
    app.wait_mcp_ready().await;
    Ok(app)
}
async fn mcp_add(
    State(st): State<AppState>,
    Path(name): Path<String>,
    Query(query): Query<HashMap<String, String>>,
    Json(body): Json<Value>,
) -> Result<StatusCode> {
    let app = mcp(&st, &query).await?;
    let status = app
        .mcp()
        .add_async(
            &name,
            body.get("config")
                .cloned()
                .ok_or_else(|| invalid("config is required"))?,
        )
        .await?;
    if status["status"] != "connected" {
        return Err(OperationError::from(
            status["error"].as_str().unwrap_or("MCP connection failed"),
        )
        .into());
    }
    Ok(StatusCode::NO_CONTENT)
}
async fn mcp_remove(
    State(st): State<AppState>,
    Path(name): Path<String>,
    Query(query): Query<HashMap<String, String>>,
) -> Result<StatusCode> {
    mcp(&st, &query).await?.mcp().remove(&name)?;
    Ok(StatusCode::NO_CONTENT)
}
