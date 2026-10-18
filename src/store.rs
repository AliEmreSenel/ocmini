use crate::types::{
    now_ms, MessageInfo, MessageWithParts, ModelRef, PromptRequest, Session, Time, Tokens,
};
use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;

/// Minutes a claimed session lease stays valid without a heartbeat. A crashed
/// instance's sessions become recoverable for other instances after expiry;
/// a live instance refreshes its lease continuously while draining.
const LEASE_MS: i64 = 60_000;

pub struct Store {
    conn: Mutex<Connection>,
    /// WAL readers run on a dedicated connection so reads never queue behind
    /// writes. In-memory test databases share tables through one connection.
    read: Option<Mutex<Connection>>,
    dirs: Mutex<std::collections::HashMap<String, String>>,
    instance: String,
}

fn sql_error(error: impl std::fmt::Display) -> String {
    format!("Session database: {error}")
}

impl Store {
    /// Open the shared session database. Several ocmini processes use this file
    /// at once, so opening is fallible: contention must surface as a clean
    /// startup error, never as a panic.
    pub fn open(path: &Path, instance: &str) -> Result<Self, String> {
        if let Some(dir) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir).map_err(sql_error)?;
        }
        let conn = Connection::open(path).map_err(sql_error)?;
        conn.busy_timeout(std::time::Duration::from_secs(10))
            .map_err(sql_error)?;
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(sql_error)?;
        conn.pragma_update(None, "synchronous", "NORMAL")
            .map_err(sql_error)?;
        // Let checkpoints release an oversized WAL left by a large transcript.
        conn.pragma_update(None, "journal_size_limit", 1024 * 1024)
            .map_err(sql_error)?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS session(id TEXT PRIMARY KEY, json TEXT NOT NULL, time_created INTEGER, time_updated INTEGER, parent_id TEXT, busy INTEGER DEFAULT 0);
             CREATE TABLE IF NOT EXISTS message(id TEXT PRIMARY KEY, session_id TEXT NOT NULL, role TEXT, json TEXT NOT NULL, time_created INTEGER);
             CREATE TABLE IF NOT EXISTS part(id TEXT PRIMARY KEY, message_id TEXT NOT NULL, session_id TEXT NOT NULL, json TEXT NOT NULL, time_created INTEGER);
             CREATE INDEX IF NOT EXISTS msg_sid ON message(session_id, time_created);
             CREATE INDEX IF NOT EXISTS part_sid ON part(session_id, time_created);
             CREATE INDEX IF NOT EXISTS part_mid ON part(message_id);
             CREATE INDEX IF NOT EXISTS session_parent ON session(parent_id);
             CREATE INDEX IF NOT EXISTS session_updated ON session(time_updated);
             CREATE TABLE IF NOT EXISTS inbox(seq INTEGER PRIMARY KEY AUTOINCREMENT, message_id TEXT NOT NULL UNIQUE, session_id TEXT NOT NULL, request TEXT NOT NULL, started INTEGER NOT NULL DEFAULT 0);
             CREATE INDEX IF NOT EXISTS inbox_session ON inbox(session_id, seq);
             CREATE TABLE IF NOT EXISTS todo(session_id TEXT PRIMARY KEY, json TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS owner(session_id TEXT PRIMARY KEY, instance_id TEXT NOT NULL, lease_expires INTEGER NOT NULL);
             CREATE TRIGGER IF NOT EXISTS session_delete_parts AFTER DELETE ON session BEGIN
               DELETE FROM part WHERE session_id=OLD.id;
               DELETE FROM message WHERE session_id=OLD.id;
               DELETE FROM inbox WHERE session_id=OLD.id;
               DELETE FROM todo WHERE session_id=OLD.id;
               DELETE FROM owner WHERE session_id=OLD.id;
             END;
             CREATE TRIGGER IF NOT EXISTS message_delete_parts AFTER DELETE ON message BEGIN
               DELETE FROM part WHERE message_id=OLD.id;
             END;",
        ).map_err(sql_error)?;
        // One-time schema migrations run under the write lock exactly once per
        // database; every instance opening the file must not repeat these writes.
        let version: i32 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .map_err(sql_error)?;
        if version < 2 {
            conn.execute_batch(
                "DELETE FROM part WHERE session_id NOT IN (SELECT id FROM session) OR message_id NOT IN (SELECT id FROM message);
                 DELETE FROM message WHERE session_id NOT IN (SELECT id FROM session);
                 DELETE FROM inbox WHERE session_id NOT IN (SELECT id FROM session);
                 DELETE FROM todo WHERE session_id NOT IN (SELECT id FROM session);").map_err(sql_error)?;
            conn.pragma_update(None, "user_version", 2)
                .map_err(sql_error)?;
        }
        let read = if path.to_string_lossy() == ":memory:" {
            None
        } else {
            Some(Mutex::new(open_connection(path)?))
        };
        let store = Self {
            conn: Mutex::new(conn),
            read,
            dirs: Mutex::new(Default::default()),
            instance: instance.to_string(),
        };
        store.recover_interrupted()?;
        // Retention is opt-in: old thread history must remain resumable from T3.
        if let Ok(days) = std::env::var("OCMINI_RETENTION_DAYS") {
            let days: i64 = days
                .parse()
                .map_err(|_| "OCMINI_RETENTION_DAYS must be a nonnegative integer")?;
            if days < 0 {
                return Err("OCMINI_RETENTION_DAYS must be nonnegative".into());
            }
            if days > 0 {
                store.prune(now_ms().saturating_sub(days.saturating_mul(86_400_000)))?;
            }
        }
        Ok(store)
    }

    /// A write transaction that takes the write lock immediately. Deferred
    /// transactions upgrade read→write with an immediate SQLITE_BUSY that
    /// bypasses busy_timeout, which is the classic multi-process "database is
    /// locked" failure; BEGIN IMMEDIATE makes the busy handler apply.
    fn write_tx<'a>(&'a self, c: &'a mut Connection) -> Result<Transaction<'a>, String> {
        c.transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_error)
    }

    /// Mark a session's work as owned by this instance. Live sessions with a
    /// fresh lease are invisible to startup recovery in every other process.
    pub fn claim(&self, sid: &str) -> Result<(), String> {
        self.conn.lock().unwrap().execute(
            "INSERT INTO owner(session_id,instance_id,lease_expires) VALUES(?1,?2,?3) ON CONFLICT(session_id) DO UPDATE SET instance_id=excluded.instance_id,lease_expires=excluded.lease_expires",
            params![sid, self.instance, now_ms() + LEASE_MS],
        ).map_err(sql_error)?;
        Ok(())
    }

    pub fn heartbeat(&self, sid: &str) -> Result<(), String> {
        self.conn
            .lock()
            .unwrap()
            .execute(
                "UPDATE owner SET lease_expires=?2 WHERE session_id=?1 AND instance_id=?3",
                params![sid, now_ms() + LEASE_MS, self.instance],
            )
            .map_err(sql_error)?;
        Ok(())
    }

    pub fn release(&self, sid: &str) {
        let _ = self.conn.lock().unwrap().execute(
            "DELETE FROM owner WHERE session_id=?1 AND instance_id=?2",
            params![sid, self.instance],
        );
    }

    fn recover_interrupted(&self) -> Result<(), String> {
        // Only recover sessions this process owns, sessions with an expired
        // lease, or sessions no instance has ever claimed. A sibling instance
        // that is actively streaming must not be interrupted by a restart.
        let mut c = self.conn.lock().unwrap();
        let tx = self.write_tx(&mut c)?;
        let now = now_ms();
        let unfinished: Vec<String> = {
            let mut s = tx.prepare("SELECT json FROM message WHERE role='assistant' AND json_extract(json,'$.time.completed') IS NULL AND session_id IN (SELECT s.id FROM session s LEFT JOIN owner o ON o.session_id=s.id WHERE o.session_id IS NULL OR o.instance_id=?1 OR o.lease_expires<?2)").map_err(sql_error)?;
            let rows = s
                .query_map(params![self.instance, now], |r| r.get(0))
                .map_err(sql_error)?;
            rows.collect::<Result<_, _>>().map_err(sql_error)?
        };
        for raw in unfinished {
            let mut info: MessageInfo = serde_json::from_str(&raw).map_err(sql_error)?;
            if info.time.completed.is_some() {
                continue;
            }
            info.time.completed = Some(now_ms().max(info.time.created));
            info.error = Some(
                json!({"name":"MessageAbortedError","data":{"message":"Server stopped before this response completed; send a new prompt to continue."}}),
            );
            tx.execute(
                "UPDATE message SET json=?2 WHERE id=?1",
                params![info.id, serde_json::to_string(&info).map_err(sql_error)?],
            )
            .map_err(sql_error)?;
        }
        let parts: Vec<(String, String)> = {
            let mut s = tx.prepare("SELECT id,json FROM part WHERE json_extract(json,'$.type')='tool' AND json_extract(json,'$.state.status') IN ('pending','running') AND session_id IN (SELECT s.id FROM session s LEFT JOIN owner o ON o.session_id=s.id WHERE o.session_id IS NULL OR o.instance_id=?1 OR o.lease_expires<?2)").map_err(sql_error)?;
            let rows = s
                .query_map(params![self.instance, now], |r| Ok((r.get(0)?, r.get(1)?)))
                .map_err(sql_error)?;
            rows.collect::<Result<_, _>>().map_err(sql_error)?
        };
        for (id, raw) in parts {
            let mut part: Value = serde_json::from_str(&raw).map_err(sql_error)?;
            part["state"]["status"] = json!("error");
            part["state"]["error"] =
                json!("Execution interrupted by server restart; the operation was not replayed.");
            part["state"]["time"]["end"] = json!(now_ms());
            tx.execute(
                "UPDATE part SET json=?2 WHERE id=?1",
                params![id, part.to_string()],
            )
            .map_err(sql_error)?;
        }
        // Never replay a possibly completed filesystem or shell side effect after a crash.
        tx.execute("DELETE FROM inbox WHERE started=1 AND session_id IN (SELECT s.id FROM session s LEFT JOIN owner o ON o.session_id=s.id WHERE o.session_id IS NULL OR o.instance_id=?1 OR o.lease_expires<?2)", params![self.instance, now]).map_err(sql_error)?;
        tx.execute("UPDATE session SET busy=0 WHERE busy=1 AND id IN (SELECT s.id FROM session s LEFT JOIN owner o ON o.session_id=s.id WHERE o.session_id IS NULL OR o.instance_id=?1 OR o.lease_expires<?2)", params![self.instance, now]).map_err(sql_error)?;
        tx.commit().map_err(sql_error)
    }

    /// Cached session→directory lookup for event stream filtering; session
    /// directories never change, so the only staleness is deleted sessions.
    pub fn directory_of(&self, sid: &str) -> Option<String> {
        if let Some(directory) = self.dirs.lock().unwrap().get(sid) {
            return Some(directory.clone());
        }
        let directory: Option<String> = self
            .with_read(|c| {
                c.query_row(
                    "SELECT json_extract(json,'$.directory') FROM session WHERE id=?1",
                    params![sid],
                    |r| r.get::<_, String>(0),
                )
                .map_err(sql_error)
            })
            .ok();
        if let Some(directory) = &directory {
            self.dirs
                .lock()
                .unwrap()
                .insert(sid.to_string(), directory.clone());
        }
        directory
    }

    pub fn prune(&self, before: i64) -> Result<usize, String> {
        let c = self.conn.lock().unwrap();
        c.execute("DELETE FROM session WHERE busy=0 AND time_updated<?1 AND id NOT IN (SELECT session_id FROM inbox) AND id NOT IN (SELECT parent_id FROM session WHERE parent_id IS NOT NULL) AND id NOT IN (SELECT session_id FROM owner WHERE lease_expires>=?2)", params![before, now_ms()]).map_err(sql_error)
    }

    #[cfg(test)]
    pub fn create_session_owned(&self, title: &str, dir: &str) -> Session {
        self.try_create_session_owned(title, dir)
            .unwrap_or_else(|e| panic!("{e}"))
    }

    pub fn try_create_session_owned(&self, title: &str, dir: &str) -> Result<Session, String> {
        let now = now_ms();
        let s = Session {
            id: crate::types::hex_id("ses_"),
            slug: crate::types::slug(),
            project_id: "global".into(),
            directory: dir.to_string(),
            path: dir.to_string(),
            cost: 0.0,
            tokens: Tokens::default(),
            title: title.to_string(),
            version: crate::OPENCODE_COMPAT_VERSION.into(),
            time: Time {
                created: now,
                updated: Some(now),
                ..Time::default()
            },
            parent_id: None,
            permission: None,
            extra: Default::default(),
        };
        self.create_session(&s)?;
        Ok(s)
    }

    pub fn create_session(&self, s: &Session) -> Result<(), String> {
        self.conn.lock().unwrap().execute(
            "INSERT INTO session(id,json,time_created,time_updated,parent_id) VALUES(?1,?2,?3,?4,?5)",
            params![s.id, serde_json::to_string(s).map_err(sql_error)?, s.time.created, s.time.updated.unwrap_or(s.time.created), s.parent_id],
        ).map_err(sql_error)?;
        Ok(())
    }

    pub fn list_sessions(&self, dir: Option<&str>) -> Result<Vec<Session>, String> {
        self.list_sessions_filtered(None, |session| {
            dir.is_none_or(|d| Path::new(&session.directory) == Path::new(d))
        })
    }

    /// Apply filters as rows arrive and stop after enough matches. Sorting uses
    /// the timestamp index, so a short page does not load every saved session.
    pub fn list_sessions_filtered(
        &self,
        limit: Option<usize>,
        mut matches: impl FnMut(&Session) -> bool,
    ) -> Result<Vec<Session>, String> {
        if limit == Some(0) {
            return Ok(Vec::new());
        }
        self.with_read(|c| {
            let mut stmt = c
                .prepare("SELECT json FROM session ORDER BY time_updated DESC,rowid DESC")
                .map_err(sql_error)?;
            let rows = stmt
                .query_map([], |r| r.get::<_, String>(0))
                .map_err(sql_error)?;
            let mut sessions = Vec::new();
            for row in rows {
                let session = serde_json::from_str(&row.map_err(sql_error)?).map_err(sql_error)?;
                if matches(&session) {
                    sessions.push(session);
                    if limit == Some(sessions.len()) {
                        break;
                    }
                }
            }
            Ok(sessions)
        })
    }

    pub fn get_session(&self, id: &str) -> Result<Option<Session>, String> {
        let raw: Option<String> = self
            .conn
            .lock()
            .unwrap()
            .query_row("SELECT json FROM session WHERE id=?1", params![id], |r| {
                r.get::<_, String>(0)
            })
            .optional()
            .map_err(sql_error)?;
        raw.map(|j| serde_json::from_str(&j).map_err(sql_error))
            .transpose()
    }

    pub fn update_session(&self, s: &Session) -> Result<(), String> {
        let n = self
            .conn
            .lock()
            .unwrap()
            .execute(
                "UPDATE session SET json=?2,time_updated=?3,parent_id=?4 WHERE id=?1",
                params![
                    s.id,
                    serde_json::to_string(s).map_err(sql_error)?,
                    s.time.updated.unwrap_or_else(now_ms),
                    s.parent_id
                ],
            )
            .map_err(sql_error)?;
        if n == 0 {
            return Err("Session not found".into());
        }
        Ok(())
    }

    pub fn delete_session(&self, id: &str) -> Result<bool, String> {
        let mut c = self.conn.lock().unwrap();
        let tx = self.write_tx(&mut c)?;
        let exists: bool = tx
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM session WHERE id=?1)",
                params![id],
                |r| r.get(0),
            )
            .map_err(sql_error)?;
        if !exists {
            return Ok(false);
        }
        // Check the inbox as well as busy: admission commits before the engine
        // publishes busy, and a delete must not erase acknowledged work.
        let active: bool = tx.query_row("WITH RECURSIVE descendants(id) AS (SELECT id FROM session WHERE id=?1 UNION SELECT s.id FROM session s JOIN descendants d ON s.parent_id=d.id) SELECT EXISTS(SELECT 1 FROM session WHERE id IN (SELECT id FROM descendants) AND busy=1) OR EXISTS(SELECT 1 FROM inbox WHERE session_id IN (SELECT id FROM descendants))", params![id], |row| row.get(0)).map_err(sql_error)?;
        if active {
            return Err(
                "Session or a child session is busy; abort and wait for idle before deleting"
                    .into(),
            );
        }
        tx.execute("WITH RECURSIVE descendants(id) AS (SELECT id FROM session WHERE id=?1 UNION SELECT s.id FROM session s JOIN descendants d ON s.parent_id=d.id) DELETE FROM session WHERE id IN (SELECT id FROM descendants)", params![id]).map_err(sql_error)?;
        tx.commit().map_err(sql_error)?;
        Ok(true)
    }

    pub fn children(&self, id: &str) -> Result<Vec<Session>, String> {
        self.with_read(|c| {
            let mut stmt = c
                .prepare("SELECT json FROM session WHERE parent_id=?1 ORDER BY time_updated DESC,rowid DESC")
                .map_err(sql_error)?;
            let rows = stmt.query_map(params![id], |r| r.get::<_, String>(0)).map_err(sql_error)?;
            rows.map(|row| serde_json::from_str(&row.map_err(sql_error)?).map_err(sql_error))
                .collect()
        })
    }

    pub fn set_busy(&self, id: &str, busy: bool) -> Result<bool, String> {
        let n = self
            .conn
            .lock()
            .unwrap()
            .execute(
                "UPDATE session SET busy=?2 WHERE id=?1",
                params![id, busy as i32],
            )
            .map_err(sql_error)?;
        Ok(n == 1)
    }

    pub fn is_busy(&self, id: &str) -> Result<bool, String> {
        Ok(self
            .with_read(|c| {
                c.query_row("SELECT busy FROM session WHERE id=?1", params![id], |r| {
                    r.get::<_, i32>(0)
                })
                .optional()
                .map_err(sql_error)
            })?
            .unwrap_or(0)
            != 0)
    }

    pub fn busy_sessions(&self) -> Result<Vec<String>, String> {
        self.with_read(|c| {
            let mut s = c
                .prepare("SELECT id FROM session WHERE busy=1 ORDER BY rowid")
                .map_err(sql_error)?;
            let rows = s.query_map([], |r| r.get(0)).map_err(sql_error)?;
            rows.collect::<Result<_, _>>().map_err(sql_error)
        })
    }

    pub fn add_message(&self, m: &MessageWithParts) -> Result<(), String> {
        let mut c = self.conn.lock().unwrap();
        let tx = self.write_tx(&mut c)?;
        save_message(&tx, m)?;
        tx.commit().map_err(sql_error)
    }

    pub fn update_part(&self, session_id: &str, part: &Value) -> Result<(), String> {
        let mut c = self.conn.lock().unwrap();
        let tx = self.write_tx(&mut c)?;
        save_part(&tx, session_id, part, now_ms())?;
        tx.commit().map_err(sql_error)
    }

    pub fn messages(&self, session_id: &str) -> Result<Vec<MessageWithParts>, String> {
        self.messages_limited(session_id, None)
    }

    /// Opt-in context pruning keeps the latest two user turns and 40k tokens
    /// of completed tool output, and marks older output only when at least
    /// another 20k tokens can be removed. Original output stays browsable.
    pub fn prune_tool_outputs(&self, sid: &str, through: &str) -> Result<Vec<Value>, String> {
        let messages = self.context_messages(sid, through)?;
        let mut turns = 0;
        let mut total = 0usize;
        let mut pruned = 0usize;
        let mut parts = Vec::new();
        'history: for message in messages.into_iter().rev() {
            if message.info.role == "user" {
                turns += 1;
            }
            if turns < 2 {
                continue;
            }
            if message.info.summary == Some(true) {
                break;
            }
            for mut part in message.parts.into_iter().rev() {
                if part["type"] != "tool"
                    || part["state"]["status"] != "completed"
                    || part["tool"] == "skill"
                {
                    continue;
                }
                if part["state"]["time"]["compacted"].is_number() {
                    break 'history;
                }
                let tokens = part["state"]["output"]
                    .as_str()
                    .unwrap_or_default()
                    .len()
                    .div_ceil(4);
                total = total.saturating_add(tokens);
                if total <= 40_000 {
                    continue;
                }
                pruned = pruned.saturating_add(tokens);
                part["state"]["time"]["compacted"] = json!(now_ms());
                parts.push(part);
            }
        }
        if pruned <= 20_000 {
            return Ok(Vec::new());
        }
        let mut conn = self.conn.lock().unwrap();
        let tx = self.write_tx(&mut conn)?;
        for part in &parts {
            save_part(&tx, sid, part, now_ms())?;
        }
        tx.commit().map_err(sql_error)?;
        Ok(parts)
    }

    pub fn messages_limited(
        &self,
        session_id: &str,
        limit: Option<usize>,
    ) -> Result<Vec<MessageWithParts>, String> {
        self.with_read(|c| {
            // The message and part queries must see the same window even if a
            // different process appends or deletes history between them.
            let tx = c.unchecked_transaction().map_err(sql_error)?;
            let messages = read_messages(&tx, session_id, limit)?;
            tx.commit().map_err(sql_error)?;
            Ok(messages)
        })
    }

    pub fn message(&self, session_id: &str, id: &str) -> Result<Option<MessageWithParts>, String> {
        self.with_read(|c| read_message(c, session_id, id))
    }

    pub fn last_model(&self, sid: &str) -> Result<Option<ModelRef>, String> {
        self.with_read(|c| {
            let mut stmt = c.prepare("SELECT json FROM message WHERE session_id=?1 ORDER BY time_created DESC,rowid DESC").map_err(sql_error)?;
            let rows = stmt.query_map(params![sid], |row| row.get::<_, String>(0)).map_err(sql_error)?;
            for row in rows {
                let info: MessageInfo = serde_json::from_str(&row.map_err(sql_error)?).map_err(sql_error)?;
                if let Some(model) = info.model {
                    return Ok(Some(model));
                }
                if let (Some(provider_id), Some(model_id)) = (info.provider_id, info.model_id) {
                    return Ok(Some(ModelRef { provider_id, model_id }));
                }
            }
            Ok(None)
        })
    }

    /// An acknowledged prompt retry needs its last response, not every part of
    /// every message in the session.
    pub fn reply_to(&self, sid: &str, parent: &str) -> Result<Option<MessageWithParts>, String> {
        self.with_read(|c| {
            let tx = c.unchecked_transaction().map_err(sql_error)?;
            let id: Option<String> = tx.query_row("SELECT id FROM message WHERE session_id=?1 AND json_extract(json,'$.parentID')=?2 ORDER BY time_created DESC,rowid DESC LIMIT 1", params![sid, parent], |row| row.get(0)).optional().map_err(sql_error)?;
            let message = id.map(|id| read_message(&tx, sid, &id)).transpose()?.flatten();
            tx.commit().map_err(sql_error)?;
            Ok(message)
        })
    }

    /// Model context starts at the latest usable summary. Read only small
    /// message headers until that boundary is known; archived tool results and
    /// attachments stay in SQLite for history, forks and session browsing.
    pub fn context_messages(
        &self,
        sid: &str,
        through: &str,
    ) -> Result<Vec<MessageWithParts>, String> {
        struct Header {
            id: String,
            role: String,
            parent: Option<String>,
            summary: bool,
            created: i64,
            ordinal: i64,
        }
        fn header(row: &rusqlite::Row<'_>) -> rusqlite::Result<Header> {
            Ok(Header {
                id: row.get(0)?,
                role: row.get(1)?,
                parent: row.get(2)?,
                summary: row.get(3)?,
                created: row.get(4)?,
                ordinal: row.get(5)?,
            })
        }
        const FIELDS: &str = "id,json_extract(json,'$.role'),json_extract(json,'$.parentID'),json_extract(json,'$.summary') IS 1 AND json_extract(json,'$.time.completed') IS NOT NULL AND json_extract(json,'$.error') IS NULL,time_created,rowid";
        self.with_read(|c| {
            let tx = c.unchecked_transaction().map_err(sql_error)?;
            let boundary: Option<(i64, i64)> = tx.query_row(
                "SELECT time_created,rowid FROM message WHERE session_id=?1 AND id=?2",
                params![sid, through], |row| Ok((row.get(0)?, row.get(1)?)),
            ).optional().map_err(sql_error)?;
            let mut users = HashMap::new();
            let mut eligible = |h: &Header| -> Result<bool, String> {
                if h.role == "user" {
                    return Ok(boundary.is_none_or(|end| (h.created, h.ordinal) <= end));
                }
                let Some(parent) = &h.parent else { return Ok(true); };
                if let Some(eligible) = users.get(parent) {
                    return Ok(*eligible);
                }
                let position: Option<(i64, i64)> = tx.query_row(
                    "SELECT time_created,rowid FROM message WHERE session_id=?1 AND id=?2 AND json_extract(json,'$.role')='user'",
                    params![sid, parent], |row| Ok((row.get(0)?, row.get(1)?)),
                ).optional().map_err(sql_error)?;
                let eligible = position.is_some_and(|position| boundary.is_none_or(|end| position <= end));
                users.insert(parent.clone(), eligible);
                Ok(eligible)
            };
            // Walk the session index backwards and stop at the newest usable
            // checkpoint. Archived headers need not be decoded on every round.
            let mut stmt = tx.prepare(&format!("SELECT {FIELDS} FROM message WHERE session_id=?1 ORDER BY time_created DESC,rowid DESC")).map_err(sql_error)?;
            let rows = stmt.query_map(params![sid], header).map_err(sql_error)?;
            let mut tail = Vec::new();
            let mut messages = Vec::new();
            for h in rows {
                let h = h.map_err(sql_error)?;
                if !eligible(&h)? { continue; }
                if h.summary {
                    let message = read_message(&tx, sid, &h.id)?.ok_or("Message not found")?;
                    if message.parts.iter().any(|part| part["type"] == "text" && part["text"].as_str().is_some_and(|text| !text.trim().is_empty())) {
                        let pending = message.parts.iter().find_map(|part| part["compactionPending"].as_str());
                        if let Some(pending) = pending {
                            let pending = tx.query_row(&format!("SELECT {FIELDS} FROM message WHERE session_id=?1 AND id=?2"), params![sid, pending], header).optional().map_err(sql_error)?;
                            if let Some(pending) = pending.filter(|p| (p.created, p.ordinal) < (h.created, h.ordinal)) {
                                if eligible(&pending)? {
                                    messages.push(read_message(&tx, sid, &pending.id)?.ok_or("Message not found")?);
                                }
                            }
                        }
                        messages.insert(0, message);
                        break;
                    }
                }
                tail.push(h);
            }
            drop(stmt);
            for h in tail.into_iter().rev() {
                messages.push(read_message(&tx, sid, &h.id)?.ok_or("Message not found")?);
            }
            tx.commit().map_err(sql_error)?;
            Ok(messages)
        })
    }

    pub fn message_owner(&self, id: &str) -> Result<Option<String>, String> {
        self.with_read(|c| {
            c.query_row(
                "SELECT session_id FROM message WHERE id=?1",
                params![id],
                |r| r.get(0),
            )
            .optional()
            .map_err(sql_error)
        })
    }

    /// The receipt, visible user message, and pending work commit together.
    pub fn admit(
        &self,
        request: &PromptRequest,
        message: &MessageWithParts,
    ) -> Result<bool, String> {
        let mut c = self.conn.lock().unwrap();
        let tx = self.write_tx(&mut c)?;
        let owner: Option<(String, String)> = tx
            .query_row(
                "SELECT session_id,role FROM message WHERE id=?1",
                params![message.info.id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(sql_error)?;
        if let Some((sid, role)) = owner {
            if sid == message.info.session_id && role == "user" {
                return Ok(false);
            }
            return Err("messageID already belongs to another session or message type".into());
        }
        save_message(&tx, message)?;
        if !request.no_reply {
            tx.execute(
                "INSERT INTO inbox(message_id,session_id,request) VALUES(?1,?2,?3)",
                params![
                    message.info.id,
                    message.info.session_id,
                    serde_json::to_string(request).map_err(sql_error)?
                ],
            )
            .map_err(sql_error)?;
        }
        let now = now_ms();
        tx.execute("UPDATE session SET time_updated=?2,json=json_set(json,'$.time.updated',?2) WHERE id=?1", params![message.info.session_id,now]).map_err(sql_error)?;
        tx.commit().map_err(sql_error)?;
        Ok(true)
    }

    pub fn next_prompt(&self, session_id: &str) -> Result<Option<PromptRequest>, String> {
        let mut c = self.conn.lock().unwrap();
        let tx = self.write_tx(&mut c)?;
        let next: Option<(i64,String)> = tx.query_row("SELECT seq,request FROM inbox WHERE session_id=?1 AND started=0 ORDER BY seq LIMIT 1", params![session_id], |r| Ok((r.get(0)?,r.get(1)?))).optional().map_err(sql_error)?;
        let Some((seq, raw)) = next else {
            return Ok(None);
        };
        let request = serde_json::from_str(&raw).map_err(sql_error)?;
        tx.execute("UPDATE inbox SET started=1 WHERE seq=?1", params![seq])
            .map_err(sql_error)?;
        tx.commit().map_err(sql_error)?;
        Ok(Some(request))
    }

    pub fn finish_prompt(&self, id: &str) -> Result<(), String> {
        self.conn
            .lock()
            .unwrap()
            .execute("DELETE FROM inbox WHERE message_id=?1", params![id])
            .map_err(sql_error)?;
        Ok(())
    }

    pub fn cancel_pending(&self, sid: &str) -> Result<(), String> {
        let mut c = self.conn.lock().unwrap();
        let tx = self.write_tx(&mut c)?;
        tx.execute("UPDATE part SET json=json_set(json,'$.ignored',json('true')) WHERE message_id IN (SELECT message_id FROM inbox WHERE session_id=?1 AND started=0)",params![sid]).map_err(sql_error)?;
        tx.execute("DELETE FROM inbox WHERE session_id=?1", params![sid])
            .map_err(sql_error)?;
        tx.commit().map_err(sql_error)
    }

    pub fn pending_sessions(&self) -> Result<Vec<String>, String> {
        self.with_read(|c| {
            let mut s = c
                .prepare("SELECT DISTINCT session_id FROM inbox WHERE started=0")
                .map_err(sql_error)?;
            let rows = s.query_map([], |r| r.get(0)).map_err(sql_error)?;
            rows.collect::<Result<_, _>>().map_err(sql_error)
        })
    }

    pub fn has_waiting(&self, sid: &str) -> Result<bool, String> {
        self.with_read(|c| {
            c.query_row(
                "SELECT EXISTS(SELECT 1 FROM inbox WHERE session_id=?1 AND started=0)",
                params![sid],
                |r| r.get(0),
            )
            .map_err(sql_error)
        })
    }

    pub fn is_pending_message(&self, id: &str) -> Result<bool, String> {
        self.with_read(|c| {
            c.query_row(
                "SELECT EXISTS(SELECT 1 FROM inbox WHERE message_id=?1)",
                params![id],
                |r| r.get(0),
            )
            .map_err(sql_error)
        })
    }

    pub fn set_todos(&self, sid: &str, todos: &Value) -> Result<(), String> {
        if !todos.is_array() {
            return Err("todos must be an array".into());
        }
        let c = self.conn.lock().unwrap();
        let exists: bool = c
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM session WHERE id=?1)",
                params![sid],
                |row| row.get(0),
            )
            .map_err(sql_error)?;
        if !exists {
            return Err("Session not found".into());
        }
        c.execute("INSERT INTO todo(session_id,json) VALUES(?1,?2) ON CONFLICT(session_id) DO UPDATE SET json=excluded.json", params![sid,todos.to_string()]).map_err(sql_error)?;
        Ok(())
    }

    #[cfg(test)]
    pub fn todos(&self, sid: &str) -> Result<Value, String> {
        let raw: Option<String> = self.with_read(|c| {
            c.query_row(
                "SELECT json FROM todo WHERE session_id=?1",
                params![sid],
                |r| r.get(0),
            )
            .optional()
            .map_err(sql_error)
        })?;
        Ok(raw
            .map(|s| serde_json::from_str(&s).map_err(sql_error))
            .transpose()?
            .unwrap_or_else(|| json!([])))
    }

    pub fn try_fork(&self, source: &Session, up_to: Option<&str>) -> Result<Session, String> {
        let mut c = self.conn.lock().unwrap();
        let tx = self.write_tx(&mut c)?;
        let history = read_messages(&tx, &source.id, None)?;
        if up_to.is_some_and(|id| !history.iter().any(|m| m.info.id == id)) {
            return Err("Fork boundary message not found".into());
        }
        let mut s = source.clone();
        s.id = crate::types::hex_id("ses_");
        s.slug = crate::types::slug();
        s.title = format!("{} (fork)", source.title);
        s.time = Time {
            created: now_ms(),
            updated: Some(now_ms()),
            ..Time::default()
        };
        s.parent_id = None;
        s.cost = 0.0;
        s.tokens = Tokens::default();
        s.extra.remove("revert");
        s.extra.remove("share");
        let mut ids = HashMap::new();
        let mut copied = Vec::new();
        for mut m in history {
            // OpenCode's boundary is the first EXCLUDED message, as used by T3 rewind.
            if up_to == Some(m.info.id.as_str()) {
                break;
            }
            let new_id = crate::types::hex_id("msg_");
            ids.insert(m.info.id.clone(), new_id.clone());
            m.info.id = new_id.clone();
            m.info.session_id = s.id.clone();
            m.info.parent_id = m
                .info
                .parent_id
                .as_ref()
                .and_then(|id| ids.get(id).cloned());
            if let Some(tokens) = &m.info.tokens {
                s.tokens.input += tokens.input;
                s.tokens.output += tokens.output;
                s.tokens.reasoning += tokens.reasoning;
                s.tokens.cache.read += tokens.cache.read;
                s.tokens.cache.write += tokens.cache.write;
            }
            s.cost += m.info.cost.unwrap_or(0.0);
            for part in &mut m.parts {
                part["id"] = json!(crate::types::hex_id("prt_"));
                part["messageID"] = json!(new_id);
                part["sessionID"] = json!(s.id);
                if let Some(pending) = part["compactionPending"].as_str() {
                    part["compactionPending"] = json!(ids.get(pending));
                }
                // A copied task cannot point at a live child owned by the source thread.
                if part["type"] == "tool" && part["tool"] == "task" {
                    if let Some(meta) = part["state"]["metadata"].as_object_mut() {
                        meta.remove("sessionId");
                        meta.remove("sessionID");
                    }
                }
            }
            copied.push(m);
        }
        tx.execute("INSERT INTO session(id,json,time_created,time_updated,parent_id) VALUES(?1,?2,?3,?4,NULL)", params![s.id,serde_json::to_string(&s).map_err(sql_error)?,s.time.created,s.time.updated]).map_err(sql_error)?;
        for m in copied {
            save_message(&tx, &m)?;
        }
        if let Some(raw) = tx
            .query_row(
                "SELECT json FROM todo WHERE session_id=?1",
                params![source.id],
                |r| r.get::<_, String>(0),
            )
            .optional()
            .map_err(sql_error)?
        {
            tx.execute(
                "INSERT INTO todo(session_id,json) VALUES(?1,?2)",
                params![s.id, raw],
            )
            .map_err(sql_error)?;
        }
        tx.commit().map_err(sql_error)?;
        Ok(s)
    }
}

impl Store {
    fn with_read<T>(&self, f: impl FnOnce(&Connection) -> Result<T, String>) -> Result<T, String> {
        match &self.read {
            Some(read) => f(&read.lock().unwrap()),
            None => f(&self.conn.lock().unwrap()),
        }
    }
}

fn open_connection(path: &Path) -> Result<Connection, String> {
    let conn = Connection::open(path).map_err(sql_error)?;
    conn.busy_timeout(std::time::Duration::from_secs(10))
        .map_err(sql_error)?;
    Ok(conn)
}

fn save_message(tx: &Transaction<'_>, m: &MessageWithParts) -> Result<(), String> {
    let owner: Option<String> = tx
        .query_row(
            "SELECT session_id FROM message WHERE id=?1",
            params![m.info.id],
            |r| r.get(0),
        )
        .optional()
        .map_err(sql_error)?;
    if owner.is_some_and(|sid| sid != m.info.session_id) {
        return Err("Cannot move a message between sessions".into());
    }
    let exists: bool = tx
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM session WHERE id=?1)",
            params![m.info.session_id],
            |r| r.get(0),
        )
        .map_err(sql_error)?;
    if !exists {
        return Err("Session not found".into());
    }
    tx.execute("INSERT INTO message(id,session_id,role,json,time_created) VALUES(?1,?2,?3,?4,?5) ON CONFLICT(id) DO UPDATE SET json=excluded.json,role=excluded.role WHERE message.json<>excluded.json OR message.role IS NOT excluded.role", params![m.info.id,m.info.session_id,m.info.role,serde_json::to_string(&m.info).map_err(sql_error)?,m.info.time.created]).map_err(sql_error)?;
    for p in &m.parts {
        save_part(tx, &m.info.session_id, p, m.info.time.created)?;
    }
    Ok(())
}

fn save_part(tx: &Transaction<'_>, sid: &str, part: &Value, created: i64) -> Result<(), String> {
    let id = part["id"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or("Part requires an id")?;
    let mid = part["messageID"]
        .as_str()
        .ok_or("Part requires a messageID")?;
    if part["sessionID"].as_str() != Some(sid) {
        return Err("Part sessionID mismatch".into());
    }
    let owner: Option<String> = tx
        .query_row(
            "SELECT session_id FROM message WHERE id=?1",
            params![mid],
            |r| r.get(0),
        )
        .optional()
        .map_err(sql_error)?;
    if owner.as_deref() != Some(sid) {
        return Err("Part's message does not belong to this session".into());
    }
    let old: Option<(String, String)> = tx
        .query_row(
            "SELECT session_id,message_id FROM part WHERE id=?1",
            params![id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .map_err(sql_error)?;
    if old.is_some_and(|(s, m)| s != sid || m != mid) {
        return Err("Part id already belongs to another message".into());
    }
    tx.execute("INSERT INTO part(id,message_id,session_id,json,time_created) VALUES(?1,?2,?3,?4,?5) ON CONFLICT(id) DO UPDATE SET json=excluded.json WHERE part.json<>excluded.json", params![id,mid,sid,part.to_string(),created]).map_err(sql_error)?;
    Ok(())
}

fn read_message(c: &Connection, sid: &str, id: &str) -> Result<Option<MessageWithParts>, String> {
    let raw: Option<String> = c
        .prepare_cached("SELECT json FROM message WHERE id=?1 AND session_id=?2")
        .map_err(sql_error)?
        .query_row(params![id, sid], |row| row.get(0))
        .optional()
        .map_err(sql_error)?;
    let Some(raw) = raw else {
        return Ok(None);
    };
    let mut stmt = c
        .prepare_cached(
            "SELECT json FROM part WHERE message_id=?1 AND session_id=?2 ORDER BY rowid",
        )
        .map_err(sql_error)?;
    let parts = stmt
        .query_map(params![id, sid], |row| row.get::<_, String>(0))
        .map_err(sql_error)?
        .map(|row| serde_json::from_str(&row.map_err(sql_error)?).map_err(sql_error))
        .collect::<Result<_, _>>()?;
    Ok(Some(MessageWithParts {
        info: serde_json::from_str(&raw).map_err(sql_error)?,
        parts,
    }))
}

fn read_messages(
    c: &Connection,
    sid: &str,
    limit: Option<usize>,
) -> Result<Vec<MessageWithParts>, String> {
    let message_sql = if limit.is_some() {
        "SELECT json FROM (SELECT rowid AS ordinal,json,time_created FROM message WHERE session_id=?1 ORDER BY time_created DESC,rowid DESC LIMIT ?2) ORDER BY time_created,ordinal"
    } else {
        "SELECT json FROM message WHERE session_id=?1 ORDER BY time_created,rowid LIMIT ?2"
    };
    let part_sql = if limit.is_some() {
        "SELECT message_id,json FROM part WHERE message_id IN (SELECT id FROM message WHERE session_id=?1 ORDER BY time_created DESC,rowid DESC LIMIT ?2) ORDER BY rowid"
    } else {
        "SELECT message_id,json FROM part WHERE session_id=?1 ORDER BY rowid LIMIT ?2"
    };
    let limit = limit.map_or(-1, |n| i64::try_from(n).unwrap_or(i64::MAX));
    let mut ms = c.prepare(message_sql).map_err(sql_error)?;
    let rows = ms
        .query_map(params![sid, limit], |r| r.get::<_, String>(0))
        .map_err(sql_error)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(MessageWithParts {
            info: serde_json::from_str(&row.map_err(sql_error)?).map_err(sql_error)?,
            parts: Vec::new(),
        });
    }
    let indices: HashMap<String, usize> = out
        .iter()
        .enumerate()
        .map(|(i, m)| (m.info.id.clone(), i))
        .collect();
    let mut ps = c.prepare(part_sql).map_err(sql_error)?;
    let rows = ps
        .query_map(params![sid, limit], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })
        .map_err(sql_error)?;
    for row in rows {
        let (mid, raw) = row.map_err(sql_error)?;
        if let Some(index) = indices.get(&mid) {
            out[*index]
                .parts
                .push(serde_json::from_str(&raw).map_err(sql_error)?);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::MessageTime;

    fn user(s: &Session, id: &str, text: &str) -> MessageWithParts {
        MessageWithParts {
            info: MessageInfo {
                id: id.into(),
                session_id: s.id.clone(),
                role: "user".into(),
                time: MessageTime {
                    created: 123,
                    completed: None,
                },
                ..Default::default()
            },
            parts: vec![
                json!({"id":format!("part_{id}"),"messageID":id,"sessionID":s.id,"type":"text","text":text}),
            ],
        }
    }

    #[test]
    fn fork_excludes_boundary_and_never_overwrites_source() {
        let db = Store::open(Path::new(":memory:"), "inst-a").unwrap();
        let s = db.create_session_owned("test", "/work");
        let a = user(&s, "msg_a", "one");
        let b = user(&s, "msg_b", "two");
        db.add_message(&a).unwrap();
        db.add_message(&b).unwrap();
        let fork = db.try_fork(&s, Some("msg_b")).unwrap();
        let history = db.messages(&fork.id).unwrap();
        assert_eq!(history.len(), 1);
        assert_ne!(history[0].info.id, a.info.id);
        assert_eq!(history[0].parts[0]["sessionID"], fork.id);
        assert_eq!(history[0].parts[0]["messageID"], history[0].info.id);
        assert_eq!(db.messages(&s.id).unwrap().len(), 2);
        assert_eq!(db.messages(&s.id).unwrap()[0].parts, a.parts);
        assert!(fork.parent_id.is_none());
    }

    #[test]
    fn admission_is_idempotent_and_scope_checked() {
        let db = Store::open(Path::new(":memory:"), "inst-a").unwrap();
        let a = db.create_session_owned("a", "/a");
        let b = db.create_session_owned("b", "/b");
        let m = user(&a, "msg_1", "first");
        let req = PromptRequest {
            message_id: Some(m.info.id.clone()),
            ..Default::default()
        };
        assert!(db.admit(&req, &m).unwrap());
        assert!(!db.admit(&req, &m).unwrap());
        assert!(db.admit(&req, &user(&b, "msg_1", "wrong")).is_err());
        assert_eq!(db.messages(&a.id).unwrap().len(), 1);
        assert_eq!(
            db.next_prompt(&a.id).unwrap().unwrap().message_id,
            req.message_id
        );
        assert!(db.next_prompt(&a.id).unwrap().is_none());
    }

    #[test]
    fn deletion_cascades_and_directory_filter_is_exact() {
        let db = Store::open(Path::new(":memory:"), "inst-a").unwrap();
        let a = db.create_session_owned("a", "/app");
        let b = db.create_session_owned("b", "/apple");
        db.add_message(&user(&a, "msg_a", "a")).unwrap();
        db.set_todos(&a.id, &json!([{"content":"a"}])).unwrap();
        assert_eq!(db.list_sessions(Some("/app")).unwrap().len(), 1);
        assert!(db.delete_session(&a.id).unwrap());
        assert!(db.messages(&a.id).unwrap().is_empty());
        assert_eq!(db.todos(&a.id).unwrap(), json!([]));
        assert!(db.get_session(&b.id).unwrap().is_some());
    }

    #[test]
    fn deletion_cannot_erase_admitted_work_or_busy_children() {
        let db = Store::open(Path::new(":memory:"), "inst-a").unwrap();
        let parent = db.create_session_owned("parent", "/app");
        let mut child = db.create_session_owned("child", "/app");
        child.parent_id = Some(parent.id.clone());
        db.update_session(&child).unwrap();
        db.set_todos(&child.id, &json!([{"content": "child todo"}]))
            .unwrap();
        let unrelated = db.create_session_owned("unrelated", "/app");
        let message = user(&child, "queued", "work");
        db.admit(
            &PromptRequest {
                message_id: Some("queued".into()),
                ..Default::default()
            },
            &message,
        )
        .unwrap();
        assert!(db.delete_session(&parent.id).unwrap_err().contains("busy"));
        assert!(db.get_session(&child.id).unwrap().is_some());
        db.cancel_pending(&child.id).unwrap();
        db.set_busy(&child.id, true).unwrap();
        assert!(db.delete_session(&parent.id).is_err());
        db.set_busy(&child.id, false).unwrap();
        assert!(db.delete_session(&parent.id).unwrap());
        assert!(db.get_session(&child.id).unwrap().is_none());
        assert!(db.set_todos(&child.id, &json!([])).is_err());
        assert!(db.get_session(&unrelated.id).unwrap().is_some());
        let connection = db.conn.lock().unwrap();
        for table in ["message", "part", "inbox", "todo", "owner"] {
            let count: i64 = connection
                .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(count, 0, "{table}");
        }
    }

    #[test]
    fn updating_streamed_parts_keeps_order() {
        let db = Store::open(Path::new(":memory:"), "inst-a").unwrap();
        let s = db.create_session_owned("s", "/work");
        let mut m = user(&s, "m", "hello");
        m.parts.push(json!({"id":"reason","messageID":"m","sessionID":s.id,"type":"reasoning","text":"thinking"}));
        db.add_message(&m).unwrap();
        db.add_message(&user(&s, "next", "later")).unwrap();
        m.parts[0]["text"] = json!("hello world");
        db.update_part(&s.id, &m.parts[0]).unwrap();
        db.add_message(&m).unwrap();
        assert_eq!(
            db.messages(&s.id)
                .unwrap()
                .iter()
                .map(|message| message.info.id.as_str())
                .collect::<Vec<_>>(),
            ["m", "next"]
        );
        assert_eq!(db.messages(&s.id).unwrap()[0].parts, m.parts);
    }

    #[test]
    fn selected_history_keeps_chronological_order_parts_and_session_scope() {
        let db = Store::open(Path::new(":memory:"), "test").unwrap();
        let session = db.create_session_owned("history", "/work");
        let other = db.create_session_owned("other", "/work");
        // Insertion order breaks timestamp ties, regardless of the message ID.
        for (id, created) in [("last", 20), ("first", 10), ("middle", 20)] {
            let mut message = user(&session, id, "café 中文");
            message.info.time.created = created;
            message.parts.push(json!({"id":format!("reason_{id}"),"messageID":id,"sessionID":session.id,"type":"reasoning","text":"thought"}));
            db.add_message(&message).unwrap();
        }
        db.add_message(&user(&other, "unrelated", "other session"))
            .unwrap();
        let all = db.messages(&session.id).unwrap();
        for limit in [0, 1, 2, 3, 10, usize::MAX] {
            let selected = db.messages_limited(&session.id, Some(limit)).unwrap();
            assert_eq!(
                serde_json::to_value(&selected).unwrap(),
                serde_json::to_value(&all[all.len().saturating_sub(limit)..]).unwrap(),
            );
        }
        assert_eq!(
            all.iter().map(|m| m.info.id.as_str()).collect::<Vec<_>>(),
            ["first", "last", "middle"]
        );
        assert_eq!(
            serde_json::to_value(db.message(&session.id, "last").unwrap().unwrap()).unwrap(),
            serde_json::to_value(&all[1]).unwrap()
        );
        assert!(db.message(&other.id, "last").unwrap().is_none());
        assert!(db.message(&session.id, "missing").unwrap().is_none());

        // Fetching a selected record must not decode unrelated old transcripts.
        db.conn
            .lock()
            .unwrap()
            .execute(
                "UPDATE part SET json='invalid' WHERE message_id='first'",
                [],
            )
            .unwrap();
        assert!(db.messages(&session.id).is_err());
        assert_eq!(db.messages_limited(&session.id, Some(2)).unwrap().len(), 2);
        assert!(db.message(&session.id, "last").unwrap().is_some());
    }

    #[test]
    fn saving_unchanged_history_avoids_writes_and_reclaims_oversized_wal() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("ocmini.db");
        let db = Store::open(&path, "test").unwrap();
        let session = db.create_session_owned("history", "/work");
        let message = user(&session, "large", &"x".repeat(2 * 1024 * 1024));
        db.add_message(&message).unwrap();
        let changes = db.conn.lock().unwrap().total_changes();
        db.add_message(&message).unwrap();
        db.update_part(&session.id, &message.parts[0]).unwrap();
        assert_eq!(db.conn.lock().unwrap().total_changes(), changes);
        db.conn
            .lock()
            .unwrap()
            .execute_batch("PRAGMA wal_checkpoint(FULL)")
            .unwrap();
        db.add_message(&user(&session, "small", "next")).unwrap();
        assert!(
            std::fs::metadata(path.with_extension("db-wal"))
                .unwrap()
                .len()
                <= 1024 * 1024
        );
        assert_eq!(
            db.message(&session.id, "large").unwrap().unwrap().parts,
            message.parts
        );
    }

    #[test]
    fn startup_recovery_only_touches_expired_or_unclaimed_leases() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("ocmini.db");
        let live = Store::open(&path, "instance-live").unwrap();
        let busy = live.create_session_owned("busy", "/work");
        live.claim(&busy.id).unwrap();
        live.set_busy(&busy.id, true).unwrap();
        live.add_message(&MessageWithParts {
            info: MessageInfo { id:"msg_live".into(), session_id:busy.id.clone(), role:"assistant".into(), time:MessageTime{created:1,completed:None}, ..Default::default() },
            parts: vec![json!({"id":"p1","messageID":"msg_live","sessionID":busy.id,"type":"text","text":"mid-stream"})],
        }).unwrap();
        live.admit(
            &PromptRequest {
                message_id: Some("msg_live".into()),
                ..Default::default()
            },
            &user(&busy, "msg_user", "queued"),
        )
        .unwrap();

        // A second instance opening the same file must leave the live session untouched.
        let sibling = Store::open(&path, "instance-sibling").unwrap();
        assert!(sibling.is_busy(&busy.id).unwrap());
        assert_eq!(sibling.messages(&busy.id).unwrap()[0].info.error, None);
        assert_eq!(sibling.messages(&busy.id).unwrap()[1].info.error, None);

        // Once the lease is expired (crash), the next opening recovers the work.
        sibling
            .conn
            .lock()
            .unwrap()
            .execute(
                "UPDATE owner SET lease_expires=?1",
                params![now_ms() - 1000],
            )
            .unwrap();
        drop(sibling);
        let third = Store::open(&path, "instance-third").unwrap();
        assert_eq!(
            third.messages(&busy.id).unwrap()[0]
                .info
                .error
                .as_ref()
                .and_then(|e| e["name"].as_str()),
            Some("MessageAbortedError")
        );
        assert!(!third.is_busy(&busy.id).unwrap());
    }

    #[test]
    fn recovery_discards_started_prompts_and_preserves_queued_work_and_completed_messages() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("ocmini.db");
        let db = Store::open(&path, "before-restart").unwrap();
        let session = db.create_session_owned("recovery", "/work");
        for id in ["active", "queued"] {
            db.admit(
                &PromptRequest {
                    message_id: Some(id.into()),
                    ..Default::default()
                },
                &user(&session, id, id),
            )
            .unwrap();
        }
        assert_eq!(
            db.next_prompt(&session.id)
                .unwrap()
                .unwrap()
                .message_id
                .as_deref(),
            Some("active")
        );
        for (id, completed) in [("unfinished", None), ("completed", Some(2))] {
            db.add_message(&MessageWithParts {
                info: MessageInfo { id: id.into(), session_id: session.id.clone(), role: "assistant".into(), time: MessageTime { created: 1, completed }, ..Default::default() },
                parts: vec![json!({"id": format!("tool_{id}"), "messageID": id, "sessionID": session.id, "type": "tool", "state": {"status": if completed.is_some() { "completed" } else { "running" }, "time": {"start": 1}}})],
            }).unwrap();
        }
        db.set_busy(&session.id, true).unwrap();
        drop(db);

        let db = Store::open(&path, "after-restart").unwrap();
        assert!(!db.is_busy(&session.id).unwrap());
        assert!(!db.is_pending_message("active").unwrap());
        let queued = db.next_prompt(&session.id).unwrap().unwrap();
        assert_eq!(queued.message_id.as_deref(), Some("queued"));
        assert!(db.next_prompt(&session.id).unwrap().is_none());
        let unfinished = db.message(&session.id, "unfinished").unwrap().unwrap();
        assert_eq!(
            unfinished.info.error.unwrap()["name"],
            "MessageAbortedError"
        );
        assert!(unfinished.info.time.completed.is_some());
        assert_eq!(unfinished.parts[0]["state"]["status"], "error");
        let completed = db.message(&session.id, "completed").unwrap().unwrap();
        assert!(completed.info.error.is_none());
        assert_eq!(completed.info.time.completed, Some(2));
        assert_eq!(completed.parts[0]["state"]["status"], "completed");
    }

    #[test]
    fn legacy_migration_removes_orphan_todos_and_preserves_session_todos() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("ocmini.db");
        let db = Store::open(&path, "migration").unwrap();
        let session = db.create_session_owned("retained", "/work");
        let todos = json!([{"content": "keep"}]);
        db.set_todos(&session.id, &todos).unwrap();
        {
            let connection = db.conn.lock().unwrap();
            connection
                .execute(
                    "INSERT INTO todo(session_id,json) VALUES('orphan','[]')",
                    [],
                )
                .unwrap();
            connection.pragma_update(None, "user_version", 1).unwrap();
        }
        drop(db);
        let db = Store::open(&path, "migration").unwrap();
        assert_eq!(db.todos(&session.id).unwrap(), todos);
        let connection = db.conn.lock().unwrap();
        assert_eq!(
            connection
                .query_row("SELECT count(*) FROM todo", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert_eq!(
            connection
                .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            2
        );
    }

    #[test]
    fn session_listing_reports_corrupt_records() {
        let store = Store::open(Path::new(":memory:"), "test").unwrap();
        let session = store.create_session_owned("corrupt", "/work");
        store
            .conn
            .lock()
            .unwrap()
            .execute(
                "UPDATE session SET json='{}' WHERE id=?1",
                params![session.id],
            )
            .unwrap();
        assert!(store
            .list_sessions(None)
            .unwrap_err()
            .contains("Session database"));
    }

    #[test]
    fn limited_session_listing_does_not_decode_records_beyond_the_requested_page() {
        let store = Store::open(Path::new(":memory:"), "test").unwrap();
        let older = store.create_session_owned("older", "/work");
        let newer = store.create_session_owned("newer", "/work");
        store
            .conn
            .lock()
            .unwrap()
            .execute(
                "UPDATE session SET time_updated=0,json='invalid' WHERE id=?1",
                params![older.id],
            )
            .unwrap();
        assert_eq!(
            store.list_sessions_filtered(Some(1), |_| true).unwrap()[0].id,
            newer.id
        );
        assert!(store
            .list_sessions_filtered(Some(0), |_| true)
            .unwrap()
            .is_empty());
        assert!(store.list_sessions(None).is_err());
    }

    #[test]
    fn context_uses_only_a_valid_summary_and_excludes_queued_prompt_descendants() {
        let db = Store::open(Path::new(":memory:"), "test").unwrap();
        let session = db.create_session_owned("context", "/work");
        let add =
            |id: &str, role: &str, parent: Option<&str>, summary: bool, text: &str, error: bool| {
                let mut message = user(&session, id, text);
                message.info.role = role.into();
                message.info.parent_id = parent.map(str::to_owned);
                message.info.summary = summary.then_some(true);
                message.info.time.completed = (role == "assistant").then_some(123);
                message.info.error = error.then(|| json!({"name":"failed"}));
                db.add_message(&message).unwrap();
            };
        add("u1", "user", None, false, "old prompt", false);
        add("a1", "assistant", Some("u1"), false, "old reply", false);
        add(
            "summary",
            "assistant",
            Some("u1"),
            true,
            "usable summary",
            false,
        );
        add("u2", "user", None, false, "current prompt", false);
        add("a2", "assistant", Some("u2"), false, "current reply", false);
        add("empty", "assistant", Some("u2"), true, "\u{2003}", false);
        add(
            "failed",
            "assistant",
            Some("u2"),
            true,
            "failed summary",
            true,
        );
        add(
            "parentless",
            "assistant",
            None,
            false,
            "keep parentless",
            false,
        );
        add(
            "orphan",
            "assistant",
            Some("missing"),
            false,
            "ignore orphan",
            false,
        );
        add("u3", "user", None, false, "queued prompt", false);
        add(
            "future",
            "assistant",
            Some("u3"),
            true,
            "future summary",
            false,
        );
        let ids = |messages: Vec<MessageWithParts>| {
            messages.into_iter().map(|m| m.info.id).collect::<Vec<_>>()
        };
        assert_eq!(
            ids(db.context_messages(&session.id, "u2").unwrap()),
            ["summary", "u2", "a2", "empty", "failed", "parentless"]
        );
        assert_eq!(
            ids(db.context_messages(&session.id, "u1").unwrap()),
            ["summary", "parentless"]
        );
        assert_eq!(
            ids(db.context_messages(&session.id, "u3").unwrap()),
            ["future"]
        );
        assert_eq!(
            ids(db.context_messages(&session.id, "unknown").unwrap()),
            ["future"]
        );
        let other = db.create_session_owned("other", "/work");
        assert!(db.context_messages(&other.id, "u2").unwrap().is_empty());
        // Archived payloads must not be loaded even if they are unreadable.
        db.conn
            .lock()
            .unwrap()
            .execute("UPDATE part SET json='invalid' WHERE message_id='a1'", [])
            .unwrap();
        assert!(db.messages(&session.id).is_err());
        assert_eq!(
            ids(db.context_messages(&session.id, "u2").unwrap())[0],
            "summary"
        );
        // Stop the reverse scan at the checkpoint: even archived headers are
        // outside the active context, not just their potentially huge parts.
        db.conn
            .lock()
            .unwrap()
            .execute("UPDATE message SET json='invalid' WHERE id='a1'", [])
            .unwrap();
        assert_eq!(
            ids(db.context_messages(&session.id, "u2").unwrap())[0],
            "summary"
        );
    }

    #[test]
    fn context_without_a_summary_preserves_full_eligible_history_and_part_order() {
        let db = Store::open(Path::new(":memory:"), "test").unwrap();
        let session = db.create_session_owned("context", "/work");
        for id in ["first", "second", "third"] {
            let mut message = user(&session, id, "café 中文");
            message.parts.push(json!({"id":format!("extra_{id}"),"messageID":id,"sessionID":session.id,"type":"text","text":"more"}));
            db.add_message(&message).unwrap();
        }
        let all = db.messages(&session.id).unwrap();
        assert_eq!(
            serde_json::to_value(db.context_messages(&session.id, "second").unwrap()).unwrap(),
            serde_json::to_value(&all[..2]).unwrap()
        );
    }

    #[test]
    fn model_and_retry_lookups_do_not_load_unrelated_history_parts() {
        let db = Store::open(Path::new(":memory:"), "test").unwrap();
        let session = db.create_session_owned("history", "/work");
        let mut prompt = user(&session, "prompt", "request");
        prompt.info.model = Some(ModelRef {
            provider_id: "first".into(),
            model_id: "one".into(),
        });
        db.add_message(&prompt).unwrap();
        assert_eq!(db.last_model(&session.id).unwrap(), prompt.info.model);
        for (id, model) in [("older", "two"), ("newer", "three")] {
            let mut reply = user(&session, id, model);
            reply.info.role = "assistant".into();
            reply.info.parent_id = Some("prompt".into());
            reply.info.provider_id = Some("next".into());
            reply.info.model_id = Some(model.into());
            db.add_message(&reply).unwrap();
        }
        db.add_message(&user(
            &session,
            "without-model",
            "latest metadata has no model",
        ))
        .unwrap();
        db.conn
            .lock()
            .unwrap()
            .execute(
                "UPDATE part SET json='invalid' WHERE message_id='older'",
                [],
            )
            .unwrap();
        assert!(db.messages(&session.id).is_err());
        assert_eq!(
            db.last_model(&session.id).unwrap(),
            Some(ModelRef {
                provider_id: "next".into(),
                model_id: "three".into()
            })
        );
        assert_eq!(
            db.reply_to(&session.id, "prompt").unwrap().unwrap().info.id,
            "newer"
        );
        assert!(db.reply_to(&session.id, "missing").unwrap().is_none());
        let other = db.create_session_owned("other", "/work");
        assert!(db.last_model(&other.id).unwrap().is_none());
        assert!(db.reply_to(&other.id, "prompt").unwrap().is_none());
    }
}
