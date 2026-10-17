use crate::types::{now_ms, MessageInfo, MessageWithParts, PromptRequest, Session, Time, Tokens};
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;

pub struct Store {
    conn: Mutex<Connection>,
}

fn sql_error(error: impl std::fmt::Display) -> String {
    format!("Session database: {error}")
}

impl Store {
    pub fn open(path: &Path) -> Self {
        Self::try_open(path).unwrap_or_else(|e| panic!("{e}"))
    }

    pub fn try_open(path: &Path) -> Result<Self, String> {
        if let Some(dir) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir).map_err(sql_error)?;
        }
        let conn = Connection::open(path).map_err(sql_error)?;
        conn.pragma_update(None, "journal_mode", "WAL").map_err(sql_error)?;
        conn.pragma_update(None, "synchronous", "NORMAL").map_err(sql_error)?;
        conn.busy_timeout(std::time::Duration::from_secs(5)).map_err(sql_error)?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS session(id TEXT PRIMARY KEY, json TEXT NOT NULL, time_created INTEGER, time_updated INTEGER, parent_id TEXT, busy INTEGER DEFAULT 0);
             CREATE TABLE IF NOT EXISTS message(id TEXT PRIMARY KEY, session_id TEXT NOT NULL, role TEXT, json TEXT NOT NULL, time_created INTEGER);
             CREATE TABLE IF NOT EXISTS part(id TEXT PRIMARY KEY, message_id TEXT NOT NULL, session_id TEXT NOT NULL, json TEXT NOT NULL, time_created INTEGER);
             CREATE INDEX IF NOT EXISTS msg_sid ON message(session_id, time_created);
             CREATE INDEX IF NOT EXISTS part_sid ON part(session_id, time_created);
             CREATE INDEX IF NOT EXISTS part_mid ON part(message_id);
             CREATE INDEX IF NOT EXISTS session_parent ON session(parent_id);
             CREATE TABLE IF NOT EXISTS inbox(seq INTEGER PRIMARY KEY AUTOINCREMENT, message_id TEXT NOT NULL UNIQUE, session_id TEXT NOT NULL, request TEXT NOT NULL, started INTEGER NOT NULL DEFAULT 0);
             CREATE INDEX IF NOT EXISTS inbox_session ON inbox(session_id, seq);
             CREATE TABLE IF NOT EXISTS todo(session_id TEXT PRIMARY KEY, json TEXT NOT NULL);
             CREATE TRIGGER IF NOT EXISTS session_delete_parts AFTER DELETE ON session BEGIN
               DELETE FROM part WHERE session_id=OLD.id;
               DELETE FROM message WHERE session_id=OLD.id;
               DELETE FROM inbox WHERE session_id=OLD.id;
               DELETE FROM todo WHERE session_id=OLD.id;
             END;
             CREATE TRIGGER IF NOT EXISTS message_delete_parts AFTER DELETE ON message BEGIN
               DELETE FROM part WHERE message_id=OLD.id;
             END;
             DELETE FROM part WHERE session_id NOT IN (SELECT id FROM session) OR message_id NOT IN (SELECT id FROM message);
             DELETE FROM message WHERE session_id NOT IN (SELECT id FROM session);
             DELETE FROM inbox WHERE session_id NOT IN (SELECT id FROM session);
             DELETE FROM todo WHERE session_id NOT IN (SELECT id FROM session);",
        ).map_err(sql_error)?;
        let store = Self { conn: Mutex::new(conn) };
        store.recover_interrupted()?;
        // Retention is opt-in: old thread history must remain resumable from T3.
        if let Ok(days) = std::env::var("OCMINI_RETENTION_DAYS") {
            let days: i64 = days.parse().map_err(|_| "OCMINI_RETENTION_DAYS must be a nonnegative integer")?;
            if days < 0 { return Err("OCMINI_RETENTION_DAYS must be nonnegative".into()); }
            if days > 0 { store.prune(now_ms().saturating_sub(days.saturating_mul(86_400_000)))?; }
        }
        Ok(store)
    }

    fn recover_interrupted(&self) -> Result<(), String> {
        let mut c = self.conn.lock().unwrap();
        let tx = c.transaction().map_err(sql_error)?;
        let unfinished: Vec<String> = {
            let mut s = tx.prepare("SELECT json FROM message WHERE role='assistant' AND json_extract(json,'$.time.completed') IS NULL").map_err(sql_error)?;
            let rows = s.query_map([], |r| r.get(0)).map_err(sql_error)?;
            rows.collect::<Result<_, _>>().map_err(sql_error)?
        };
        for raw in unfinished {
            let mut info: MessageInfo = serde_json::from_str(&raw).map_err(sql_error)?;
            if info.time.completed.is_some() { continue; }
            info.time.completed = Some(now_ms().max(info.time.created));
            info.error = Some(json!({"name":"MessageAbortedError","data":{"message":"Server stopped before this response completed; send a new prompt to continue."}}));
            tx.execute("UPDATE message SET json=?2 WHERE id=?1", params![info.id, serde_json::to_string(&info).map_err(sql_error)?]).map_err(sql_error)?;
        }
        let parts: Vec<(String, String)> = {
            let mut s = tx.prepare("SELECT id,json FROM part WHERE json_extract(json,'$.type')='tool' AND json_extract(json,'$.state.status') IN ('pending','running')").map_err(sql_error)?;
            let rows = s.query_map([], |r| Ok((r.get(0)?,r.get(1)?))).map_err(sql_error)?;
            rows.collect::<Result<_, _>>().map_err(sql_error)?
        };
        for (id, raw) in parts {
            let mut part: Value = serde_json::from_str(&raw).map_err(sql_error)?;
            part["state"]["status"] = json!("error");
            part["state"]["error"] = json!("Execution interrupted by server restart; the operation was not replayed.");
            part["state"]["time"]["end"] = json!(now_ms());
            tx.execute("UPDATE part SET json=?2 WHERE id=?1", params![id, part.to_string()]).map_err(sql_error)?;
        }
        // Never replay a possibly completed filesystem or shell side effect after a crash.
        tx.execute("DELETE FROM inbox WHERE started=1", []).map_err(sql_error)?;
        tx.execute("UPDATE session SET busy=0", []).map_err(sql_error)?;
        tx.commit().map_err(sql_error)
    }

    pub fn prune(&self, before: i64) -> Result<usize, String> {
        let c = self.conn.lock().unwrap();
        c.execute("DELETE FROM session WHERE busy=0 AND time_updated<?1 AND id NOT IN (SELECT session_id FROM inbox) AND id NOT IN (SELECT parent_id FROM session WHERE parent_id IS NOT NULL)", params![before]).map_err(sql_error)
    }

    pub fn create_session_owned(&self, title: &str, dir: &str) -> Session {
        self.try_create_session_owned(title, dir).unwrap_or_else(|e| panic!("{e}"))
    }

    pub fn try_create_session_owned(&self, title: &str, dir: &str) -> Result<Session, String> {
        let now = now_ms();
        let s = Session {
            id: crate::types::hex_id("ses_"), slug: crate::types::slug(), project_id: "global".into(),
            directory: dir.to_string(), path: dir.to_string(), cost: 0.0, tokens: Tokens::default(),
            title: title.to_string(), version: crate::OPENCODE_COMPAT_VERSION.into(),
            time: Time { created: now, updated: Some(now), ..Time::default() },
            parent_id: None, permission: None, extra: Default::default(),
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

    pub fn list_sessions(&self, dir: Option<&str>) -> Vec<Session> {
        let c = self.conn.lock().unwrap();
        let mut stmt = c.prepare("SELECT json FROM session ORDER BY time_updated DESC,rowid DESC").expect("session query");
        let rows = stmt.query_map([], |r| r.get::<_, String>(0)).expect("list sessions");
        rows.map(|r| serde_json::from_str::<Session>(&r.expect("session row")).expect("session JSON"))
            .filter(|s| dir.is_none_or(|d| Path::new(&s.directory) == Path::new(d))).collect()
    }

    pub fn get_session(&self, id: &str) -> Option<Session> {
        self.conn.lock().unwrap().query_row("SELECT json FROM session WHERE id=?1", params![id], |r| r.get::<_, String>(0))
            .optional().expect("get session").map(|j| serde_json::from_str(&j).expect("session JSON"))
    }

    pub fn update_session(&self, s: &Session) -> Result<(), String> {
        let n = self.conn.lock().unwrap().execute(
            "UPDATE session SET json=?2,time_updated=?3,parent_id=?4 WHERE id=?1",
            params![s.id, serde_json::to_string(s).map_err(sql_error)?, s.time.updated.unwrap_or_else(now_ms), s.parent_id],
        ).map_err(sql_error)?;
        if n == 0 { return Err("Session not found".into()); }
        Ok(())
    }

    pub fn delete_session(&self, id: &str) -> Result<bool, String> {
        let mut c = self.conn.lock().unwrap();
        let tx = c.transaction().map_err(sql_error)?;
        let exists: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM session WHERE id=?1)", params![id], |r| r.get(0)).map_err(sql_error)?;
        if !exists { return Ok(false); }
        // Check the inbox as well as busy: admission commits before the engine
        // publishes busy, and a delete must not erase acknowledged work.
        let active: bool = tx.query_row("WITH RECURSIVE descendants(id) AS (SELECT id FROM session WHERE id=?1 UNION SELECT s.id FROM session s JOIN descendants d ON s.parent_id=d.id) SELECT EXISTS(SELECT 1 FROM session WHERE id IN (SELECT id FROM descendants) AND busy=1) OR EXISTS(SELECT 1 FROM inbox WHERE session_id IN (SELECT id FROM descendants))", params![id], |row| row.get(0)).map_err(sql_error)?;
        if active { return Err("Session or a child session is busy; abort and wait for idle before deleting".into()); }
        tx.execute("WITH RECURSIVE descendants(id) AS (SELECT id FROM session WHERE id=?1 UNION SELECT s.id FROM session s JOIN descendants d ON s.parent_id=d.id) DELETE FROM session WHERE id IN (SELECT id FROM descendants)", params![id]).map_err(sql_error)?;
        tx.commit().map_err(sql_error)?;
        Ok(true)
    }

    pub fn children(&self, id: &str) -> Vec<Session> {
        self.list_sessions(None).into_iter().filter(|s| s.parent_id.as_deref() == Some(id)).collect()
    }

    pub fn set_busy(&self, id: &str, busy: bool) -> bool {
        self.conn.lock().unwrap().execute("UPDATE session SET busy=?2 WHERE id=?1", params![id,busy as i32]).expect("update busy") == 1
    }

    pub fn is_busy(&self, id: &str) -> bool {
        self.conn.lock().unwrap().query_row("SELECT busy FROM session WHERE id=?1", params![id], |r| r.get::<_, i32>(0)).optional().expect("read busy").unwrap_or(0) != 0
    }

    pub fn busy_sessions(&self) -> Vec<String> {
        let c = self.conn.lock().unwrap();
        let mut s = c.prepare("SELECT id FROM session WHERE busy=1 ORDER BY rowid").expect("busy query");
        let rows = s.query_map([], |r| r.get(0)).expect("busy rows");
        rows.collect::<Result<_, _>>().expect("busy row")
    }

    pub fn add_message(&self, m: &MessageWithParts) -> Result<(), String> {
        let mut c = self.conn.lock().unwrap();
        let tx = c.transaction().map_err(sql_error)?;
        save_message(&tx, m)?;
        tx.commit().map_err(sql_error)
    }

    pub fn update_part(&self, session_id: &str, part: &Value) -> Result<(), String> {
        let mut c = self.conn.lock().unwrap();
        let tx = c.transaction().map_err(sql_error)?;
        save_part(&tx, session_id, part, now_ms())?;
        tx.commit().map_err(sql_error)
    }

    pub fn messages(&self, session_id: &str) -> Vec<MessageWithParts> {
        let c = self.conn.lock().unwrap();
        read_messages(&c, session_id).expect("read message history")
    }

    pub fn message(&self, session_id: &str, id: &str) -> Option<MessageWithParts> {
        self.messages(session_id).into_iter().find(|m| m.info.id == id)
    }

    pub fn message_owner(&self, id: &str) -> Option<String> {
        self.conn.lock().unwrap().query_row("SELECT session_id FROM message WHERE id=?1", params![id], |r| r.get(0)).optional().expect("message owner")
    }

    /// The receipt, visible user message, and pending work commit together.
    pub fn admit(&self, request: &PromptRequest, message: &MessageWithParts) -> Result<bool, String> {
        let mut c = self.conn.lock().unwrap();
        let tx = c.transaction().map_err(sql_error)?;
        let owner: Option<(String,String)> = tx.query_row("SELECT session_id,role FROM message WHERE id=?1", params![message.info.id], |r| Ok((r.get(0)?,r.get(1)?))).optional().map_err(sql_error)?;
        if let Some((sid, role)) = owner {
            if sid == message.info.session_id && role == "user" { return Ok(false); }
            return Err("messageID already belongs to another session or message type".into());
        }
        save_message(&tx, message)?;
        if !request.no_reply {
            tx.execute("INSERT INTO inbox(message_id,session_id,request) VALUES(?1,?2,?3)", params![message.info.id,message.info.session_id,serde_json::to_string(request).map_err(sql_error)?]).map_err(sql_error)?;
        }
        let now = now_ms();
        tx.execute("UPDATE session SET time_updated=?2,json=json_set(json,'$.time.updated',?2) WHERE id=?1", params![message.info.session_id,now]).map_err(sql_error)?;
        tx.commit().map_err(sql_error)?;
        Ok(true)
    }

    pub fn next_prompt(&self, session_id: &str) -> Result<Option<PromptRequest>, String> {
        let mut c = self.conn.lock().unwrap();
        let tx = c.transaction().map_err(sql_error)?;
        let next: Option<(i64,String)> = tx.query_row("SELECT seq,request FROM inbox WHERE session_id=?1 AND started=0 ORDER BY seq LIMIT 1", params![session_id], |r| Ok((r.get(0)?,r.get(1)?))).optional().map_err(sql_error)?;
        let Some((seq,raw)) = next else { return Ok(None); };
        let request = serde_json::from_str(&raw).map_err(sql_error)?;
        tx.execute("UPDATE inbox SET started=1 WHERE seq=?1", params![seq]).map_err(sql_error)?;
        tx.commit().map_err(sql_error)?;
        Ok(Some(request))
    }

    pub fn finish_prompt(&self, id: &str) -> Result<(), String> {
        self.conn.lock().unwrap().execute("DELETE FROM inbox WHERE message_id=?1", params![id]).map_err(sql_error)?;
        Ok(())
    }

    pub fn cancel_pending(&self, sid: &str) -> Result<(), String> {
        let mut c=self.conn.lock().unwrap();
        let tx=c.transaction().map_err(sql_error)?;
        tx.execute("UPDATE part SET json=json_set(json,'$.ignored',json('true')) WHERE message_id IN (SELECT message_id FROM inbox WHERE session_id=?1 AND started=0)",params![sid]).map_err(sql_error)?;
        tx.execute("DELETE FROM inbox WHERE session_id=?1", params![sid]).map_err(sql_error)?;
        tx.commit().map_err(sql_error)
    }

    pub fn pending_sessions(&self) -> Vec<String> {
        let c = self.conn.lock().unwrap();
        let mut s = c.prepare("SELECT DISTINCT session_id FROM inbox WHERE started=0").expect("inbox sessions");
        let rows = s.query_map([], |r| r.get(0)).expect("inbox rows");
        rows.collect::<Result<_, _>>().expect("inbox row")
    }

    pub fn has_pending(&self, sid: &str) -> bool {
        self.conn.lock().unwrap().query_row("SELECT EXISTS(SELECT 1 FROM inbox WHERE session_id=?1)", params![sid], |r| r.get(0)).expect("pending check")
    }

    pub fn has_waiting(&self, sid: &str) -> bool {
        self.conn.lock().unwrap().query_row("SELECT EXISTS(SELECT 1 FROM inbox WHERE session_id=?1 AND started=0)", params![sid], |r| r.get(0)).expect("waiting check")
    }

    pub fn is_pending_message(&self, id: &str) -> bool {
        self.conn.lock().unwrap().query_row("SELECT EXISTS(SELECT 1 FROM inbox WHERE message_id=?1)", params![id], |r| r.get(0)).expect("pending message")
    }

    pub fn set_todos(&self, sid: &str, todos: &Value) -> Result<(), String> {
        if !todos.is_array() { return Err("todos must be an array".into()); }
        let c = self.conn.lock().unwrap();
        let exists: bool = c.query_row("SELECT EXISTS(SELECT 1 FROM session WHERE id=?1)", params![sid], |row| row.get(0)).map_err(sql_error)?;
        if !exists { return Err("Session not found".into()); }
        c.execute("INSERT INTO todo(session_id,json) VALUES(?1,?2) ON CONFLICT(session_id) DO UPDATE SET json=excluded.json", params![sid,todos.to_string()]).map_err(sql_error)?;
        Ok(())
    }

    pub fn todos(&self, sid: &str) -> Value {
        let raw: Option<String> = self.conn.lock().unwrap().query_row("SELECT json FROM todo WHERE session_id=?1", params![sid], |r| r.get(0)).optional().expect("read todos");
        raw.map(|s| serde_json::from_str(&s).expect("todo JSON")).unwrap_or_else(|| json!([]))
    }

    pub fn fork(&self, source: &Session, up_to: Option<&str>) -> Session {
        self.try_fork(source, up_to).unwrap_or_else(|e| panic!("{e}"))
    }

    pub fn try_fork(&self, source: &Session, up_to: Option<&str>) -> Result<Session, String> {
        let mut c = self.conn.lock().unwrap();
        let tx = c.transaction().map_err(sql_error)?;
        let history = read_messages(&tx, &source.id)?;
        if up_to.is_some_and(|id| !history.iter().any(|m| m.info.id == id)) { return Err("Fork boundary message not found".into()); }
        let mut s = source.clone();
        s.id = crate::types::hex_id("ses_");
        s.slug = crate::types::slug();
        s.title = format!("{} (fork)", source.title);
        s.time = Time { created: now_ms(), updated: Some(now_ms()), ..Time::default() };
        s.parent_id = None;
        s.cost = 0.0;
        s.tokens = Tokens::default();
        s.extra.remove("revert");
        s.extra.remove("share");
        let mut ids = HashMap::new();
        let mut copied = Vec::new();
        for mut m in history {
            // OpenCode's boundary is the first EXCLUDED message, as used by T3 rewind.
            if up_to == Some(m.info.id.as_str()) { break; }
            let new_id = crate::types::hex_id("msg_");
            ids.insert(m.info.id.clone(), new_id.clone());
            m.info.id = new_id.clone();
            m.info.session_id = s.id.clone();
            m.info.parent_id = m.info.parent_id.as_ref().and_then(|id| ids.get(id).cloned());
            if let Some(tokens) = &m.info.tokens {
                s.tokens.input += tokens.input; s.tokens.output += tokens.output; s.tokens.reasoning += tokens.reasoning;
                s.tokens.cache.read += tokens.cache.read; s.tokens.cache.write += tokens.cache.write;
            }
            s.cost += m.info.cost.unwrap_or(0.0);
            for part in &mut m.parts {
                part["id"] = json!(crate::types::hex_id("prt_"));
                part["messageID"] = json!(new_id);
                part["sessionID"] = json!(s.id);
                // A copied task cannot point at a live child owned by the source thread.
                if part["type"] == "tool" && part["tool"] == "task" {
                    if let Some(meta) = part["state"]["metadata"].as_object_mut() { meta.remove("sessionId"); meta.remove("sessionID"); }
                }
            }
            copied.push(m);
        }
        tx.execute("INSERT INTO session(id,json,time_created,time_updated,parent_id) VALUES(?1,?2,?3,?4,NULL)", params![s.id,serde_json::to_string(&s).map_err(sql_error)?,s.time.created,s.time.updated]).map_err(sql_error)?;
        for m in copied { save_message(&tx, &m)?; }
        if let Some(raw) = tx.query_row("SELECT json FROM todo WHERE session_id=?1", params![source.id], |r| r.get::<_, String>(0)).optional().map_err(sql_error)? {
            tx.execute("INSERT INTO todo(session_id,json) VALUES(?1,?2)", params![s.id,raw]).map_err(sql_error)?;
        }
        tx.commit().map_err(sql_error)?;
        Ok(s)
    }
}

fn save_message(tx: &Transaction<'_>, m: &MessageWithParts) -> Result<(), String> {
    let owner: Option<String> = tx.query_row("SELECT session_id FROM message WHERE id=?1", params![m.info.id], |r| r.get(0)).optional().map_err(sql_error)?;
    if owner.is_some_and(|sid| sid != m.info.session_id) { return Err("Cannot move a message between sessions".into()); }
    let exists: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM session WHERE id=?1)", params![m.info.session_id], |r| r.get(0)).map_err(sql_error)?;
    if !exists { return Err("Session not found".into()); }
    tx.execute("INSERT INTO message(id,session_id,role,json,time_created) VALUES(?1,?2,?3,?4,?5) ON CONFLICT(id) DO UPDATE SET json=excluded.json,role=excluded.role", params![m.info.id,m.info.session_id,m.info.role,serde_json::to_string(&m.info).map_err(sql_error)?,m.info.time.created]).map_err(sql_error)?;
    for p in &m.parts { save_part(tx, &m.info.session_id, p, m.info.time.created)?; }
    Ok(())
}

fn save_part(tx: &Transaction<'_>, sid: &str, part: &Value, created: i64) -> Result<(), String> {
    let id = part["id"].as_str().filter(|s| !s.is_empty()).ok_or("Part requires an id")?;
    let mid = part["messageID"].as_str().ok_or("Part requires a messageID")?;
    if part["sessionID"].as_str() != Some(sid) { return Err("Part sessionID mismatch".into()); }
    let owner: Option<String> = tx.query_row("SELECT session_id FROM message WHERE id=?1", params![mid], |r| r.get(0)).optional().map_err(sql_error)?;
    if owner.as_deref() != Some(sid) { return Err("Part's message does not belong to this session".into()); }
    let old: Option<(String,String)> = tx.query_row("SELECT session_id,message_id FROM part WHERE id=?1", params![id], |r| Ok((r.get(0)?,r.get(1)?))).optional().map_err(sql_error)?;
    if old.is_some_and(|(s,m)| s != sid || m != mid) { return Err("Part id already belongs to another message".into()); }
    tx.execute("INSERT INTO part(id,message_id,session_id,json,time_created) VALUES(?1,?2,?3,?4,?5) ON CONFLICT(id) DO UPDATE SET json=excluded.json", params![id,mid,sid,part.to_string(),created]).map_err(sql_error)?;
    Ok(())
}

fn read_messages(c: &Connection, sid: &str) -> Result<Vec<MessageWithParts>, String> {
    let mut ms = c.prepare("SELECT json FROM message WHERE session_id=?1 ORDER BY time_created,rowid").map_err(sql_error)?;
    let rows = ms.query_map(params![sid], |r| r.get::<_, String>(0)).map_err(sql_error)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(MessageWithParts { info: serde_json::from_str(&row.map_err(sql_error)?).map_err(sql_error)?, parts: Vec::new() });
    }
    let indices: HashMap<String,usize> = out.iter().enumerate().map(|(i,m)| (m.info.id.clone(),i)).collect();
    let mut ps = c.prepare("SELECT message_id,json FROM part WHERE session_id=?1 ORDER BY rowid").map_err(sql_error)?;
    let rows = ps.query_map(params![sid], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))).map_err(sql_error)?;
    for row in rows {
        let (mid,raw) = row.map_err(sql_error)?;
        if let Some(index) = indices.get(&mid) { out[*index].parts.push(serde_json::from_str(&raw).map_err(sql_error)?); }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::MessageTime;

    fn user(s: &Session, id: &str, text: &str) -> MessageWithParts {
        MessageWithParts { info: MessageInfo { id:id.into(),session_id:s.id.clone(),role:"user".into(),time:MessageTime{created:123,completed:None},..Default::default() }, parts:vec![json!({"id":format!("part_{id}"),"messageID":id,"sessionID":s.id,"type":"text","text":text})] }
    }

    #[test]
    fn fork_excludes_boundary_and_never_overwrites_source() {
        let db=Store::open(Path::new(":memory:"));
        let s=db.create_session_owned("test","/work");
        let a=user(&s,"msg_a","one"); let b=user(&s,"msg_b","two");
        db.add_message(&a).unwrap(); db.add_message(&b).unwrap();
        let fork=db.try_fork(&s,Some("msg_b")).unwrap();
        let history=db.messages(&fork.id);
        assert_eq!(history.len(),1); assert_ne!(history[0].info.id,a.info.id);
        assert_eq!(history[0].parts[0]["sessionID"],fork.id);
        assert_eq!(history[0].parts[0]["messageID"],history[0].info.id);
        assert_eq!(db.messages(&s.id).len(),2); assert_eq!(db.messages(&s.id)[0].parts,a.parts);
        assert!(fork.parent_id.is_none());
    }

    #[test]
    fn admission_is_idempotent_and_scope_checked() {
        let db=Store::open(Path::new(":memory:")); let a=db.create_session_owned("a","/a"); let b=db.create_session_owned("b","/b");
        let m=user(&a,"msg_1","first"); let req=PromptRequest{message_id:Some(m.info.id.clone()),..Default::default()};
        assert!(db.admit(&req,&m).unwrap()); assert!(!db.admit(&req,&m).unwrap());
        assert!(db.admit(&req,&user(&b,"msg_1","wrong")).is_err());
        assert_eq!(db.messages(&a.id).len(),1); assert_eq!(db.next_prompt(&a.id).unwrap().unwrap().message_id,req.message_id);
        assert!(db.next_prompt(&a.id).unwrap().is_none());
    }

    #[test]
    fn deletion_cascades_and_directory_filter_is_exact() {
        let db=Store::open(Path::new(":memory:")); let a=db.create_session_owned("a","/app"); let b=db.create_session_owned("b","/apple");
        db.add_message(&user(&a,"msg_a","a")).unwrap(); db.set_todos(&a.id,&json!([{"content":"a"}])).unwrap();
        assert_eq!(db.list_sessions(Some("/app")).len(),1);
        assert!(db.delete_session(&a.id).unwrap()); assert!(db.messages(&a.id).is_empty()); assert_eq!(db.todos(&a.id),json!([]));
        assert!(db.get_session(&b.id).is_some());
    }

    #[test]
    fn deletion_cannot_erase_admitted_work_or_busy_children() {
        let db=Store::open(Path::new(":memory:"));
        let parent=db.create_session_owned("parent","/app");
        let mut child=db.create_session_owned("child","/app");
        child.parent_id=Some(parent.id.clone()); db.update_session(&child).unwrap();
        let message=user(&child,"queued","work");
        db.admit(&PromptRequest{message_id:Some("queued".into()),..Default::default()},&message).unwrap();
        assert!(db.delete_session(&parent.id).unwrap_err().contains("busy"));
        assert!(db.get_session(&child.id).is_some());
        db.cancel_pending(&child.id).unwrap(); db.set_busy(&child.id,true);
        assert!(db.delete_session(&parent.id).is_err());
        db.set_busy(&child.id,false);
        assert!(db.delete_session(&parent.id).unwrap());
        assert!(db.get_session(&child.id).is_none());
        assert!(db.set_todos(&child.id,&json!([])).is_err());
    }

    #[test]
    fn updating_streamed_parts_keeps_order() {
        let db=Store::open(Path::new(":memory:")); let s=db.create_session_owned("s","/work"); let mut m=user(&s,"m","hello");
        m.parts.push(json!({"id":"reason","messageID":"m","sessionID":s.id,"type":"reasoning","text":"thinking"}));
        db.add_message(&m).unwrap(); m.parts[0]["text"]=json!("hello world"); db.update_part(&s.id,&m.parts[0]).unwrap();
        assert_eq!(db.messages(&s.id)[0].parts,m.parts);
    }
}
