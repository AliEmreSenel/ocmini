use crate::store::Store;
use crate::types::{now_ms, Session};
use serde_json::Value;
use std::path::Path;

/// Titles a launcher gives a fresh session. The first prompt replaces one of
/// these so `--resume` and `session list` say what each conversation was about.
const PLACEHOLDER_TITLES: [&str; 3] = ["tui session", "run", "New session"];

/// Longest auto-title in characters; the picker truncates further to fit.
const TITLE_MAX: usize = 80;

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

/// True when a title came from the launcher rather than from the user. A fork
/// of a placeholder session is a placeholder too. Naming a session is the only
/// job that looks at this, so it is the whole once-only rule.
pub(crate) fn is_placeholder(title: &str) -> bool {
    let title = title.trim_end_matches(" (fork)");
    title.trim().is_empty() || PLACEHOLDER_TITLES.contains(&title)
}

/// The text of a prompt's text parts, in order.
pub fn prompt_text(parts: &[Value]) -> String {
    parts
        .iter()
        .filter(|part| part["type"] == "text")
        .filter_map(|part| part["text"].as_str())
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
pub fn set_title(store: &Store, id: &str, title: &str) -> Result<Option<Session>, String> {
    let Some(mut session) = store.get_session(id)? else {
        return Ok(None);
    };
    if !is_placeholder(&session.title) {
        return Ok(None);
    }
    session.title = title.to_string();
    session.time.updated = Some(now_ms());
    store.update_session(&session)?;
    Ok(Some(session))
}

/// Name a freshly launched session after its prompt, so the resume list shows
/// the topic instead of `tui session`.
pub fn auto_title(store: &Store, id: &str, parts: &[Value]) -> Result<Option<Session>, String> {
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

    #[test]
    fn first_prompt_names_the_session_for_the_resume_list() {
        let project = tempfile::tempdir().unwrap();
        let path = directory(project.path().to_str().unwrap()).unwrap();
        let store = Store::open(Path::new(":memory:"), "test").unwrap();
        let session = open(&store, Some(&path), None, false).unwrap();
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
