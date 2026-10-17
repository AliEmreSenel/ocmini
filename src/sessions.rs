use crate::store::Store;
use crate::types::Session;
use std::path::Path;

pub fn directory(path: &str) -> Result<String, String> {
    let path = Path::new(path)
        .canonicalize()
        .map_err(|error| format!("Cannot open directory {path}: {error}"))?;
    if !path.is_dir() {
        return Err(format!("Not a directory: {}", path.display()));
    }
    Ok(path.to_string_lossy().into_owned())
}

pub fn open(
    store: &Store,
    path: Option<&str>,
    id: Option<&str>,
    continue_last: bool,
) -> Result<Session, String> {
    let path = path.map(directory).transpose()?;
    let session = if let Some(id) = id {
        let session = store
            .get_session(id)?
            .ok_or_else(|| format!("Session not found: {id}"))?;
        if let Some(path) = &path {
            if *path != directory(&session.directory)? {
                return Err(format!("Session {id} belongs to {}", session.directory));
            }
        }
        session
    } else {
        let path = path.map_or_else(|| directory("."), Ok)?;
        if continue_last {
            store
                .list_sessions(Some(&path))?
                .into_iter()
                .find(|session| session.parent_id.is_none())
                .ok_or_else(|| format!("No saved sessions in {path}"))?
        } else {
            store.try_create_session_owned("tui session", &path)?
        }
    };
    if store.is_busy(&session.id)? {
        return Err(format!("Session {} is already busy", session.id));
    }
    // Verify the saved project still exists before entering terminal raw mode.
    directory(&session.directory)?;
    Ok(session)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{MessageInfo, MessageWithParts};

    #[test]
    fn continue_selects_latest_parent_in_the_current_project() {
        let project = tempfile::tempdir().unwrap();
        let path = directory(project.path().to_str().unwrap()).unwrap();
        let store = Store::open(Path::new(":memory:"), "test").unwrap();
        let older = store.create_session_owned("older", &path);
        let mut latest = store.create_session_owned("latest", &path);
        latest.time.updated = Some(older.time.created + 10);
        store.update_session(&latest).unwrap();
        let mut child = store.create_session_owned("child", &path);
        child.parent_id = Some(latest.id.clone());
        child.time.updated = Some(older.time.created + 20);
        store.update_session(&child).unwrap();
        store.create_session_owned("another project", "/elsewhere");
        assert_eq!(open(&store, Some(&path), None, true).unwrap().id, latest.id);
        assert_ne!(
            open(&store, Some(&path), None, false).unwrap().id,
            latest.id
        );
    }

    #[test]
    fn resume_keeps_the_saved_directory_and_transcript_across_restart() {
        let project = tempfile::tempdir().unwrap();
        let path = directory(project.path().to_str().unwrap()).unwrap();
        let database = project.path().join("sessions.db");
        let store = Store::open(&database, "first").unwrap();
        let session = open(&store, Some(&path), None, false).unwrap();
        store.add_message(&MessageWithParts {
            info: MessageInfo {
                id: "previous".into(), session_id: session.id.clone(), role: "user".into(),
                ..Default::default()
            },
            parts: vec![serde_json::json!({"id": "part", "messageID": "previous", "sessionID": session.id, "type": "text", "text": "remember this"})],
        }).unwrap();
        drop(store);
        let store = Store::open(&database, "second").unwrap();
        let resumed = open(&store, None, Some(&session.id), false).unwrap();
        assert_eq!(resumed.directory, path);
        assert_eq!(
            store.messages(&resumed.id).unwrap()[0].parts[0]["text"],
            "remember this"
        );
        assert_eq!(store.list_sessions(None).unwrap().len(), 1);
    }

    #[test]
    fn resume_reports_missing_busy_and_mismatched_sessions() {
        let project = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let path = directory(project.path().to_str().unwrap()).unwrap();
        let store = Store::open(Path::new(":memory:"), "test").unwrap();
        assert!(open(&store, Some(&path), None, true)
            .unwrap_err()
            .contains("No saved sessions"));
        assert!(open(&store, None, Some("missing"), false)
            .unwrap_err()
            .contains("Session not found"));
        let session = open(&store, Some(&path), None, false).unwrap();
        assert!(
            open(&store, other.path().to_str(), Some(&session.id), false)
                .unwrap_err()
                .contains("belongs to")
        );
        store.set_busy(&session.id, true).unwrap();
        assert!(open(&store, None, Some(&session.id), false)
            .unwrap_err()
            .contains("already busy"));
    }

    #[test]
    fn continue_breaks_equal_timestamps_by_creation_order() {
        let project = tempfile::tempdir().unwrap();
        let path = directory(project.path().to_str().unwrap()).unwrap();
        let store = Store::open(Path::new(":memory:"), "test").unwrap();
        let mut first = store.create_session_owned("first", &path);
        let mut second = store.create_session_owned("second", &path);
        for session in [&mut first, &mut second] {
            session.time.updated = Some(1);
            store.update_session(session).unwrap();
        }
        assert_eq!(open(&store, Some(&path), None, true).unwrap().id, second.id);
    }
}
