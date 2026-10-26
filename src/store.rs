use crate::types::{
    now_ms, MessageInfo, MessageWithParts, ModelRef, OperationError, PromptRequest, Session, Time,
    Tokens,
};
use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;

mod hook_state;
pub(crate) use hook_state::{AdmissionReservation, InterruptedTurn, RecoveredTurn};
mod hook_events;
pub(crate) use hook_events::{HookDeliveryBudget, OutboxIntent};

/// Minutes a claimed session lease stays valid without a heartbeat. A crashed
/// instance's sessions become recoverable for other instances after expiry;
/// a live instance refreshes its lease continuously while draining.
const LEASE_MS: i64 = 60_000;

/// How long a writer waits for the database lock. Sibling instances recover a
/// large transcript at startup, and several may start at once; waiting keeps
/// that a slow start instead of a "database is locked" failure.
const BUSY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Startup recovery scans, as query prefixes ending in `AND `: the recovery
/// scope is appended by [`scan_interrupted`]. They filter the mirrored
/// `completed`/`kind`/`status` columns, which `msg_unfinished` and
/// `part_unfinished` index for unfinished work only.
const RECOVER_MESSAGES: &str =
    "SELECT id,json FROM message WHERE role='assistant' AND completed IS NULL AND ";
const RECOVER_PARTS: &str =
    "SELECT id,json FROM part WHERE kind='tool' AND status IN ('pending','running') AND ";

/// Recovery scope appended to every scan prefix: sessions no instance claimed,
/// sessions this instance owns, and sessions whose lease expired.
const AVAILABLE_SCOPE: &str = "session_id IN (SELECT s.id FROM session s LEFT JOIN owner o ON o.session_id=s.id WHERE o.session_id IS NULL OR o.instance_id=?1 OR o.lease_expires<?2)";
const RECOVER_SCOPE: &str = "session_id IN (SELECT s.id FROM session s LEFT JOIN owner o ON o.session_id=s.id WHERE o.session_id IS NULL OR o.lease_expires<?2)";

#[derive(Clone)]
pub struct SessionScope {
    pub directory: String,
    pub parent_id: Option<String>,
}

pub struct Store {
    conn: Mutex<Connection>,
    /// WAL readers run on a dedicated connection so reads never queue behind
    /// writes. In-memory test databases share tables through one connection.
    read: Option<Mutex<Connection>>,
    scopes: Mutex<HashMap<String, SessionScope>>,
    instance: String,
    claims: Mutex<HashMap<String, String>>,
}

impl From<rusqlite::Error> for OperationError {
    fn from(error: rusqlite::Error) -> Self {
        let kind = match &error {
            rusqlite::Error::SqliteFailure(error, _)
                if matches!(
                    error.code,
                    rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
                ) =>
            {
                crate::types::FailureKind::Busy
            }
            _ => crate::types::FailureKind::Error,
        };
        Self::new(kind, format!("Session database: {error}"))
    }
}
fn data_error(error: impl std::fmt::Display) -> OperationError {
    OperationError::from(format!("Session database: {error}"))
}

impl Store {
    #[cfg(test)]
    pub(crate) fn open_recovered(path: &Path, instance: &str) -> Result<Self, OperationError> {
        let store = Self::open(path, instance)?;
        crate::hooks::recover(&store)?;
        Ok(store)
    }

    /// Debug inspection must never recover turns, create tables, or start
    /// observer processes. Older stores simply have no delivery metadata yet.
    pub fn inspect_hook_deliveries(
        path: &Path,
        directory: &str,
    ) -> Result<Vec<Value>, OperationError> {
        crate::blocking::run(|| {
            let connection =
                Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
            connection.busy_timeout(BUSY_TIMEOUT)?;
            let exists:bool=connection.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='hook_delivery')",[],|r|r.get(0))?;
            if !exists {
                return Ok(Vec::new());
            }
            let store = Self {
                conn: Mutex::new(connection),
                read: None,
                scopes: Mutex::new(Default::default()),
                instance: "debug".into(),
                claims: Mutex::new(HashMap::new()),
            };
            store.hook_deliveries(directory)
        })
    }

    /// Open the shared session database. Several ocmini processes use this file
    /// at once, so opening is fallible: contention must surface as a clean
    /// startup error, never as a panic.
    pub fn open(path: &Path, instance: &str) -> Result<Self, OperationError> {
        crate::blocking::run(|| {
            // Startup competes with sibling instances that are recovering the same
            // file, and SQLite answers some of those races with SQLITE_BUSY
            // immediately, before busy_timeout applies. Opening is idempotent, so a
            // new instance waits a bounded time instead of failing the launch.
            let mut locked = None;
            for attempt in 0..5 {
                match Self::open_once(path, instance) {
                    Ok(store) => return Ok(store),
                    Err(error) if attempt < 4 && error.kind == crate::types::FailureKind::Busy => {
                        locked = Some(error);
                        std::thread::sleep(std::time::Duration::from_millis(100 << attempt));
                    }
                    Err(error) => return Err(error),
                }
            }
            Err(locked.unwrap_or_else(|| "Session database: unavailable".into()))
        })
    }

    fn open_once(path: &Path, instance: &str) -> Result<Self, OperationError> {
        if let Some(dir) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir).map_err(data_error)?;
        }
        let conn = Connection::open(path)?;
        conn.busy_timeout(BUSY_TIMEOUT)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        // Let checkpoints release an oversized WAL left by a large transcript.
        conn.pragma_update(None, "journal_size_limit", 1024 * 1024)?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS session(id TEXT PRIMARY KEY, json TEXT NOT NULL, time_created INTEGER, time_updated INTEGER, parent_id TEXT, busy INTEGER DEFAULT 0);
             CREATE TABLE IF NOT EXISTS message(id TEXT PRIMARY KEY, session_id TEXT NOT NULL, role TEXT, json TEXT NOT NULL, time_created INTEGER, completed INTEGER);
             CREATE TABLE IF NOT EXISTS part(id TEXT PRIMARY KEY, message_id TEXT NOT NULL, session_id TEXT NOT NULL, json TEXT NOT NULL, time_created INTEGER, kind TEXT, status TEXT);
             CREATE INDEX IF NOT EXISTS msg_sid ON message(session_id, time_created);
             CREATE INDEX IF NOT EXISTS part_sid ON part(session_id, time_created);
             CREATE INDEX IF NOT EXISTS part_mid ON part(message_id);
             CREATE INDEX IF NOT EXISTS session_parent ON session(parent_id);
             CREATE INDEX IF NOT EXISTS session_updated ON session(time_updated);
             CREATE TABLE IF NOT EXISTS inbox(seq INTEGER PRIMARY KEY AUTOINCREMENT, message_id TEXT NOT NULL UNIQUE, session_id TEXT NOT NULL, request TEXT NOT NULL, started INTEGER NOT NULL DEFAULT 0);
             CREATE INDEX IF NOT EXISTS inbox_session ON inbox(session_id, seq);
             CREATE TABLE IF NOT EXISTS todo(session_id TEXT PRIMARY KEY, json TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS owner(session_id TEXT PRIMARY KEY, instance_id TEXT NOT NULL, lease_expires INTEGER NOT NULL,lease_token TEXT);
             CREATE TABLE IF NOT EXISTS hook_turn(message_id TEXT PRIMARY KEY,session_id TEXT NOT NULL,state TEXT NOT NULL,snapshot TEXT,result TEXT,reason TEXT,started INTEGER NOT NULL DEFAULT 0,continuations INTEGER NOT NULL DEFAULT 0,finalizer TEXT);
             CREATE INDEX IF NOT EXISTS hook_turn_session ON hook_turn(session_id);
             CREATE INDEX IF NOT EXISTS hook_turn_pending ON hook_turn(state,session_id) WHERE state IN ('running','finalizing');
             CREATE TABLE IF NOT EXISTS hook_pending_events(message_id TEXT PRIMARY KEY,session_id TEXT NOT NULL,json TEXT NOT NULL);
             CREATE TRIGGER IF NOT EXISTS session_delete_pending_hook_events AFTER DELETE ON session BEGIN DELETE FROM hook_pending_events WHERE session_id=OLD.id; END;
             CREATE TABLE IF NOT EXISTS hook_admission(seq INTEGER PRIMARY KEY AUTOINCREMENT,message_id TEXT NOT NULL UNIQUE,session_id TEXT NOT NULL,token TEXT NOT NULL,state TEXT NOT NULL,deadline INTEGER NOT NULL,error TEXT);
             CREATE INDEX IF NOT EXISTS hook_admission_queue ON hook_admission(session_id,state,seq);
             CREATE TABLE IF NOT EXISTS hook_delivery(subscription_key TEXT NOT NULL,event_id TEXT NOT NULL,directory TEXT NOT NULL,event_json TEXT NOT NULL,event_bytes INTEGER NOT NULL,attempts INTEGER NOT NULL DEFAULT 0,next_attempt INTEGER NOT NULL,expires INTEGER NOT NULL,state TEXT NOT NULL,lease_owner TEXT,lease_until INTEGER,PRIMARY KEY(subscription_key,event_id));
             CREATE INDEX IF NOT EXISTS hook_delivery_claim ON hook_delivery(subscription_key,state,next_attempt);
             CREATE INDEX IF NOT EXISTS hook_delivery_dir ON hook_delivery(directory,state);
             CREATE INDEX IF NOT EXISTS hook_delivery_expiry ON hook_delivery(expires);
             CREATE TRIGGER IF NOT EXISTS session_delete_hooks AFTER DELETE ON session BEGIN
               DELETE FROM hook_turn WHERE session_id=OLD.id;
               DELETE FROM hook_admission WHERE session_id=OLD.id;
             END;
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
        )?;
        conn.execute(
            "DELETE FROM hook_delivery WHERE expires<=?1",
            params![now_ms()],
        )?;
        // One-time schema migrations run under the write lock exactly once per
        // database; every instance opening the file must not repeat these writes.
        let version: i32 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if version < 2 {
            conn.execute_batch(
                "DELETE FROM part WHERE session_id NOT IN (SELECT id FROM session) OR message_id NOT IN (SELECT id FROM message);
                 DELETE FROM message WHERE session_id NOT IN (SELECT id FROM session);
                 DELETE FROM inbox WHERE session_id NOT IN (SELECT id FROM session);
                 DELETE FROM todo WHERE session_id NOT IN (SELECT id FROM session);")?;
            conn.pragma_update(None, "user_version", 2)?;
        }
        if version < 3 {
            // Startup recovery looked for unfinished work inside the JSON, so
            // every open read every message and part row: 741 ms on a store with
            // 38,781 messages and 159,024 parts, matching zero rows. Recovery
            // filters on plain columns now, partially indexed on unfinished
            // work, so a scan touches that work and nothing else.
            for (table, column, kind) in [
                ("message", "completed", "INTEGER"),
                ("part", "kind", "TEXT"),
                ("part", "status", "TEXT"),
            ] {
                add_column(&conn, table, column, kind)?;
            }
            // Malformed legacy rows keep NULL markers: they stay out of recovery,
            // as they already stay out of history reads.
            conn.execute_batch(
                "UPDATE message SET completed=json_extract(json,'$.time.completed') WHERE json_valid(json);
                 UPDATE part SET kind=json_extract(json,'$.type'), status=json_extract(json,'$.state.status') WHERE json_valid(json);
                 CREATE INDEX IF NOT EXISTS msg_unfinished ON message(session_id) WHERE role='assistant' AND completed IS NULL;
                 CREATE INDEX IF NOT EXISTS part_unfinished ON part(session_id) WHERE kind='tool' AND status IN ('pending','running');")?;
            conn.pragma_update(None, "user_version", 3)?;
        }
        let read = if path.to_string_lossy() == ":memory:" {
            None
        } else {
            Some(Mutex::new(open_connection(path)?))
        };
        if version < 4 {
            add_column(&conn, "owner", "lease_token", "TEXT")?;
            conn.pragma_update(None, "user_version", 4)?;
        }
        if version < 5 {
            add_column(&conn, "session", "directory", "TEXT")?;
            conn.execute("UPDATE session SET directory=json_extract(json,'$.directory') WHERE json_valid(json)", [])?;
            conn.execute("CREATE INDEX IF NOT EXISTS session_directory_updated ON session(directory,time_updated DESC)", [])?;
            let directories: Vec<String> = conn
                .prepare("SELECT DISTINCT directory FROM session WHERE directory IS NOT NULL")?
                .query_map([], |row| row.get(0))?
                .collect::<Result<_, _>>()?;
            for directory in directories {
                conn.execute(
                    "UPDATE session SET directory=?2 WHERE directory=?1",
                    params![directory, scope_key(&directory)],
                )?;
            }
            conn.pragma_update(None, "user_version", 5)?;
        }
        let store = Self {
            conn: Mutex::new(conn),
            read,
            scopes: Mutex::new(Default::default()),
            instance: instance.to_string(),
            claims: Mutex::new(HashMap::new()),
        };
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
    fn write_tx<'a>(&'a self, c: &'a mut Connection) -> Result<Transaction<'a>, OperationError> {
        c.transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(OperationError::from)
    }

    /// One boundary owns locking, commit and rollback for every domain write.
    fn transaction<T>(
        &self,
        work: impl FnOnce(&Transaction<'_>) -> Result<T, OperationError>,
    ) -> Result<T, OperationError> {
        self.with_write(|connection| {
            let tx = self.write_tx(connection)?;
            let value = work(&tx)?;
            tx.commit()?;
            Ok(value)
        })
    }

    /// Mark a session's work as owned by this instance. Live sessions with a
    /// fresh lease are invisible to startup recovery in every other process.
    pub fn claim(&self, sid: &str) -> Result<(), OperationError> {
        self.with_write(|connection| {
            let tx = self.write_tx(connection)?;
            let now = now_ms();
            let mut claims = self.claims.lock().unwrap();
            let previous = claims.get(sid).cloned().unwrap_or_default();
            let token = format!("{}:{}", self.instance, crate::types::hex_id("lease_"));
            let claimed = tx.execute(
                "INSERT INTO owner(session_id,instance_id,lease_expires,lease_token) VALUES(?1,?2,?3,?6) ON CONFLICT(session_id) DO UPDATE SET instance_id=excluded.instance_id,lease_expires=excluded.lease_expires,lease_token=excluded.lease_token WHERE owner.lease_token=?4 OR owner.lease_expires<?5",
                params![sid, self.instance, now + LEASE_MS, previous, now, token],
            )?;
            if claimed == 0 {
                return Err(OperationError::busy("Session is busy in another instance"));
            }
            tx.execute("UPDATE session SET busy=1 WHERE id=?1", params![sid])
                ?;
            tx.commit()?;
            claims.insert(sid.to_owned(), token);
            Ok(())
        })
    }

    pub fn heartbeat(&self, sid: &str) -> Result<bool, OperationError> {
        let token = self
            .claims
            .lock()
            .unwrap()
            .get(sid)
            .cloned()
            .unwrap_or_default();
        let changed = self.execute(
                "UPDATE owner SET lease_expires=?2 WHERE session_id=?1 AND lease_token=?3 AND lease_expires>=?4",
                params![sid, now_ms() + LEASE_MS, token, now_ms()],
            )?;
        Ok(changed == 1)
    }

    fn check_claim(&self, connection: &Connection, sid: &str) -> Result<(), OperationError> {
        let token = self.claims.lock().unwrap().get(sid).cloned();
        if let Some(token) = token {
            let owned: bool = connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM owner WHERE session_id=?1 AND lease_token=?2 AND lease_expires>=?3)",
                params![sid, token, now_ms()], |row| row.get(0),
            )?;
            if !owned {
                return Err("Session ownership lost".into());
            }
        }
        Ok(())
    }
    pub fn check_ownership(&self, sid: &str) -> Result<(), OperationError> {
        self.with_read(|connection| self.check_claim(connection, sid))
    }

    pub fn release(&self, sid: &str) {
        let token = self
            .claims
            .lock()
            .unwrap()
            .get(sid)
            .cloned()
            .unwrap_or_default();
        let result = self.transaction(|tx| {
            tx.execute("UPDATE session SET busy=0 WHERE id=?1 AND EXISTS(SELECT 1 FROM owner WHERE session_id=?1 AND lease_token=?2)", params![sid, token])?;
            tx.execute("DELETE FROM owner WHERE session_id=?1 AND lease_token=?2", params![sid, token]).map_err(OperationError::from)
        });
        match result {
            Ok(released) if released != 0 => {
                self.claims.lock().unwrap().remove(sid);
            }
            Ok(_) => {}
            Err(error) => eprintln!("ocmini: {error}"),
        }
    }

    pub fn recover_interrupted(
        &self,
        plans: &[RecoveredTurn],
    ) -> Result<(Vec<std::sync::Arc<crate::types::BusEvent>>, Vec<String>), OperationError> {
        crate::blocking::run(|| {
            // Only recover sessions this process owns, sessions with an expired
            // lease, or sessions no instance has ever claimed. A sibling instance
            // that is actively streaming must not be interrupted by a restart.
            //
            // The two scans below run on the read connection: holding the write lock
            // across them makes every sibling instance queue, and a burst of instance
            // startups pushes them past their busy timeout with "database is locked".
            // Each update re-checks the scanned condition, so a row a sibling changed
            // in the meantime is left alone. The expression indexes below the scan
            // prefixes keep the scans proportional to unfinished work, not to the
            // size of the store.
            let now = now_ms();
            let (unfinished, parts) = self.with_read(|c| {
                Ok((
                    scan_interrupted(c, &self.instance, now, RECOVER_MESSAGES)?,
                    scan_interrupted(c, &self.instance, now, RECOVER_PARTS)?,
                ))
            })?;
            let mut c = self.conn.lock().unwrap();
            let tx = self.write_tx(&mut c)?;
            for (id, raw) in unfinished {
                let mut info: MessageInfo = serde_json::from_str(&raw).map_err(data_error)?;
                info.time.completed = Some(now.max(info.time.created));
                info.error = Some(
                    json!({"name":"MessageAbortedError","data":{"message":"Server stopped before this response completed; send a new prompt to continue."}}),
                );
                tx.execute(
                    "UPDATE message SET json=?2,completed=?3 WHERE id=?1 AND completed IS NULL AND NOT EXISTS(SELECT 1 FROM owner o WHERE o.session_id=message.session_id AND o.lease_expires>=?5)",
                    params![
                        id,
                        serde_json::to_string(&info).map_err(data_error)?,
                        info.time.completed,
                        self.instance,
                        now
                    ],
                )
                ?;
            }
            for (id, raw) in parts {
                let mut part: Value = serde_json::from_str(&raw).map_err(data_error)?;
                part["state"]["status"] = json!("error");
                part["state"]["error"] = json!(
                    "Execution interrupted by server restart; the operation was not replayed."
                );
                part["state"]["time"]["end"] = json!(now);
                tx.execute(
                "UPDATE part SET json=?2,status='error' WHERE id=?1 AND status IN ('pending','running') AND NOT EXISTS(SELECT 1 FROM owner o WHERE o.session_id=part.session_id AND o.lease_expires>=?4)",
                params![id, part.to_string(), self.instance, now],
            )
            ?;
            }
            // Never replay a possibly completed filesystem or shell side effect after a crash.
            let mut recovered_events = Vec::new();
            let mut delivery_failures = Vec::new();
            for plan in plans {
                let source = &plan.source;
                let changed = tx.execute(
                    "UPDATE hook_turn SET state='done',finalizer='interrupted',reason=CASE WHEN state='running' THEN 'error' ELSE reason END,result=CASE WHEN state='running' THEN json_object('error','Execution interrupted by server restart; no effects were replayed.') ELSE result END WHERE message_id=?1 AND state=?2 AND snapshot IS ?3 AND (SELECT json FROM hook_pending_events WHERE message_id=?1) IS ?4 AND NOT EXISTS(SELECT 1 FROM owner o WHERE o.session_id=hook_turn.session_id AND o.lease_expires>=?6)",
                    params![source.message_id, source.state, source.snapshot, source.prepared, self.instance, now],
                )?;
                if changed == 0 {
                    continue;
                }
                delivery_failures.extend(Self::persist_outbox_tx(&tx, &plan.deliveries)?);
                tx.execute(
                    "DELETE FROM hook_pending_events WHERE message_id=?1",
                    params![source.message_id],
                )?;
                recovered_events.extend(plan.events.iter().cloned());
            }
            tx.execute("DELETE FROM inbox WHERE started=1 AND session_id IN (SELECT s.id FROM session s LEFT JOIN owner o ON o.session_id=s.id WHERE o.session_id IS NULL OR o.lease_expires<?2)", params![self.instance, now])?;
            tx.execute("UPDATE session SET busy=0 WHERE busy=1 AND id IN (SELECT s.id FROM session s LEFT JOIN owner o ON o.session_id=s.id WHERE o.session_id IS NULL OR o.lease_expires<?2)", params![self.instance, now])?;
            tx.commit()?;
            drop(c);
            Ok((recovered_events, delivery_failures))
        })
    }

    /// Only immutable scope is cached; policy and other session fields stay authoritative in SQLite.
    pub fn scope(&self, sid: &str) -> Option<SessionScope> {
        if let Some(scope) = self.scopes.lock().unwrap().get(sid) {
            return Some(scope.clone());
        }
        let session = self.get_session(sid).ok().flatten()?;
        let scope = SessionScope {
            directory: session.directory,
            parent_id: session.parent_id,
        };
        let mut cache = self.scopes.lock().unwrap();
        if cache.len() >= 1024 {
            cache.clear();
        }
        cache.insert(sid.into(), scope.clone());
        Some(scope)
    }
    pub fn directory_of(&self, sid: &str) -> Option<String> {
        self.scope(sid).map(|scope| scope.directory)
    }

    pub fn prune(&self, before: i64) -> Result<usize, OperationError> {
        self.execute("DELETE FROM session WHERE busy=0 AND time_updated<?1 AND id NOT IN (SELECT session_id FROM inbox) AND id NOT IN (SELECT parent_id FROM session WHERE parent_id IS NOT NULL) AND id NOT IN (SELECT session_id FROM owner WHERE lease_expires>=?2)", params![before, now_ms()])
    }

    #[cfg(test)]
    pub fn create_session_owned(&self, title: &str, dir: &str) -> Session {
        self.try_create_session_owned(title, dir)
            .unwrap_or_else(|e| panic!("{e}"))
    }

    #[cfg(test)]
    pub fn try_create_session_owned(
        &self,
        title: &str,
        dir: &str,
    ) -> Result<Session, OperationError> {
        let s = Session::new(title, dir);
        self.create_session(&s)?;
        Ok(s)
    }

    pub fn create_session(&self, s: &Session) -> Result<(), OperationError> {
        self.transaction(|tx| {
            // A creation hook can outlive the parent task's ownership claim.
            if let Some(parent) = &s.parent_id {
                self.check_claim(tx, parent)?;
            }
            tx.execute(
                "INSERT INTO session(id,json,time_created,time_updated,parent_id,directory) VALUES(?1,?2,?3,?4,?5,?6)",
                params![s.id, serde_json::to_string(s).map_err(data_error)?, s.time.created, s.time.updated.unwrap_or(s.time.created), s.parent_id, scope_key(&s.directory)],
            )?;
            Ok(())
        })
    }

    pub fn list_sessions(&self, dir: Option<&str>) -> Result<Vec<Session>, OperationError> {
        self.list_sessions_filtered(dir, false, None, |_| true)
    }

    /// Apply filters as rows arrive and stop after enough matches. Sorting uses
    /// the timestamp index, so a short page does not load every saved session.
    pub fn list_sessions_filtered(
        &self,
        directory: Option<&str>,
        roots: bool,
        limit: Option<usize>,
        mut matches: impl FnMut(&Session) -> bool,
    ) -> Result<Vec<Session>, OperationError> {
        if limit == Some(0) {
            return Ok(Vec::new());
        }
        self.with_read(|c| {
            let scope = directory.map(scope_key);
            let predicate = if scope.is_some() { "directory=?1" } else { "?1 IS NULL" };
            let roots = if roots { " AND parent_id IS NULL" } else { "" };
            let sql = format!("SELECT json FROM session WHERE {predicate}{roots} ORDER BY time_updated DESC,rowid DESC");
            let mut stmt = c.prepare(&sql)?;
            let rows = stmt
                .query_map(params![scope], |r| r.get::<_, String>(0))
                ?;
            let mut sessions = Vec::new();
            for row in rows {
                let session = serde_json::from_str(&row?).map_err(data_error)?;
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

    pub fn get_session(&self, id: &str) -> Result<Option<Session>, OperationError> {
        self.with_read(|connection| {
            let raw: Option<String> = connection
                .query_row("SELECT json FROM session WHERE id=?1", params![id], |r| {
                    r.get(0)
                })
                .optional()?;
            raw.map(|raw| serde_json::from_str(&raw).map_err(data_error))
                .transpose()
        })
    }

    /// Read and mutate the latest fields under one writer transaction. A false
    /// result leaves the row untouched (for conditional automatic updates).
    pub fn mutate_session(
        &self,
        id: &str,
        change: impl FnOnce(&mut Session) -> Result<bool, OperationError>,
    ) -> Result<Option<Session>, OperationError> {
        self.transaction(|tx| {
            let raw: Option<String> = tx
                .query_row("SELECT json FROM session WHERE id=?1", params![id], |r| {
                    r.get(0)
                })
                .optional()?;
            let Some(raw) = raw else {
                return Ok(None);
            };
            let mut session: Session = serde_json::from_str(&raw).map_err(data_error)?;
            if !change(&mut session)? {
                return Ok(None);
            }
            session.time.updated = Some(now_ms());
            tx.execute(
                "UPDATE session SET json=?2,time_updated=?3,parent_id=?4,directory=?5 WHERE id=?1",
                params![
                    id,
                    serde_json::to_string(&session).map_err(data_error)?,
                    session.time.updated,
                    session.parent_id,
                    scope_key(&session.directory)
                ],
            )?;
            Ok(Some(session))
        })
    }

    #[cfg(test)]
    pub fn update_session(&self, session: &Session) -> Result<(), OperationError> {
        self.mutate_session(&session.id, |current| {
            *current = session.clone();
            Ok(true)
        })?
        .ok_or_else(|| OperationError::not_found("Session not found"))
        .map(|_| ())
    }

    #[cfg(test)]
    pub fn delete_session(&self, id: &str) -> Result<bool, OperationError> {
        self.delete_session_inner(id, None)
    }

    pub fn delete_session_checked(
        &self,
        id: &str,
        expected_ids: &[String],
    ) -> Result<bool, OperationError> {
        self.delete_session_inner(id, Some(expected_ids))
    }

    fn delete_session_inner(
        &self,
        id: &str,
        expected_ids: Option<&[String]>,
    ) -> Result<bool, OperationError> {
        self.transaction(|tx| {
            let exists: bool = tx
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM session WHERE id=?1)",
                    params![id],
                    |r| r.get(0),
                )
                ?;
            if !exists {
                return Ok(false);
            }
            if let Some(expected) = expected_ids {
                let mut statement=tx.prepare("WITH RECURSIVE descendants(id) AS (SELECT id FROM session WHERE id=?1 UNION SELECT s.id FROM session s JOIN descendants d ON s.parent_id=d.id) SELECT id FROM descendants ORDER BY id")?;
                let current = statement
                    .query_map(params![id], |r| r.get::<_, String>(0))
                    ?
                    .collect::<Result<Vec<_>, _>>()
                    ?;
                let mut expected = expected.to_vec();
                expected.sort();
                expected.dedup();
                if current != expected {
                    return Err(OperationError::busy("Session descendants changed during delete hooks; retry deletion"));
                }
            }
            // Check the inbox as well as busy: admission commits before the engine
            // publishes busy, and a delete must not erase acknowledged work.
            let active: bool = tx.query_row("WITH RECURSIVE descendants(id) AS (SELECT id FROM session WHERE id=?1 UNION SELECT s.id FROM session s JOIN descendants d ON s.parent_id=d.id) SELECT EXISTS(SELECT 1 FROM session WHERE id IN (SELECT id FROM descendants) AND busy=1) OR EXISTS(SELECT 1 FROM inbox WHERE session_id IN (SELECT id FROM descendants))", params![id], |row| row.get(0))?;
            if active {
                return Err(OperationError::busy("Session or a child session is busy; abort and wait for idle before deleting"));
            }
            tx.execute("WITH RECURSIVE descendants(id) AS (SELECT id FROM session WHERE id=?1 UNION SELECT s.id FROM session s JOIN descendants d ON s.parent_id=d.id) DELETE FROM session WHERE id IN (SELECT id FROM descendants)", params![id])?;
            Ok(true)
        })
    }

    pub fn children(&self, id: &str) -> Result<Vec<Session>, OperationError> {
        self.with_read(|c| {
            let mut stmt = c.prepare(
                "SELECT json FROM session WHERE parent_id=?1 ORDER BY time_updated DESC,rowid DESC",
            )?;
            let rows = stmt.query_map(params![id], |r| r.get::<_, String>(0))?;
            rows.map(|row| serde_json::from_str(&row?).map_err(data_error))
                .collect()
        })
    }

    #[cfg(test)]
    pub fn set_busy(&self, id: &str, busy: bool) -> Result<bool, OperationError> {
        let n = self.conn.lock().unwrap().execute(
            "UPDATE session SET busy=?2 WHERE id=?1",
            params![id, busy as i32],
        )?;
        Ok(n == 1)
    }

    pub fn is_busy(&self, id: &str) -> Result<bool, OperationError> {
        Ok(self
            .with_read(|c| {
                c.query_row("SELECT busy FROM session WHERE id=?1", params![id], |r| {
                    r.get::<_, i32>(0)
                })
                .optional()
                .map_err(OperationError::from)
            })?
            .unwrap_or(0)
            != 0)
    }

    pub fn busy_sessions(&self) -> Result<Vec<String>, OperationError> {
        self.with_read(|c| {
            let mut s = c.prepare("SELECT id FROM session WHERE busy=1 ORDER BY rowid")?;
            let rows = s.query_map([], |r| r.get(0))?;
            rows.collect::<Result<_, _>>().map_err(OperationError::from)
        })
    }

    pub fn add_message(&self, m: &MessageWithParts) -> Result<(), OperationError> {
        self.transaction(|tx| {
            self.check_claim(tx, &m.info.session_id)?;
            save_message(tx, m)?;
            Ok(())
        })
    }

    pub fn update_part(&self, session_id: &str, part: &Value) -> Result<(), OperationError> {
        self.transaction(|tx| {
            self.check_claim(tx, session_id)?;
            save_part(tx, session_id, part, now_ms())?;
            Ok(())
        })
    }

    pub fn messages(&self, session_id: &str) -> Result<Vec<MessageWithParts>, OperationError> {
        self.messages_limited(session_id, None)
    }

    /// Opt-in context pruning keeps the latest two user turns and 40k tokens
    /// of completed tool output, and marks older output only when at least
    /// another 20k tokens can be removed. Original output stays browsable.
    pub fn prune_tool_outputs(
        &self,
        sid: &str,
        through: &str,
    ) -> Result<Vec<Value>, OperationError> {
        crate::blocking::run(|| {
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
            self.transaction(|tx| {
                self.check_claim(tx, sid)?;
                for part in &parts {
                    save_part(tx, sid, part, now_ms())?;
                }
                Ok(())
            })?;
            Ok(parts)
        })
    }

    pub fn messages_limited(
        &self,
        session_id: &str,
        limit: Option<usize>,
    ) -> Result<Vec<MessageWithParts>, OperationError> {
        self.with_read(|c| {
            // The message and part queries must see the same window even if a
            // different process appends or deletes history between them.
            let tx = c.unchecked_transaction()?;
            let messages = read_messages(&tx, session_id, limit, None)?;
            tx.commit()?;
            Ok(messages)
        })
    }

    pub fn message(
        &self,
        session_id: &str,
        id: &str,
    ) -> Result<Option<MessageWithParts>, OperationError> {
        self.with_read(|c| read_message(c, session_id, id))
    }

    pub fn last_model(&self, sid: &str) -> Result<Option<ModelRef>, OperationError> {
        self.with_read(|c| {
            let mut stmt = c.prepare("SELECT json FROM message WHERE session_id=?1 ORDER BY time_created DESC,rowid DESC")?;
            let rows = stmt.query_map(params![sid], |row| row.get::<_, String>(0))?;
            for row in rows {
                let info: MessageInfo = serde_json::from_str(&row?).map_err(data_error)?;
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
    pub fn reply_to(
        &self,
        sid: &str,
        parent: &str,
    ) -> Result<Option<MessageWithParts>, OperationError> {
        self.with_read(|c| {
            let tx = c.unchecked_transaction()?;
            let id: Option<String> = tx.query_row("SELECT id FROM message WHERE session_id=?1 AND json_extract(json,'$.parentID')=?2 ORDER BY time_created DESC,rowid DESC LIMIT 1", params![sid, parent], |row| row.get(0)).optional()?;
            let message = id.map(|id| read_message(&tx, sid, &id)).transpose()?.flatten();
            tx.commit()?;
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
    ) -> Result<Vec<MessageWithParts>, OperationError> {
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
            let tx = c.unchecked_transaction()?;
            let boundary: Option<(i64, i64)> = tx.query_row(
                "SELECT time_created,rowid FROM message WHERE session_id=?1 AND id=?2",
                params![sid, through], |row| Ok((row.get(0)?, row.get(1)?)),
            ).optional()?;
            let mut users = HashMap::new();
            let mut eligible = |h: &Header| -> Result<bool, OperationError> {
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
                ).optional()?;
                let eligible = position.is_some_and(|position| boundary.is_none_or(|end| position <= end));
                users.insert(parent.clone(), eligible);
                Ok(eligible)
            };
            // Walk the session index backwards and stop at the newest usable
            // checkpoint. Archived headers need not be decoded on every round.
            let mut stmt = tx.prepare(&format!("SELECT {FIELDS} FROM message WHERE session_id=?1 ORDER BY time_created DESC,rowid DESC"))?;
            let rows = stmt.query_map(params![sid], header)?;
            let mut tail = Vec::new();
            let mut messages = Vec::new();
            for h in rows {
                let h = h?;
                if !eligible(&h)? { continue; }
                if h.summary {
                    let message = read_message(&tx, sid, &h.id)?.ok_or_else(|| OperationError::not_found("Message not found"))?;
                    if message.parts.iter().any(|part| part["type"] == "text" && part["text"].as_str().is_some_and(|text| !text.trim().is_empty())) {
                        let pending = message.parts.iter().find_map(|part| part["compactionPending"].as_str());
                        if let Some(pending) = pending {
                            let pending = tx.query_row(&format!("SELECT {FIELDS} FROM message WHERE session_id=?1 AND id=?2"), params![sid, pending], header).optional()?;
                            if let Some(pending) = pending.filter(|p| (p.created, p.ordinal) < (h.created, h.ordinal)) {
                                if eligible(&pending)? {
                                    messages.push(read_message(&tx, sid, &pending.id)?.ok_or_else(|| OperationError::not_found("Message not found"))?);
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
                messages.push(read_message(&tx, sid, &h.id)?.ok_or_else(|| OperationError::not_found("Message not found"))?);
            }
            tx.commit()?;
            Ok(messages)
        })
    }

    pub fn message_owner(&self, id: &str) -> Result<Option<String>, OperationError> {
        self.with_read(|c| {
            c.query_row(
                "SELECT session_id FROM message WHERE id=?1",
                params![id],
                |r| r.get(0),
            )
            .optional()
            .map_err(OperationError::from)
        })
    }

    /// The receipt, visible user message, and pending work commit together.
    pub fn admit(
        &self,
        request: &PromptRequest,
        message: &MessageWithParts,
    ) -> Result<bool, OperationError> {
        self.transaction(|tx| {
            let foreign_owner: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM owner WHERE session_id=?1 AND instance_id<>?2 AND lease_expires>=?3)",
            params![message.info.session_id, self.instance, now_ms()], |row| row.get(0),
        )?;
            if foreign_owner {
                return Err(OperationError::busy("Session is busy in another instance"));
            }
            let owner: Option<(String, String)> = tx
                .query_row(
                    "SELECT session_id,role FROM message WHERE id=?1",
                    params![message.info.id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()
                ?;
            if let Some((sid, role)) = owner {
                if sid == message.info.session_id && role == "user" {
                    return Ok(false);
                }
                return Err(OperationError::invalid("messageID already belongs to another session or message type"));
            }
            if let Some(token) = &request.admission_token {
                let valid:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM hook_admission WHERE message_id=?1 AND session_id=?2 AND token=?3 AND state='reserved' AND deadline>=?4)",params![message.info.id,message.info.session_id,token,now_ms()],|r|r.get(0))?;
                if !valid {
                    return Err("Prompt admission was aborted or expired".into());
                }
                tx.execute(
                    "UPDATE hook_admission SET state='accepted' WHERE message_id=?1 AND token=?2",
                    params![message.info.id, token],
                )
                ?;
            }
            save_message(tx, message)?;
            if !request.no_reply {
                tx.execute("INSERT INTO hook_turn(message_id,session_id,state,snapshot) VALUES(?1,?2,'queued',?3)",params![message.info.id,message.info.session_id,request.hook_snapshot.as_ref().map(Value::to_string)])?;
                tx.execute(
                    "INSERT INTO inbox(message_id,session_id,request) VALUES(?1,?2,?3)",
                    params![
                        message.info.id,
                        message.info.session_id,
                        serde_json::to_string(request).map_err(data_error)?
                    ],
                )
                ?;
            }
            let now = now_ms();
            tx.execute("UPDATE session SET time_updated=?2,json=json_set(json,'$.time.updated',?2) WHERE id=?1", params![message.info.session_id,now])?;
            Ok(true)
        })
    }

    pub fn next_prompt(&self, session_id: &str) -> Result<Option<PromptRequest>, OperationError> {
        self.transaction(|tx| {
            let next: Option<(i64,String)> = tx.query_row("SELECT seq,request FROM inbox WHERE session_id=?1 AND started=0 ORDER BY seq LIMIT 1", params![session_id], |r| Ok((r.get(0)?,r.get(1)?))).optional()?;
            let Some((seq, raw)) = next else {
                return Ok(None);
            };
            let request = serde_json::from_str(&raw).map_err(data_error)?;
            tx.execute("UPDATE inbox SET started=1 WHERE seq=?1", params![seq])
                ?;
            tx.execute("INSERT INTO hook_turn(message_id,session_id,state,started) SELECT message_id,session_id,'running',1 FROM inbox WHERE seq=?1 ON CONFLICT(message_id) DO UPDATE SET state='running',started=1",params![seq])?;
            Ok(Some(request))
        })
    }

    pub fn finish_prompt(&self, id: &str) -> Result<(), OperationError> {
        self.execute("DELETE FROM inbox WHERE message_id=?1", params![id])?;
        Ok(())
    }

    pub fn cancel_pending(&self, sid: &str) -> Result<(), OperationError> {
        self.transaction(|tx| {
            tx.execute("UPDATE part SET json=json_set(json,'$.ignored',json('true')) WHERE message_id IN (SELECT message_id FROM inbox WHERE session_id=?1 AND started=0)",params![sid])?;
            tx.execute("DELETE FROM inbox WHERE session_id=?1", params![sid])
                ?;
            Ok(())
        })
    }

    pub fn pending_sessions(&self) -> Result<Vec<String>, OperationError> {
        self.with_read(|c| {
            let mut s = c.prepare(&format!(
                "SELECT DISTINCT session_id FROM inbox WHERE started=0 AND {AVAILABLE_SCOPE}"
            ))?;
            let rows = s.query_map(params![self.instance, now_ms()], |r| r.get(0))?;
            rows.collect::<Result<_, _>>().map_err(OperationError::from)
        })
    }

    pub fn has_waiting(&self, sid: &str) -> Result<bool, OperationError> {
        self.with_read(|c| {
            c.query_row(
                "SELECT EXISTS(SELECT 1 FROM inbox WHERE session_id=?1 AND started=0)",
                params![sid],
                |r| r.get(0),
            )
            .map_err(OperationError::from)
        })
    }

    pub fn is_pending_message(&self, id: &str) -> Result<bool, OperationError> {
        self.with_read(|c| {
            c.query_row(
                "SELECT EXISTS(SELECT 1 FROM inbox WHERE message_id=?1)",
                params![id],
                |r| r.get(0),
            )
            .map_err(OperationError::from)
        })
    }

    pub fn set_todos(&self, sid: &str, todos: &Value) -> Result<(), OperationError> {
        if !todos.is_array() {
            return Err(OperationError::invalid("todos must be an array"));
        }
        self.transaction(|c| {
            let exists: bool = c
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM session WHERE id=?1)",
                    params![sid],
                    |row| row.get(0),
                )
                ?;
            if !exists {
                return Err(OperationError::not_found("Session not found"));
            }
            c.execute("INSERT INTO todo(session_id,json) VALUES(?1,?2) ON CONFLICT(session_id) DO UPDATE SET json=excluded.json", params![sid,todos.to_string()])?;
            Ok(())
        })
    }

    #[cfg(test)]
    pub fn todos(&self, sid: &str) -> Result<Value, OperationError> {
        let raw: Option<String> = self.with_read(|c| {
            c.query_row(
                "SELECT json FROM todo WHERE session_id=?1",
                params![sid],
                |r| r.get(0),
            )
            .optional()
            .map_err(OperationError::from)
        })?;
        Ok(raw
            .map(|s| serde_json::from_str(&s).map_err(data_error))
            .transpose()?
            .unwrap_or_else(|| json!([])))
    }

    pub fn try_fork(
        &self,
        source: &Session,
        up_to: Option<&str>,
    ) -> Result<Session, OperationError> {
        self.transaction(|tx| {
            let history = read_messages(tx, &source.id, None, up_to)?;
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
            tx.execute("INSERT INTO session(id,json,time_created,time_updated,parent_id,directory) VALUES(?1,?2,?3,?4,NULL,?5)", params![s.id,serde_json::to_string(&s).map_err(data_error)?,s.time.created,s.time.updated,scope_key(&s.directory)])?;
            for m in copied {
                save_message(tx, &m)?;
            }
            if let Some(raw) = tx
                .query_row(
                    "SELECT json FROM todo WHERE session_id=?1",
                    params![source.id],
                    |r| r.get::<_, String>(0),
                )
                .optional()
                ?
            {
                tx.execute(
                    "INSERT INTO todo(session_id,json) VALUES(?1,?2)",
                    params![s.id, raw],
                )
                ?;
            }
            Ok(s)
        })
    }
}

impl Store {
    fn with_write<T>(
        &self,
        work: impl FnOnce(&mut Connection) -> Result<T, OperationError>,
    ) -> Result<T, OperationError> {
        crate::blocking::run(|| work(&mut self.conn.lock().unwrap()))
    }

    fn execute(&self, sql: &str, params: impl rusqlite::Params) -> Result<usize, OperationError> {
        self.with_write(|connection| {
            connection
                .execute(sql, params)
                .map_err(OperationError::from)
        })
    }

    fn with_read<T>(
        &self,
        f: impl FnOnce(&Connection) -> Result<T, OperationError>,
    ) -> Result<T, OperationError> {
        crate::blocking::run(|| match &self.read {
            Some(read) => f(&read.lock().unwrap()),
            None => f(&self.conn.lock().unwrap()),
        })
    }
}

/// Interrupted rows in this instance's recovery scope, as `(id, json)` pairs.
/// `select` is a query prefix that ends with `AND `.
fn scan_interrupted(
    c: &Connection,
    instance: &str,
    now: i64,
    select: &str,
) -> Result<Vec<(String, String)>, OperationError> {
    let sql = format!("{select}{RECOVER_SCOPE}");
    let mut s = c.prepare(&sql)?;
    let rows = s.query_map(params![instance, now], |r| Ok((r.get(0)?, r.get(1)?)))?;
    rows.collect::<Result<_, _>>().map_err(OperationError::from)
}

fn scope_key(directory: &str) -> String {
    crate::config::directory_identity(Path::new(directory))
        .to_string_lossy()
        .into_owned()
}

fn add_column(c: &Connection, table: &str, column: &str, kind: &str) -> Result<(), OperationError> {
    let mut statement = c.prepare(&format!("PRAGMA table_info({table})"))?;
    for name in statement.query_map([], |row| row.get::<_, String>(1))? {
        if name? == column {
            return Ok(());
        }
    }
    c.execute(
        &format!("ALTER TABLE {table} ADD COLUMN {column} {kind}"),
        [],
    )?;
    Ok(())
}

fn open_connection(path: &Path) -> Result<Connection, OperationError> {
    let conn = Connection::open(path)?;
    conn.busy_timeout(BUSY_TIMEOUT)?;
    Ok(conn)
}

fn save_message(tx: &Transaction<'_>, m: &MessageWithParts) -> Result<(), OperationError> {
    let owner: Option<String> = tx
        .query_row(
            "SELECT session_id FROM message WHERE id=?1",
            params![m.info.id],
            |r| r.get(0),
        )
        .optional()?;
    if owner.is_some_and(|sid| sid != m.info.session_id) {
        return Err(OperationError::invalid(
            "Cannot move a message between sessions",
        ));
    }
    let exists: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM session WHERE id=?1)",
        params![m.info.session_id],
        |r| r.get(0),
    )?;
    if !exists {
        return Err(OperationError::not_found("Session not found"));
    }
    tx.execute("INSERT INTO message(id,session_id,role,json,time_created,completed) VALUES(?1,?2,?3,?4,?5,?6) ON CONFLICT(id) DO UPDATE SET json=excluded.json,role=excluded.role,completed=excluded.completed WHERE message.json<>excluded.json OR message.role IS NOT excluded.role", params![m.info.id,m.info.session_id,m.info.role,serde_json::to_string(&m.info).map_err(data_error)?,m.info.time.created,m.info.time.completed])?;
    for p in &m.parts {
        save_part(tx, &m.info.session_id, p, m.info.time.created)?;
    }
    Ok(())
}

fn save_part(
    tx: &Transaction<'_>,
    sid: &str,
    part: &Value,
    created: i64,
) -> Result<(), OperationError> {
    let id = part["id"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or("Part requires an id")?;
    let mid = part["messageID"]
        .as_str()
        .ok_or("Part requires a messageID")?;
    if part["sessionID"].as_str() != Some(sid) {
        return Err(OperationError::invalid("Part sessionID mismatch"));
    }
    let owner: Option<String> = tx
        .query_row(
            "SELECT session_id FROM message WHERE id=?1",
            params![mid],
            |r| r.get(0),
        )
        .optional()?;
    if owner.as_deref() != Some(sid) {
        return Err(OperationError::invalid(
            "Part's message does not belong to this session",
        ));
    }
    let old: Option<(String, String)> = tx
        .query_row(
            "SELECT session_id,message_id FROM part WHERE id=?1",
            params![id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    if old.is_some_and(|(s, m)| s != sid || m != mid) {
        return Err(OperationError::invalid(
            "Part id already belongs to another message",
        ));
    }
    // Recovery filters the mirrored kind/status columns, so keep them in step
    // with the JSON the row is saved with.
    let (kind, status) = (
        part["type"].as_str().map(str::to_string),
        part["state"]["status"].as_str().map(str::to_string),
    );
    tx.execute("INSERT INTO part(id,message_id,session_id,json,time_created,kind,status) VALUES(?1,?2,?3,?4,?5,?6,?7) ON CONFLICT(id) DO UPDATE SET json=excluded.json,kind=excluded.kind,status=excluded.status WHERE part.json<>excluded.json", params![id,mid,sid,part.to_string(),created,kind,status])?;
    Ok(())
}

fn read_message(
    c: &Connection,
    sid: &str,
    id: &str,
) -> Result<Option<MessageWithParts>, OperationError> {
    let raw: Option<String> = c
        .prepare_cached("SELECT json FROM message WHERE id=?1 AND session_id=?2")?
        .query_row(params![id, sid], |row| row.get(0))
        .optional()?;
    let Some(raw) = raw else {
        return Ok(None);
    };
    let mut stmt = c.prepare_cached(
        "SELECT json FROM part WHERE message_id=?1 AND session_id=?2 ORDER BY rowid",
    )?;
    let parts = stmt
        .query_map(params![id, sid], |row| row.get::<_, String>(0))?
        .map(|row| serde_json::from_str(&row?).map_err(data_error))
        .collect::<Result<_, _>>()?;
    Ok(Some(MessageWithParts {
        info: serde_json::from_str(&raw).map_err(data_error)?,
        parts,
    }))
}

fn read_messages(
    c: &Connection,
    sid: &str,
    limit: Option<usize>,
    before: Option<&str>,
) -> Result<Vec<MessageWithParts>, OperationError> {
    if let Some(id) = before {
        let exists: bool = c.query_row(
            "SELECT EXISTS(SELECT 1 FROM message WHERE id=?1 AND session_id=?2)",
            params![id, sid],
            |row| row.get(0),
        )?;
        if !exists {
            return Err(OperationError::not_found("Fork boundary message not found"));
        }
    }
    let selected = "WITH selected AS (
        SELECT rowid AS ordinal,id,json,time_created FROM message
        WHERE session_id=?1 AND (?3 IS NULL OR (time_created,rowid)<(SELECT time_created,rowid FROM message WHERE id=?3 AND session_id=?1))
        ORDER BY time_created DESC,rowid DESC LIMIT ?2
    )";
    let message_sql = format!("{selected} SELECT json FROM selected ORDER BY time_created,ordinal");
    let part_sql = format!("{selected} SELECT message_id,json FROM part WHERE session_id=?1 AND message_id IN (SELECT id FROM selected) ORDER BY rowid");
    let limit = limit.map_or(-1, |n| i64::try_from(n).unwrap_or(i64::MAX));
    let mut ms = c.prepare(&message_sql)?;
    let rows = ms.query_map(params![sid, limit, before], |r| r.get::<_, String>(0))?;
    let mut out = Vec::new();
    for row in rows {
        out.push(MessageWithParts {
            info: serde_json::from_str(&row?).map_err(data_error)?,
            parts: Vec::new(),
        });
    }
    let indices: HashMap<String, usize> = out
        .iter()
        .enumerate()
        .map(|(i, m)| (m.info.id.clone(), i))
        .collect();
    let mut ps = c.prepare(&part_sql)?;
    let rows = ps.query_map(params![sid, limit, before], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    })?;
    for row in rows {
        let (mid, raw) = row?;
        if let Some(index) = indices.get(&mid) {
            out[*index]
                .parts
                .push(serde_json::from_str(&raw).map_err(data_error)?);
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
        let db = Store::open_recovered(Path::new(":memory:"), "inst-a").unwrap();
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
        let db = Store::open_recovered(Path::new(":memory:"), "inst-a").unwrap();
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
        let db = Store::open_recovered(Path::new(":memory:"), "inst-a").unwrap();
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
        let db = Store::open_recovered(Path::new(":memory:"), "inst-a").unwrap();
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
        let db = Store::open_recovered(Path::new(":memory:"), "inst-a").unwrap();
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
        let db = Store::open_recovered(Path::new(":memory:"), "test").unwrap();
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
        let db = Store::open_recovered(&path, "test").unwrap();
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
        let live = Store::open_recovered(&path, "instance-live").unwrap();
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
        let sibling = Store::open_recovered(&path, "instance-sibling").unwrap();
        assert!(sibling.is_busy(&busy.id).unwrap());
        assert!(sibling.pending_sessions().unwrap().is_empty());
        assert_eq!(live.pending_sessions().unwrap(), vec![busy.id.clone()]);
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
        let third = Store::open_recovered(&path, "instance-third").unwrap();
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
        let db = Store::open_recovered(&path, "before-restart").unwrap();
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

        let db = Store::open_recovered(&path, "after-restart").unwrap();
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
    fn recovery_scans_seek_the_indexes_on_unfinished_work() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("ocmini.db");
        let db = Store::open_recovered(&path, "planner").unwrap();
        let session = db.create_session_owned("planner", "/work");
        // Finished work is what a used store is mostly made of; scanning it on
        // every open is what makes startup slow, so it must stay out of the scan.
        for index in 0..50 {
            db.add_message(&MessageWithParts {
                info: MessageInfo {
                    id: format!("msg{index}"),
                    session_id: session.id.clone(),
                    role: "assistant".into(),
                    time: MessageTime { created: 1, completed: Some(2) },
                    ..Default::default()
                },
                parts: vec![json!({"id":format!("part{index}"),"messageID":format!("msg{index}"),"sessionID":session.id,"type":"tool","state":{"status":"completed","time":{"start":1,"end":2}}})],
            })
            .unwrap();
        }
        for (label, prefix, index) in [
            ("message", RECOVER_MESSAGES, "msg_unfinished"),
            ("part", RECOVER_PARTS, "part_unfinished"),
        ] {
            let plan = db
                .with_read(|c| {
                    let sql = format!("EXPLAIN QUERY PLAN {prefix}{RECOVER_SCOPE}");
                    let mut statement = c.prepare(&sql)?;
                    let plan = statement
                        .query_map(params!["planner", now_ms()], |row| row.get::<_, String>(3))?
                        .collect::<Result<Vec<_>, _>>()?
                        .join(" | ");
                    Ok(plan)
                })
                .unwrap();
            assert!(plan.contains(index), "{label} recovery plan: {plan}");
        }
    }

    #[test]
    fn migration_mirrors_recovery_columns_and_survives_a_corrupt_legacy_row() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("ocmini.db");
        let db = Store::open_recovered(&path, "before-migration").unwrap();
        let session = db.create_session_owned("legacy", "/work");
        for (id, completed, status) in
            [("done", Some(2), "completed"), ("corrupt", None, "running")]
        {
            db.add_message(&MessageWithParts {
                info: MessageInfo { id: id.into(), session_id: session.id.clone(), role: "assistant".into(), time: MessageTime { created: 1, completed }, ..Default::default() },
                parts: vec![json!({"id":format!("tool_{id}"),"messageID":id,"sessionID":session.id,"type":"tool","state":{"status":status,"time":{"start":1}}})],
            })
            .unwrap();
        }
        {
            let connection = db.conn.lock().unwrap();
            // A v2 store: no mirrored columns, no recovery indexes, and one
            // unreadable part row, which the old full-table scan decoded.
            for statement in [
                "DROP INDEX msg_unfinished",
                "DROP INDEX part_unfinished",
                "ALTER TABLE message DROP COLUMN completed",
                "ALTER TABLE part DROP COLUMN kind",
                "ALTER TABLE part DROP COLUMN status",
                "UPDATE part SET json='invalid' WHERE id='tool_corrupt'",
            ] {
                connection.execute(statement, []).unwrap();
            }
            connection.pragma_update(None, "user_version", 2).unwrap();
        }
        drop(db);

        // Opening recovers (the corrupt row is unreadable, so the old scan
        // failed the whole open), mirrors the fields and re-indexes them.
        let db = Store::open_recovered(&path, "after-migration").unwrap();
        let connection = db.conn.lock().unwrap();
        assert_eq!(
            connection
                .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            5
        );
        assert_eq!(
            connection
                .query_row("SELECT completed FROM message WHERE id='done'", [], |row| {
                    row.get::<_, i64>(0)
                })
                .unwrap(),
            2
        );
        for (id, mirrored) in [("done", Some(("tool", "completed"))), ("corrupt", None)] {
            assert_eq!(
                connection
                    .query_row(
                        "SELECT kind,status FROM part WHERE id=?1",
                        params![format!("tool_{id}")],
                        |row| Ok((
                            row.get::<_, Option<String>>(0)?,
                            row.get::<_, Option<String>>(1)?
                        )),
                    )
                    .unwrap(),
                mirrored
                    .map(|(kind, status)| (Some(kind.to_string()), Some(status.to_string())))
                    .unwrap_or((None, None)),
                "{id}"
            );
        }
        assert_eq!(
            connection
                .query_row("SELECT json FROM part WHERE id='tool_corrupt'", [], |row| {
                    row.get::<_, String>(0)
                })
                .unwrap(),
            "invalid",
            "recovery must not touch a row it cannot read"
        );
    }

    #[test]
    fn legacy_migration_removes_orphan_todos_and_preserves_session_todos() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("ocmini.db");
        let db = Store::open_recovered(&path, "migration").unwrap();
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
        let db = Store::open_recovered(&path, "migration").unwrap();
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
            5
        );
    }

    #[test]
    fn session_listing_reports_corrupt_records() {
        let store = Store::open_recovered(Path::new(":memory:"), "test").unwrap();
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
        let store = Store::open_recovered(Path::new(":memory:"), "test").unwrap();
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
            store
                .list_sessions_filtered(None, false, Some(1), |_| true)
                .unwrap()[0]
                .id,
            newer.id
        );
        assert!(store
            .list_sessions_filtered(None, false, Some(0), |_| true)
            .unwrap()
            .is_empty());
        assert!(store.list_sessions(None).is_err());
    }

    #[test]
    fn context_uses_only_a_valid_summary_and_excludes_queued_prompt_descendants() {
        let db = Store::open_recovered(Path::new(":memory:"), "test").unwrap();
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
        let db = Store::open_recovered(Path::new(":memory:"), "test").unwrap();
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
        let db = Store::open_recovered(Path::new(":memory:"), "test").unwrap();
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

    #[test]
    fn live_leases_cannot_be_stolen_and_stale_releases_cannot_clear_busy() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("leases.db");
        let first = Store::open_recovered(&path, "first").unwrap();
        let session = first.create_session_owned("lease", "/work");
        first.claim(&session.id).unwrap();
        let second = Store::open_recovered(&path, "second").unwrap();
        assert!(second.claim(&session.id).unwrap_err().contains("busy"));
        second.release(&session.id);
        assert!(first.is_busy(&session.id).unwrap());
        assert!(second
            .admit(&PromptRequest::default(), &user(&session, "foreign", "no"))
            .unwrap_err()
            .contains("busy"));
        assert!(first.message(&session.id, "foreign").unwrap().is_none());
        first
            .conn
            .lock()
            .unwrap()
            .execute("UPDATE owner SET lease_expires=0", [])
            .unwrap();
        second.claim(&session.id).unwrap();
        first.release(&session.id);
        assert!(second.is_busy(&session.id).unwrap());
        second.release(&session.id);
        assert!(!second.is_busy(&session.id).unwrap());
    }
    #[test]
    fn expired_owners_cannot_renew_write_or_release_a_reclaimed_session() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("lease.db");
        let first = Store::open_recovered(&path, "first").unwrap();
        let session = first.create_session_owned("lease", "/workspace");
        first.claim(&session.id).unwrap();
        let message = user(&session, "user", "prompt");
        first.add_message(&message).unwrap();
        first
            .conn
            .lock()
            .unwrap()
            .execute(
                "UPDATE owner SET lease_expires=0 WHERE session_id=?1",
                params![session.id],
            )
            .unwrap();
        let second = Store::open_recovered(&path, "second").unwrap();
        second.claim(&session.id).unwrap();
        assert!(!first.heartbeat(&session.id).unwrap());
        assert!(first
            .add_message(&message)
            .unwrap_err()
            .contains("ownership lost"));
        assert!(first.update_part(&session.id, &json!({"id":"stale","sessionID":session.id,"messageID":"user","type":"text","text":"stale"})).unwrap_err().contains("ownership lost"));
        first.release(&session.id);
        assert!(second.is_busy(&session.id).unwrap());
        assert!(second.heartbeat(&session.id).unwrap());
        assert!(first.check_ownership(&session.id).is_err());
    }
    #[test]
    fn concurrent_session_fields_share_one_transactional_authority() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("mutations.db");
        let first = Store::open_recovered(&path, "first").unwrap();
        let session = first.create_session_owned("initial", "/workspace");
        let second = Store::open_recovered(&path, "second").unwrap();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                for _ in 0..20 {
                    first
                        .mutate_session(&session.id, |current| {
                            current.tokens.input += 1;
                            Ok(true)
                        })
                        .unwrap();
                }
            });
            scope.spawn(|| {
                for i in 0..20 {
                    second
                        .mutate_session(&session.id, |current| {
                            current.title = format!("title {i}");
                            Ok(true)
                        })
                        .unwrap();
                }
            });
        });
        let saved = first.get_session(&session.id).unwrap().unwrap();
        assert_eq!(saved.tokens.input, 20);
        assert_eq!(saved.title, "title 19");
    }
    #[test]
    fn scoped_listing_and_boundary_forks_never_decode_excluded_records() {
        let store = Store::open_recovered(Path::new(":memory:"), "scoped").unwrap();
        let session = store.create_session_owned("selected", "/selected");
        let other = store.create_session_owned("other", "/excluded");
        store
            .conn
            .lock()
            .unwrap()
            .execute(
                "UPDATE session SET json='invalid' WHERE id=?1",
                params![other.id],
            )
            .unwrap();
        assert_eq!(store.list_sessions(Some("/selected")).unwrap().len(), 1);
        for id in ["keep", "boundary", "excluded"] {
            store.add_message(&user(&session, id, id)).unwrap();
        }
        store
            .conn
            .lock()
            .unwrap()
            .execute(
                "UPDATE message SET json='invalid' WHERE id IN ('boundary','excluded')",
                [],
            )
            .unwrap();
        let fork = store.try_fork(&session, Some("boundary")).unwrap();
        let copied = store.messages(&fork.id).unwrap();
        assert_eq!(copied.len(), 1);
        assert_eq!(copied[0].parts[0]["text"], "keep");
    }
    #[test]
    fn contention_errors_keep_their_kind_without_matching_error_prose() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("busy.db");
        let store = Store::open(&path, "error-kind").unwrap();
        store
            .conn
            .lock()
            .unwrap()
            .busy_timeout(std::time::Duration::ZERO)
            .unwrap();
        let mut sibling = Connection::open(&path).unwrap();
        let transaction = sibling
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        let error = store
            .create_session(&Session::new("blocked", "/tmp"))
            .unwrap_err();
        assert_eq!(error.kind, crate::types::FailureKind::Busy);
        transaction.rollback().unwrap();
        let error = store
            .with_write(|connection| {
                connection.execute("SELECT busy FROM nonexistent_table", [])?;
                Ok(())
            })
            .unwrap_err();
        assert_eq!(error.kind, crate::types::FailureKind::Error);
    }
}
