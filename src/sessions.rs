use crate::events as event;
pub(crate) mod hooks;
use crate::engine::App;
use crate::store::Store;
use crate::types::{OperationError, PermissionRule, Session};
use serde_json::{json, Value};
use std::path::Path;

/// Titles a launcher gives a fresh session. The first prompt replaces one of
/// these so `--resume` and `session list` say what each conversation was about.
const PLACEHOLDER_TITLES: [&str; 3] = ["tui session", "run", "New session"];

/// Longest auto-title in characters; the picker truncates further to fit.
const TITLE_MAX: usize = 80;

pub fn directory(path: &str) -> Result<String, OperationError> {
    let path = Path::new(path).canonicalize().map_err(|error| {
        OperationError::invalid(format!("Cannot open directory {path}: {error}"))
    })?;
    if !path.is_dir() {
        return Err(OperationError::invalid(format!(
            "Not a directory: {}",
            path.display()
        )));
    }
    Ok(path.to_string_lossy().into_owned())
}

pub fn open(
    store: &Store,
    path: Option<&str>,
    id: Option<&str>,
    continue_last: bool,
) -> Result<Session, OperationError> {
    let path = path.map(directory).transpose()?;
    let session = if let Some(id) = id {
        let session = store
            .get_session(id)?
            .ok_or_else(|| OperationError::not_found(format!("Session not found: {id}")))?;
        if let Some(path) = &path {
            if *path != directory(&session.directory)? {
                return Err(OperationError::not_found(format!(
                    "Session {id} belongs to {}",
                    session.directory
                )));
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
                .ok_or_else(|| OperationError::not_found(format!("No saved sessions in {path}")))?
        } else {
            return Err("Fresh sessions must be created through the application lifecycle".into());
        }
    };
    if store.is_busy(&session.id)? {
        return Err(OperationError::busy(format!(
            "Session {} is already busy",
            session.id
        )));
    }
    // Verify the saved project still exists before entering terminal raw mode.
    directory(&session.directory)?;
    Ok(session)
}

/// CLI subprocesses inherit explicit hook correlation from the command runner.
pub fn reject_correlated_reentry(session_id: &str) -> Result<(), OperationError> {
    let invocation = std::env::var("OCMINI_HOOK_INVOCATION_ID").ok();
    let origin_session = std::env::var("OCMINI_HOOK_SESSION_ID").ok();
    if invocation.as_ref().is_some_and(|value| !value.is_empty())
        && origin_session.as_deref() == Some(session_id)
    {
        return Err(OperationError::new(
            crate::types::FailureKind::HookReentry,
            "A hook cannot synchronously wait for a prompt in its own session",
        ));
    }
    Ok(())
}

/// Open a launcher session through the resource lifecycle. Resuming is read-only.
pub async fn open_for_app(
    app: &App,
    path: Option<&str>,
    id: Option<&str>,
    continue_last: bool,
) -> Result<Session, OperationError> {
    if id.is_some() || continue_last {
        return open(&app.store, path, id, continue_last);
    }
    let path = directory(path.unwrap_or("."))?;
    create(app, Session::new("tui session", &path), None).await
}

/// User/resource creation; automatic accounting and title updates use Store directly.
pub fn set_status(app: &App, sid: &str, status: event::SessionStatus) {
    let mut statuses = app.statuses.lock().unwrap();
    match status {
        event::SessionStatus::Idle => {
            statuses.remove(sid);
        }
        _ => {
            statuses.insert(sid.to_owned(), status.clone());
        }
    }
    drop(statuses);
    app.hooks.emit(
        event::Session::Status {
            session_id: sid,
            status: &status,
        },
        &app.session_context(sid, json!({})),
    );
}

pub async fn create(
    app: &App,
    mut session: Session,
    metadata: Option<&Value>,
) -> Result<Session, OperationError> {
    if let Some(metadata) = metadata {
        apply_public_fields(&mut session, &json!({"metadata":metadata}), false)?;
    }
    let app = app.for_directory(&session.directory)?;
    validate_parent(&app, &session)?;
    let context =
        json!({"directory":session.directory,"sessionID":session.id,"parentID":session.parent_id});
    let input = app
        .hooks
        .run(
            crate::sessions::hooks::SESSION_CREATE_BEFORE,
            &context,
            json!({"title":session.title,"metadata":session.extra,"permission":session.permission}),
            None,
        )
        .await?
        .input;
    apply_public_fields(&mut session, &input, true)?;
    validate_parent(&app, &session)?;
    app.store.create_session(&session)?;
    app.hooks
        .emit(event::Session::Created { info: &session }, &context);
    Ok(session)
}

fn validate_parent(app: &App, session: &Session) -> Result<(), OperationError> {
    if let Some(parent) = &session.parent_id {
        let parent = app
            .store
            .get_session(parent)?
            .ok_or_else(|| OperationError::not_found("Parent session not found"))?;
        if directory(&parent.directory)? != directory(&session.directory)? {
            return Err(OperationError::not_found(
                "Parent session not found in this directory",
            ));
        }
    }
    Ok(())
}

fn apply_public_fields(
    session: &mut Session,
    input: &Value,
    title_allowed: bool,
) -> Result<(), OperationError> {
    if title_allowed {
        if let Some(title) = input.get("title") {
            session.title = title
                .as_str()
                .ok_or_else(|| OperationError::invalid("title must be a string"))?
                .into();
        }
    }
    if let Some(metadata) = input.get("metadata") {
        let metadata = metadata
            .as_object()
            .ok_or_else(|| OperationError::invalid("metadata must be an object"))?;
        // These flattened keys are authority or persisted runtime state, never public metadata.
        const RESERVED: &[&str] = &[
            "id",
            "slug",
            "projectID",
            "directory",
            "path",
            "parentID",
            "permission",
            "title",
            "version",
            "time",
            "tokens",
            "cost",
            "revert",
            "share",
            "summary",
            "ocminiTitleSource",
        ];
        if metadata.keys().any(|key| RESERVED.contains(&key.as_str())) {
            return Err(OperationError::invalid(
                "metadata contains a reserved session field",
            ));
        }
        let mut fields: serde_json::Map<String, Value> = session
            .extra
            .iter()
            .filter(|(key, _)| RESERVED.contains(&key.as_str()))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        fields.extend(
            metadata
                .iter()
                .map(|(key, value)| (key.clone(), value.clone())),
        );
        session.extra = fields;
    }
    Ok(())
}

pub async fn update(
    app: &App,
    source: &Session,
    requested: Value,
) -> Result<Session, OperationError> {
    let app = app.for_directory(&source.directory)?;
    if !requested.is_object() {
        return Err(OperationError::invalid("Session body must be an object"));
    }
    // Validate before a handler runs. Permission authority is visible but immutable to hooks.
    let mut validated = source.clone();
    apply_public_fields(&mut validated, &requested, true)?;
    let permission: Option<Vec<PermissionRule>> = requested
        .get("permission")
        .map(crate::permission::parse_rules)
        .transpose()
        .map_err(OperationError::invalid)?;
    let title_requested = requested.get("title").is_some();
    let metadata_requested = requested.get("metadata").is_some();
    let context =
        json!({"directory":source.directory,"sessionID":source.id,"parentID":source.parent_id});
    let input = app
        .hooks
        .run(
            crate::sessions::hooks::SESSION_UPDATE_BEFORE,
            &context,
            requested,
            None,
        )
        .await?
        .input;
    if (!title_requested && input.get("title").is_some())
        || (!metadata_requested && input.get("metadata").is_some())
    {
        return Err(OperationError::invalid(
            "Session hooks may only change explicitly requested fields",
        ));
    }
    let session = app
        .store
        .mutate_session(&source.id, |session| {
            apply_public_fields(session, &input, title_requested)?;
            if title_requested {
                session.title_source = Some(crate::types::TitleSource::Explicit);
            }
            if let Some(permission) = permission {
                session.permission = Some(permission);
            }
            Ok(true)
        })?
        .ok_or_else(|| OperationError::not_found("Session not found"))?;
    app.hooks
        .emit(event::Session::Updated { info: &session }, &context);
    Ok(session)
}

pub async fn fork(
    app: &App,
    source: &Session,
    boundary: Option<&str>,
) -> Result<Session, OperationError> {
    let app = app.for_directory(&source.directory)?;
    if app.store.is_busy(&source.id)? {
        return Err(OperationError::busy("Session is busy"));
    }
    if let Some(boundary) = boundary {
        if app.store.message_owner(boundary)?.as_deref() != Some(&source.id) {
            return Err(OperationError::not_found("Fork boundary message not found"));
        }
    }
    let context =
        json!({"directory":source.directory,"sessionID":source.id,"parentID":source.parent_id});
    let input = app.hooks.run(crate::sessions::hooks::SESSION_FORK_BEFORE, &context,
        json!({"title":format!("{} (fork)",source.title),"sourceID":source.id,"messageID":boundary}), None).await?.input;
    let title = input["title"]
        .as_str()
        .ok_or_else(|| OperationError::invalid("title must be a string"))?;
    let input = app
        .hooks
        .run(
            crate::sessions::hooks::SESSION_CREATE_BEFORE,
            &context,
            json!({"title":title,"metadata":{},"permission":source.permission}),
            None,
        )
        .await?
        .input;
    let mut fields = source.clone();
    fields.extra.clear();
    apply_public_fields(&mut fields, &input, true)?;
    if app.store.is_busy(&source.id)? {
        return Err(OperationError::busy("Session is busy"));
    }
    let forked = app.store.try_fork(source, boundary)?;
    let session = app
        .store
        .mutate_session(&forked.id, |session| {
            session.title = fields.title;
            session.extra.extend(fields.extra);
            Ok(true)
        })?
        .ok_or_else(|| OperationError::not_found("Session not found"))?;
    let created_context =
        json!({"directory":session.directory,"sessionID":session.id,"parentID":session.parent_id});
    app.hooks
        .emit(event::Session::Created { info: &session }, &created_context);
    app.hooks.emit(
        event::Session::Forked {
            source_id: &source.id,
            session_id: &session.id,
            message_id: boundary,
        },
        &created_context,
    );
    Ok(session)
}

pub async fn delete(app: &App, session: &Session) -> Result<(), OperationError> {
    let mut sessions = vec![session.clone()];
    let mut ids = std::collections::HashSet::from([session.id.clone()]);
    let mut index = 0;
    while index < sessions.len() {
        for child in app.store.children(&sessions[index].id)? {
            if !ids.insert(child.id.clone()) {
                return Err("Session ancestry contains a cycle".into());
            }
            sessions.push(child);
        }
        index += 1;
    }
    // Cascade vetoes all run before the transaction or process cleanup begins.
    for session in &sessions {
        if app.store.is_busy(&session.id)? {
            return Err(OperationError::busy("Session or a child session is busy"));
        }
    }
    let scopes = sessions
        .iter()
        .map(|session| app.for_directory(&session.directory))
        .collect::<Result<Vec<_>, _>>()?;
    for (session, scoped) in sessions.iter().zip(&scopes) {
        let context = json!({"directory":session.directory,"sessionID":session.id,"parentID":session.parent_id});
        scoped
            .hooks
            .run(
                crate::sessions::hooks::SESSION_DELETE_BEFORE,
                &context,
                json!({"info":session}),
                None,
            )
            .await?;
    }
    if !app.store.delete_session_checked(
        &session.id,
        &sessions.iter().map(|s| s.id.clone()).collect::<Vec<_>>(),
    )? {
        return Err(OperationError::not_found("Session not found"));
    }
    for (session, scoped) in sessions.iter().zip(&scopes) {
        crate::tools::cancel_background_jobs(&session.id);
        let context = json!({"directory":session.directory,"sessionID":session.id,"parentID":session.parent_id});
        scoped
            .hooks
            .emit(event::Session::Deleted { info: session }, &context);
    }
    Ok(())
}

/// True when a title came from the launcher rather than from the user. A fork
/// of a placeholder session is a placeholder too. Naming a session is the only
/// job that looks at this, so it is the whole once-only rule.
pub(crate) fn is_placeholder(title: &str) -> bool {
    let title = title.trim_end_matches(" (fork)");
    title.trim().is_empty() || PLACEHOLDER_TITLES.contains(&title)
}

pub(crate) fn can_auto_title(session: &Session) -> bool {
    session.title_source.is_none()
        && session.extra.get("ocminiTitleExplicit") != Some(&json!(true))
        && is_placeholder(&session.title)
}

/// The text of a prompt's text parts, in order.
pub fn prompt_text(parts: &[Value]) -> String {
    crate::types::text_parts(parts)
        .collect::<Vec<_>>()
        .join("\n")
}

/// First non-empty trimmed line, shortened to fit a session title.
fn first_line(text: &str) -> Option<String> {
    let line = text.lines().map(str::trim).find(|line| !line.is_empty())?;
    let title: String = line.chars().take(TITLE_MAX).collect();
    Some(title.trim_end().to_string())
}

/// First non-empty line of a prompt, shortened for a session title. `None`
/// when the prompt carries no text, such as an image-only one.
pub fn title_from_prompt(parts: &[Value]) -> Option<String> {
    first_line(&prompt_text(parts))
}

/// Title from a model's reply: its first line, after dropping chat-template
/// control tokens a reasoning model leaked into the text channel. A dangling
/// opener means the reply is the tail of the model's thinking, so the title
/// is what follows the last one. The tokens are built from escapes so this
/// source file never contains them.
pub fn title_from_reply(reply: &str) -> Option<String> {
    const CONTROL: [&str; 2] = ["<\u{5c}im_start\u{5c}>", "<\u{5c}im_end\u{5c}>"];
    let mut text = reply;
    for tag in CONTROL {
        while let Some(index) = text.find(tag) {
            text = &text[index + tag.len()..];
        }
    }
    first_line(text)
}

/// Name a session, keeping a title the user chose (`--title`,
/// `PATCH /session`). Returns the updated session when renamed.
pub fn set_title(store: &Store, id: &str, title: &str) -> Result<Option<Session>, OperationError> {
    store.mutate_session(id, |session| {
        if !can_auto_title(session) {
            return Ok(false);
        }
        session.title = title.to_owned();
        session.title_source = Some(crate::types::TitleSource::Automatic);
        Ok(true)
    })
}

/// Name a freshly launched session after its prompt, so the resume list shows
/// the topic instead of `tui session`.
pub fn auto_title(
    store: &Store,
    id: &str,
    parts: &[Value],
) -> Result<Option<Session>, OperationError> {
    match title_from_prompt(parts) {
        Some(title) => set_title(store, id, &title),
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{MessageInfo, MessageWithParts};

    #[test]
    fn public_metadata_cannot_replace_authority_or_remove_bookkeeping() {
        let store = Store::open(Path::new(":memory:"), "test").unwrap();
        let mut session = store.create_session_owned("initial", "/project");
        session
            .extra
            .insert("revert".into(), json!({"messageID":"saved"}));
        for key in [
            "id",
            "directory",
            "parentID",
            "permission",
            "time",
            "cost",
            "revert",
        ] {
            let original = serde_json::to_value(&session).unwrap();
            assert!(apply_public_fields(
                &mut session,
                &json!({"metadata":{(key):"replacement"}}),
                true
            )
            .is_err());
            assert_eq!(serde_json::to_value(&session).unwrap(), original);
        }
        apply_public_fields(
            &mut session,
            &json!({"title":"requested", "metadata":{"topic":"rust"}}),
            true,
        )
        .unwrap();
        assert_eq!(session.title, "requested");
        assert_eq!(session.extra["topic"], "rust");
        assert_eq!(session.extra["revert"]["messageID"], "saved");
    }

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
            store.create_session_owned("tui session", &path).id,
            latest.id
        );
    }

    #[test]
    fn resume_keeps_the_saved_directory_and_transcript_across_restart() {
        let project = tempfile::tempdir().unwrap();
        let path = directory(project.path().to_str().unwrap()).unwrap();
        let database = project.path().join("sessions.db");
        let store = Store::open(&database, "first").unwrap();
        let session = store.create_session_owned("tui session", &path);
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
        let session = store.create_session_owned("tui session", &path);
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

    #[test]
    fn first_prompt_names_the_session_for_the_resume_list() {
        let project = tempfile::tempdir().unwrap();
        let path = directory(project.path().to_str().unwrap()).unwrap();
        let store = Store::open(Path::new(":memory:"), "test").unwrap();
        let session = store.create_session_owned("tui session", &path);
        assert_eq!(session.title, "tui session");
        let prompt =
            [serde_json::json!({"type":"text","text":"  \nFix the resume picker\nkeep scrolling"})];

        let titled = auto_title(&store, &session.id, &prompt).unwrap().unwrap();
        assert_eq!(titled.title, "Fix the resume picker");
        assert_eq!(
            store.get_session(&session.id).unwrap().unwrap().title,
            "Fix the resume picker"
        );
        // The title summarizes the session, so a later prompt keeps it.
        let later = [serde_json::json!({"type":"text","text":"and the tests?"})];
        assert!(auto_title(&store, &session.id, &later).unwrap().is_none());
    }

    #[test]
    fn titles_come_from_the_first_line_of_the_prompt() {
        let text = |text: &str| vec![serde_json::json!({"type": "text", "text": text})];
        assert_eq!(
            title_from_prompt(&text("Fix the TUI\nsecond line")).as_deref(),
            Some("Fix the TUI")
        );
        assert_eq!(
            title_from_prompt(&text("   \n\t\n  padded  ")).as_deref(),
            Some("padded")
        );
        assert_eq!(title_from_prompt(&text("")).map(|t| t.len()), None);
        // An image-only prompt has nothing to name the session after.
        assert_eq!(
            title_from_prompt(&[
                serde_json::json!({"type":"file","url":"file:///a.png","mime":"image/png"})
            ]),
            None
        );
        let long = title_from_prompt(&text(&"x".repeat(300))).unwrap();
        assert_eq!(long.chars().count(), TITLE_MAX);
    }

    #[test]
    fn a_title_the_user_chose_survives_the_first_prompt() {
        let project = tempfile::tempdir().unwrap();
        let path = directory(project.path().to_str().unwrap()).unwrap();
        let store = Store::open(Path::new(":memory:"), "test").unwrap();
        let prompt = [serde_json::json!({"type":"text","text":"summarize it"})];
        let named = store.create_session_owned("Review the plan", &path);
        assert!(auto_title(&store, &named.id, &prompt).unwrap().is_none());
        // A launcher placeholder, and a fork of one, are both renamed.
        for title in ["run", "New session", "tui session (fork)", "  "] {
            let session = store.create_session_owned(title, &path);
            assert_eq!(
                auto_title(&store, &session.id, &prompt)
                    .unwrap()
                    .unwrap()
                    .title,
                "summarize it",
                "{title}"
            );
        }
        // A prompt with no text leaves the session alone.
        let image = store.create_session_owned("run", &path);
        let image_only =
            [serde_json::json!({"type":"file","url":"file:///a.png","mime":"image/png"})];
        assert!(auto_title(&store, &image.id, &image_only)
            .unwrap()
            .is_none());
        assert_eq!(store.get_session(&image.id).unwrap().unwrap().title, "run");
    }

    #[test]
    fn reply_titles_drop_leaked_control_tokens() {
        // The tokens are built from escapes so this file stays clean.
        let (open, close) = ("<\u{5c}im_start\u{5c}>", "<\u{5c}im_end\u{5c}>");
        assert_eq!(
            title_from_reply("Fix the picker").as_deref(),
            Some("Fix the picker")
        );
        assert_eq!(
            title_from_reply(&format!(
                "{open}thinking words{close}\nFix the picker\nmore"
            ))
            .as_deref(),
            Some("Fix the picker")
        );
        // A dangling opener (closed pair never streamed) keeps the tail.
        assert_eq!(
            title_from_reply(&format!("plan words{open}\nFix the picker")).as_deref(),
            Some("Fix the picker")
        );
        assert_eq!(title_from_reply(&format!("{open}\n{open}  \n")), None);
        assert_eq!(title_from_reply("   \n  "), None);
    }

    #[test]
    fn prompt_text_joins_text_parts() {
        let parts = vec![
            serde_json::json!({"type": "text", "text": "Fix the picker"}),
            serde_json::json!({"type": "file", "url": "file:///a.png", "mime": "image/png"}),
            serde_json::json!({"type": "text", "text": "and keep scrolling"}),
        ];
        assert_eq!(prompt_text(&parts), "Fix the picker\nand keep scrolling");
        assert_eq!(title_from_prompt(&parts).as_deref(), Some("Fix the picker"));
    }
}
