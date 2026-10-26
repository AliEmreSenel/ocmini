// Interactive session selector: live search, cycling sort order and a
// keyboard-driven viewport over the saved sessions.
use crate::println;
use crate::store::Store;
use crate::tui::{decode_event, truncate, Key, Motion, BOLD, DIM, RESET, YELLOW};
use crate::types::{now_ms, Session};
use crossterm::cursor::{MoveToColumn, MoveUp};
use crossterm::event::EventStream;
use crossterm::terminal::{self, Clear, ClearType};
use crossterm::ExecutableCommand;
use futures::StreamExt;
use std::io::Write;

/// Rows drawn between the header and the footer.
const VISIBLE: usize = 10;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Sort {
    UpdatedDesc,
    UpdatedAsc,
    CreatedDesc,
    CreatedAsc,
    TitleAsc,
    TitleDesc,
}

impl Sort {
    fn label(self) -> &'static str {
        match self {
            Sort::UpdatedDesc => "updated ↓",
            Sort::UpdatedAsc => "updated ↑",
            Sort::CreatedDesc => "created ↓",
            Sort::CreatedAsc => "created ↑",
            Sort::TitleAsc => "title a-z",
            Sort::TitleDesc => "title z-a",
        }
    }

    fn next(self) -> Self {
        use Sort::*;
        match self {
            UpdatedDesc => UpdatedAsc,
            UpdatedAsc => CreatedDesc,
            CreatedDesc => CreatedAsc,
            CreatedAsc => TitleAsc,
            TitleAsc => TitleDesc,
            TitleDesc => UpdatedDesc,
        }
    }
}

fn updated(session: &Session) -> i64 {
    session.time.updated.unwrap_or(session.time.created)
}

/// Case-insensitive substring search over title, ID and directory.
fn matches(session: &Session, query: &str) -> bool {
    let query = query.trim().to_lowercase();
    query.is_empty()
        || session.title.to_lowercase().contains(&query)
        || session.id.to_lowercase().contains(&query)
        || session.directory.to_lowercase().contains(&query)
}

fn title_key(session: &Session) -> (String, String) {
    (session.title.to_lowercase(), session.id.clone())
}

fn sort_sessions(rows: &mut [&Session], sort: Sort) {
    if matches!(sort, Sort::TitleAsc | Sort::TitleDesc) {
        rows.sort_by_cached_key(|session| title_key(session));
        if sort == Sort::TitleDesc {
            rows.reverse();
        }
        return;
    }
    rows.sort_by(|a, b| match sort {
        Sort::UpdatedDesc => updated(b).cmp(&updated(a)).then_with(|| b.id.cmp(&a.id)),
        Sort::UpdatedAsc => updated(a).cmp(&updated(b)).then_with(|| b.id.cmp(&a.id)),
        Sort::CreatedDesc => b
            .time
            .created
            .cmp(&a.time.created)
            .then_with(|| b.id.cmp(&a.id)),
        Sort::CreatedAsc => a
            .time
            .created
            .cmp(&b.time.created)
            .then_with(|| b.id.cmp(&a.id)),
        Sort::TitleAsc | Sort::TitleDesc => unreachable!(),
    });
}

/// Coarse "how old" stamp for a row: 42s, 7m, 20h, 12d.
fn time_ago(created: i64, now: i64) -> String {
    let secs = (now - created).max(0) / 1000;
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86_400)
    }
}

/// Truncate from the front so paths keep their tail (the interesting part).
fn tail(text: &str, max: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= max {
        return text.to_string();
    }
    let mut out = String::from("…");
    out.extend(chars[chars.len() - (max - 1).max(1)..].iter());
    out
}

/// Render one frame, replacing the previous one. Returns the number of lines
/// printed so the next frame can erase exactly that block.
fn draw(
    store: &Store,
    scope: Option<&str>,
    rows: &[&Session],
    query: &str,
    sort: Sort,
    cursor: usize,
    drawn: usize,
) -> Result<usize, String> {
    if drawn > 0 {
        let mut out = std::io::stdout();
        out.execute(MoveToColumn(0))
            .and_then(|out| out.execute(MoveUp(drawn.min(u16::MAX as usize) as u16)))
            .and_then(|out| out.execute(Clear(ClearType::FromCursorDown)))
            .map_err(|error| format!("Cannot redraw the session list: {error}"))?;
    }
    let (cols, _) = terminal::size().unwrap_or((80, 24));
    let title_width = (cols as usize).saturating_sub(40).clamp(12, 48);
    let top = cursor
        .saturating_sub(VISIBLE - 1)
        .min(rows.len().saturating_sub(1));
    println!(
        "{DIM}┌ resume · {} · type to search · Tab sort: {} · Enter open · Esc cancel{RESET}",
        scope.unwrap_or("all projects"),
        sort.label()
    );
    println!("{DIM}│{RESET} {BOLD}›{RESET} {query}");
    if rows.is_empty() {
        println!("{DIM}│ no matching sessions{RESET}");
    }
    for (index, session) in rows.iter().enumerate().skip(top).take(VISIBLE) {
        let marker = if index == cursor { "›" } else { " " };
        let style = if index == cursor { BOLD } else { DIM };
        let busy = store.is_busy(&session.id).unwrap_or(false);
        println!(
            "{DIM}│{RESET} {marker} {style}{:<title_width$}{RESET}{} {DIM}{} · {}{RESET}",
            truncate(&session.title, title_width),
            if busy {
                format!(" {YELLOW}busy{RESET}")
            } else {
                String::new()
            },
            time_ago(updated(session), now_ms()),
            tail(
                &session.directory,
                (cols as usize).saturating_sub(title_width + 24).max(8),
            ),
        );
    }
    println!("{DIM}└ {}/{}{RESET}", cursor + 1, rows.len());
    let _ = std::io::stdout().flush();
    Ok(3 + rows.len().clamp(1, VISIBLE))
}

/// Run the selector until Enter picks a session or the user cancels.
/// `scope` limits the list to one canonical directory; `None` shows every
/// project. The caller must already own the terminal's raw mode.
pub(crate) async fn select(
    store: &Store,
    scope: Option<&str>,
    keys: &mut EventStream,
) -> Result<Option<Session>, String> {
    let all = store.list_sessions(scope)?;
    let mut query = String::new();
    let mut sort = Sort::UpdatedDesc;
    let mut cursor = 0usize;
    let mut drawn = 0usize;
    let mut view = None;
    let mut rows = Vec::new();
    loop {
        let key = (query.clone(), sort);
        if view.as_ref() != Some(&key) {
            rows = all
                .iter()
                .filter(|session| matches(session, &query))
                .collect();
            sort_sessions(&mut rows, sort);
            view = Some(key);
        }
        cursor = cursor.min(rows.len().saturating_sub(1));
        drawn = draw(store, scope, &rows, &query, sort, cursor, drawn)?;
        let event = keys
            .next()
            .await
            .ok_or_else(|| "Terminal input closed".to_string())?
            .map_err(|error| error.to_string())?;
        match decode_event(event) {
            Key::Enter => return Ok(rows.get(cursor).copied().cloned()),
            Key::Esc | Key::CtrlC | Key::CtrlD => return Ok(None),
            Key::Tab => {
                sort = sort.next();
                cursor = 0;
            }
            Key::Char(ch) => {
                query.push(ch);
                cursor = 0;
            }
            Key::Backspace => {
                query.pop();
                cursor = 0;
            }
            Key::Up(steps) => cursor = cursor.saturating_sub(steps),
            Key::Down(steps) => cursor = (cursor + steps).min(rows.len().saturating_sub(1)),
            Key::PageUp => cursor = cursor.saturating_sub(VISIBLE),
            Key::PageDown => cursor = (cursor + VISIBLE).min(rows.len().saturating_sub(1)),
            Key::Move(Motion::Home, false) => cursor = 0,
            Key::Move(Motion::End, false) => cursor = rows.len().saturating_sub(1),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn session(id: &str, title: &str, dir: &str, created: i64, updated: i64) -> Session {
        let store = Store::open(Path::new(":memory:"), "test").unwrap();
        let mut session = store.create_session_owned(title, dir);
        session.id = id.into();
        session.time.created = created;
        session.time.updated = Some(updated);
        session
    }

    #[test]
    fn search_matches_title_id_and_directory_case_insensitively() {
        let session = session("ses_A1", "Fix TUI", "/home/me/Proj", 0, 0);
        assert!(matches(&session, ""));
        assert!(matches(&session, "  "));
        assert!(matches(&session, "fix tu"));
        assert!(matches(&session, "SES_a1"));
        assert!(matches(&session, "PROJ"));
        assert!(!matches(&session, "zzz"));
    }

    #[test]
    fn sort_modes_reorder_the_same_rows() {
        let newest = session("ses_new", "alpha", "/x", 1, 30);
        let oldest = session("ses_old", "beta", "/x", 5, 10);
        let mut rows = vec![&oldest, &newest];

        sort_sessions(&mut rows, Sort::UpdatedDesc);
        assert_eq!(
            rows.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
            ["ses_new", "ses_old"]
        );
        sort_sessions(&mut rows, Sort::UpdatedAsc);
        assert_eq!(
            rows.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
            ["ses_old", "ses_new"]
        );
        // Created order is independent from updated order (ses_old was created later).
        sort_sessions(&mut rows, Sort::CreatedDesc);
        assert_eq!(
            rows.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
            ["ses_old", "ses_new"]
        );
        sort_sessions(&mut rows, Sort::TitleAsc);
        assert_eq!(
            rows.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
            ["ses_new", "ses_old"]
        );
        sort_sessions(&mut rows, Sort::TitleDesc);
        assert_eq!(
            rows.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
            ["ses_old", "ses_new"]
        );
    }

    #[test]
    fn equal_sort_keys_stay_deterministic() {
        let a = session("ses_a", "same", "/x", 7, 7);
        let b = session("ses_b", "same", "/x", 7, 7);
        // Time ties break by ID descending; title ties (lowercased) break by ID ascending.
        for (sort, expected) in [
            (Sort::UpdatedDesc, ["ses_b", "ses_a"]),
            (Sort::CreatedDesc, ["ses_b", "ses_a"]),
            (Sort::TitleAsc, ["ses_a", "ses_b"]),
        ] {
            let mut rows = vec![&b, &a];
            sort_sessions(&mut rows, sort);
            assert_eq!(
                rows.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
                expected,
                "{sort:?}"
            );
        }
    }

    #[test]
    fn time_ago_uses_coarse_buckets() {
        let now = 1_700_000_000_000;
        assert_eq!(time_ago(now - 5_000, now), "5s");
        assert_eq!(time_ago(now + 10_000, now), "0s");
        assert_eq!(time_ago(now - 120_000, now), "2m");
        assert_eq!(time_ago(now - 3_600_000, now), "1h");
        assert_eq!(time_ago(now - 2 * 86_400_000, now), "2d");
    }

    #[test]
    fn tail_keeps_the_end_of_long_paths() {
        assert_eq!(tail("/short", 20), "/short");
        assert_eq!(tail("/home/user/projects/ocmini", 10), "…ts/ocmini");
    }
}
