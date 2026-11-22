//! Configuration and sandbox selection for the browser interface.
//!
//! Settings live in the existing OpenCode configuration files; there is no
//! separate store. Every route does its work on request and holds nothing
//! afterwards.
use crate::{blocking, locked, Result, WebError};
use axum::{
    extract::{Path as UrlPath, Query, State},
    http::StatusCode,
    routing::get,
    Json, Router,
};
use ocmini_core::{config::Config, sessions, tools::changes, types::FailureKind};
use ocmini_server::server::AppState;
use serde::Deserialize;
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};

/// Settings the form edits. Nested keys use dots; everything else is edited
/// in the file itself.
const FIELDS: [&str; 6] = [
    "model",
    "small_model",
    "title_model",
    "sandbox",
    "compaction.auto",
    "compaction.prune",
];

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/web/api/config", get(read_config).patch(patch_config))
        .route("/web/api/sandbox", get(sandbox_choices))
        .route(
            "/web/api/session/{sid}/sandbox",
            get(session_sandbox).put(select_sandbox),
        )
        .route("/web/api/session/{sid}/status", get(session_status))
}

/// Whether a session is running, and whether this process runs it. Live
/// events only cover this process; a page watching work owned by another
/// ocmini process (for example T3's server) reads history instead.
async fn session_status(
    State(st): State<AppState>,
    UrlPath(sid): UrlPath<String>,
) -> Result<Json<Value>> {
    blocking(move || {
        if st.app.store.get_session(&sid)?.is_none() {
            return Err(WebError::new(
                StatusCode::NOT_FOUND,
                "NotFoundError",
                "Session not found",
            ));
        }
        let busy = st.app.store.is_busy(&sid)?;
        let local = st
            .app
            .statuses
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(&sid);
        Ok(Json(json!({"busy": busy, "local": local})))
    })
    .await
}

impl From<ocmini_core::types::OperationError> for WebError {
    fn from(error: ocmini_core::types::OperationError) -> Self {
        let (status, tag) = match error.kind {
            FailureKind::Invalid | FailureKind::SandboxRefused => {
                (StatusCode::BAD_REQUEST, "InvalidRequestError")
            }
            FailureKind::NotFound => (StatusCode::NOT_FOUND, "NotFoundError"),
            FailureKind::Busy => (StatusCode::CONFLICT, "SessionBusyError"),
            _ => (StatusCode::INTERNAL_SERVER_ERROR, "UnknownError"),
        };
        WebError::new(status, tag, error.message)
    }
}

#[derive(Deserialize)]
struct DirectoryQuery {
    directory: String,
}

fn directory(raw: &str) -> Result<String> {
    Ok(sessions::directory(
        &crate::absolute(raw)?.to_string_lossy(),
    )?)
}

/// The file a scope's settings are written to: an existing `opencode.json`,
/// else an existing `opencode.jsonc`, else a new `opencode.json`.
fn config_file(scope: &str, directory: &str) -> Result<PathBuf> {
    let folder = match scope {
        "global" => PathBuf::from(ocmini_core::config::config_home()).join("opencode"),
        "project" => PathBuf::from(directory),
        _ => return Err(WebError::invalid("scope must be global or project")),
    };
    let json = folder.join("opencode.json");
    let jsonc = folder.join("opencode.jsonc");
    Ok(if !json.exists() && jsonc.exists() {
        jsonc
    } else {
        json
    })
}

fn get_path<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    key.split('.')
        .try_fold(value, |value, part| value.get(part))
}

fn set_path(root: &mut Map<String, Value>, key: &str, value: Value) {
    let (head, rest) = match key.split_once('.') {
        Some((head, rest)) => (head, Some(rest)),
        None => (key, None),
    };
    match rest {
        None if value.is_null() => {
            root.remove(head);
        }
        None => {
            root.insert(head.to_owned(), value);
        }
        Some(rest) => {
            let child = root
                .entry(head.to_owned())
                .or_insert_with(|| Value::Object(Map::new()));
            if !child.is_object() {
                *child = Value::Object(Map::new());
            }
            let object = child.as_object_mut().expect("object");
            set_path(object, rest, value);
            if object.is_empty() {
                root.remove(head);
            }
        }
    }
}

/// A scope's own settings. JSONC with comments is reported as not editable
/// here: rewriting it would drop the comments.
fn scope_state(path: &Path) -> Value {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return json!({"path": path, "exists": false, "editable": true, "values": {}});
        }
        Err(error) => {
            return json!({"path": path, "exists": true, "editable": false, "error": error.to_string()});
        }
    };
    match serde_json::from_str::<Value>(&text) {
        Ok(value) if value.is_object() => {
            let values: Map<String, Value> = FIELDS
                .iter()
                .filter_map(|key| get_path(&value, key).map(|v| ((*key).to_owned(), v.clone())))
                .collect();
            json!({"path": path, "exists": true, "editable": true, "values": values})
        }
        _ => json!({"path": path, "exists": true, "editable": false, "values": {}}),
    }
}

fn snapshot(st: &AppState, directory: &str) -> Result<Value> {
    let mut files = Map::new();
    for scope in ["global", "project"] {
        files.insert(scope.into(), scope_state(&config_file(scope, directory)?));
    }
    let config = Config::load_fresh(directory);
    let (effective, error) = match &config {
        Ok(config) => (
            json!({
                "model": config.model,
                "small_model": config.small_model,
                "title_model": config.title_model,
                "compaction": {"auto": config.compaction.auto.unwrap_or(true), "prune": config.compaction.prune.unwrap_or(false)},
            }),
            Value::Null,
        ),
        Err(error) => (Value::Null, json!(error)),
    };
    let sandbox = match &config {
        Ok(config) => sessions::sandbox_choices(&st.app, config)
            .unwrap_or_else(|error| json!({"error": error.message})),
        Err(_) => Value::Null,
    };
    Ok(json!({
        "directory": directory,
        "files": files,
        "effective": effective,
        "error": error,
        "sandbox": sandbox,
    }))
}

async fn read_config(
    State(st): State<AppState>,
    Query(query): Query<DirectoryQuery>,
) -> Result<Json<Value>> {
    let directory = directory(&query.directory)?;
    blocking(move || snapshot(&st, &directory).map(Json)).await
}

#[derive(Deserialize)]
struct ConfigPatch {
    scope: String,
    directory: String,
    set: Map<String, Value>,
}

/// Sets or (with null) removes form fields in one scope's file. The merged
/// configuration is validated in memory before writing to disk.
async fn patch_config(
    State(st): State<AppState>,
    Json(body): Json<ConfigPatch>,
) -> Result<Json<Value>> {
    let directory = directory(&body.directory)?;
    if let Some(key) = body.set.keys().find(|key| !FIELDS.contains(&key.as_str())) {
        return Err(WebError::invalid(format!("{key} is not editable here")));
    }
    let path = changes::target(&config_file(&body.scope, &directory)?)?;
    let guards = locked(std::slice::from_ref(&path)).await?;
    let result = blocking(move || {
        let previous = match std::fs::read_to_string(&path) {
            Ok(text) => Some(text),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(WebError::io(&path, error)),
        };
        let mut value = match &previous {
            Some(text) => serde_json::from_str::<Value>(text).map_err(|_| {
                WebError::new(
                    StatusCode::CONFLICT,
                    "ConfigNotEditableError",
                    format!(
                        "{} uses JSONC syntax; edit it in the editor to keep its comments",
                        path.display()
                    ),
                )
            })?,
            None => json!({"$schema": "https://opencode.ai/config.json"}),
        };
        let object = value
            .as_object_mut()
            .ok_or_else(|| WebError::invalid(format!("{} is not a JSON object", path.display())))?;
        for (key, field) in body.set {
            if key == "sandbox" {
                if let Some(existing) = object.get_mut("sandbox").and_then(Value::as_object_mut) {
                    if field.is_null() {
                        existing.remove("preset");
                        if existing.is_empty() {
                            object.remove("sandbox");
                        }
                        continue;
                    }
                    if field.is_string() {
                        existing.insert("preset".into(), field);
                        continue;
                    }
                }
            }
            set_path(object, &key, field);
        }
        let text = serde_json::to_string_pretty(&value).expect("JSON") + "\n";
        let checked = Config::load_with_override(&directory, &path, &text).and_then(|config| {
            sessions::sandbox_choices(&st.app, &config)
                .map(|_| ())
                .map_err(|error| error.message)
        });
        if let Err(error) = checked {
            return Err(WebError::invalid(format!(
                "The configuration was not saved: {error}"
            )));
        }
        ocmini_core::tools::files::atomic_write(&path, text.as_bytes())?;
        snapshot(&st, &directory).map(Json)
    })
    .await;
    drop(guards);
    result
}

async fn sandbox_choices(
    State(st): State<AppState>,
    Query(query): Query<DirectoryQuery>,
) -> Result<Json<Value>> {
    let directory = directory(&query.directory)?;
    blocking(move || {
        let config = Config::load_for(&directory).map_err(WebError::invalid)?;
        Ok(Json(sessions::sandbox_choices(&st.app, &config)?))
    })
    .await
}

fn session_state(st: &AppState, sid: &str) -> Result<Value> {
    let session = st.app.store.get_session(sid)?.ok_or_else(|| {
        WebError::new(StatusCode::NOT_FOUND, "NotFoundError", "Session not found")
    })?;
    let config = Config::load_for(&session.directory).map_err(WebError::invalid)?;
    let mut value = sessions::sandbox_choices(&st.app, &config)?;
    // A session without saved settings takes the default on its first turn.
    value["preset"] = session
        .sandbox
        .as_ref()
        .and_then(|saved| saved.get("preset").cloned())
        .unwrap_or(Value::Null);
    value["root"] = json!(session.parent_id.is_none());
    Ok(value)
}

async fn session_sandbox(
    State(st): State<AppState>,
    UrlPath(sid): UrlPath<String>,
) -> Result<Json<Value>> {
    blocking(move || session_state(&st, &sid).map(Json)).await
}

#[derive(Deserialize)]
struct Selection {
    preset: String,
}

async fn select_sandbox(
    State(st): State<AppState>,
    UrlPath(sid): UrlPath<String>,
    Json(body): Json<Selection>,
) -> Result<Json<Value>> {
    blocking(move || {
        let session = st.app.store.get_session(&sid)?.ok_or_else(|| {
            WebError::new(StatusCode::NOT_FOUND, "NotFoundError", "Session not found")
        })?;
        sessions::select_sandbox(&st.app, &session, &body.preset)?;
        session_state(&st, &sid).map(Json)
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn form_fields_set_and_remove_nested_keys() {
        let mut root = Map::new();
        set_path(&mut root, "model", json!("a/b"));
        set_path(&mut root, "compaction.auto", json!(false));
        assert_eq!(
            Value::Object(root.clone()),
            json!({"model":"a/b","compaction":{"auto":false}})
        );
        assert_eq!(
            get_path(&Value::Object(root.clone()), "compaction.auto"),
            Some(&json!(false))
        );
        set_path(&mut root, "compaction.auto", Value::Null);
        set_path(&mut root, "model", Value::Null);
        assert!(
            root.is_empty(),
            "removing the last nested key drops its parent"
        );
    }

    #[test]
    fn jsonc_files_are_reported_but_not_rewritten() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("opencode.jsonc");
        std::fs::write(&path, "{\n  // keep me\n  \"model\": \"a/b\"\n}\n").unwrap();
        let state = scope_state(&path);
        assert_eq!(state["editable"], false);
        let plain = root.path().join("opencode.json");
        std::fs::write(&plain, r#"{"model":"a/b","sandbox":"off","other":1}"#).unwrap();
        let state = scope_state(&plain);
        assert_eq!(state["editable"], true);
        assert_eq!(state["values"], json!({"model":"a/b","sandbox":"off"}));
    }
}
