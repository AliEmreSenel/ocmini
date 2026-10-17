use crate::types::{MessageWithParts, Session, now_ms};
use rusqlite::{params, Connection};
use std::sync::Mutex;

pub struct Store {
    conn: Mutex<Connection>,
}

impl Store {
    pub fn open(path: &std::path::Path) -> Self {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).ok();
        }
        let conn = Connection::open(path).expect("open store");
        conn.pragma_update(None, "journal_mode", "WAL").ok();
        conn.pragma_update(None, "synchronous", "NORMAL").ok();
        conn.pragma_update(None, "busy_timeout", 5000).ok();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS session(id TEXT PRIMARY KEY, json TEXT NOT NULL, time_created INTEGER, time_updated INTEGER, parent_id TEXT, busy INTEGER DEFAULT 0);
             CREATE TABLE IF NOT EXISTS message(id TEXT PRIMARY KEY, session_id TEXT NOT NULL, role TEXT, json TEXT NOT NULL, time_created INTEGER);
             CREATE TABLE IF NOT EXISTS part(id TEXT PRIMARY KEY, message_id TEXT NOT NULL, session_id TEXT NOT NULL, json TEXT NOT NULL, time_created INTEGER);
             CREATE INDEX IF NOT EXISTS msg_sid ON message(session_id, time_created);
             CREATE INDEX IF NOT EXISTS part_sid ON part(session_id, time_created);",
        )
        .expect("schema");
        conn.execute("DELETE FROM session WHERE busy = 0 AND time_updated < ?1 - 30 * 86400000", params![now_ms()])
            .ok();
        Self { conn: Mutex::new(conn) }
    }

    pub fn create_session_owned(&self, title: &str, dir: &str) -> Session {
        let now = crate::types::now_ms();
        let s = Session {
            id: crate::types::hex_id("ses_"),
            slug: crate::types::slug(),
            project_id: "global".into(),
            directory: dir.to_string(),
            path: dir.trim_start_matches('/').to_string(),
            cost: 0.0,
            tokens: crate::types::Tokens::default(),
            title: title.to_string(),
            version: "1.19.2".into(),
            time: crate::types::Time { created: now, updated: Some(now) },
            parent_id: None,
            permission: None,
        };
        self.create_session(&s);
        s
    }

    pub fn create_session(&self, s: &Session) {        let c = self.conn.lock().unwrap();
        c.execute(
            "INSERT INTO session(id, json, time_created, time_updated, parent_id) VALUES(?1,?2,?3,?3,?4)",
            params![s.id, serde_json::to_string(s).unwrap(), s.time.created, s.parent_id],
        )
        .unwrap();
    }

    pub fn list_sessions(&self, dir: Option<&str>) -> Vec<Session> {
        let c = self.conn.lock().unwrap();
        let mut stmt = c
            .prepare("SELECT json FROM session ORDER BY time_created DESC LIMIT 200")
            .unwrap();
        let rows: Vec<Session> = stmt
            .query_map([], |r| {
                let j: String = r.get(0)?;
                Ok(serde_json::from_str::<Session>(&j))
            })
            .unwrap()
            .filter_map(|r| r.ok().and_then(|s| s.ok()))
            .collect();
        drop(stmt);
        match dir {
            Some(d) => rows
                .into_iter()
                .filter(|s| s.directory == d || s.directory.starts_with(d))
                .collect(),
            None => rows,
        }
    }

    pub fn get_session(&self, id: &str) -> Option<Session> {        let c = self.conn.lock().unwrap();
        c.query_row("SELECT json FROM session WHERE id=?1", params![id], |r| r.get::<_, String>(0))
            .ok()
            .and_then(|j| serde_json::from_str(&j).ok())
    }

    pub fn update_session(&self, s: &Session) {
        let c = self.conn.lock().unwrap();
        c.execute(
            "UPDATE session SET json=?2, time_updated=?3 WHERE id=?1",
            params![s.id, serde_json::to_string(s).unwrap(), now_ms()],
        )
        .unwrap();
    }

    pub fn delete_session(&self, id: &str) {
        let c = self.conn.lock().unwrap();
        c.execute("DELETE FROM session WHERE id=?1", params![id]).ok();
    }

    pub fn children(&self, id: &str) -> Vec<Session> {
        let c = self.conn.lock().unwrap();
        let mut st = c
            .prepare("SELECT json FROM session WHERE parent_id=?1 ORDER BY time_created")
            .unwrap();
        st.query_map(params![id], |r| r.get::<_, String>(0))
            .unwrap()
            .filter_map(|r| r.ok().and_then(|j| serde_json::from_str(&j).ok()))
            .collect()
    }

    pub fn set_busy(&self, id: &str, busy: bool) -> bool {
        let c = self.conn.lock().unwrap();
        c.execute(
            "UPDATE session SET busy=?2, time_updated=?3 WHERE id=?1",
            params![id, busy as i32, now_ms()],
        )
        .is_ok()
    }

    pub fn is_busy(&self, id: &str) -> bool {
        let c = self.conn.lock().unwrap();
        c.query_row("SELECT busy FROM session WHERE id=?1", params![id], |r| r.get::<_, i32>(0))
            .unwrap_or(0)
            != 0
    }

    pub fn busy_sessions(&self) -> Vec<String> {
        let c = self.conn.lock().unwrap();
        let mut st = c.prepare("SELECT id FROM session WHERE busy=1").unwrap();
        st.query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect()
    }

    pub fn add_message(&self, m: &MessageWithParts) {
        let c = self.conn.lock().unwrap();
        c.execute(
            "INSERT OR REPLACE INTO message(id, session_id, role, json, time_created) VALUES(?1,?2,?3,?4,?5)",
            params![m.info.id, m.info.session_id, m.info.role, serde_json::to_string(&m.info).unwrap(), m.info.time.created],
        )
        .unwrap();
        for p in &m.parts {
            c.execute(
                "INSERT OR REPLACE INTO part(id, message_id, session_id, json, time_created) VALUES(?1,?2,?3,?4,?5)",
                params![
                    p["id"].as_str().unwrap_or_default(),
                    m.info.id,
                    m.info.session_id,
                    p.to_string(),
                    m.info.time.created
                ],
            )
            .unwrap();
        }
    }

    pub fn update_part(&self, session_id: &str, part: &serde_json::Value) {
        let c = self.conn.lock().unwrap();
        c.execute(
            "INSERT OR REPLACE INTO part(id, message_id, session_id, json, time_created) VALUES(?1,?2,?3,?4,?5)",
            params![
                part["id"].as_str().unwrap_or_default(),
                part["messageID"].as_str().unwrap_or_default(),
                session_id,
                part.to_string(),
                now_ms()
            ],
        )
        .unwrap();
    }

    pub fn messages(&self, session_id: &str) -> Vec<MessageWithParts> {
        let c = self.conn.lock().unwrap();
        let mut ms = c
            .prepare("SELECT json FROM message WHERE session_id=?1 ORDER BY time_created")
            .unwrap();
        let infos: Vec<crate::types::MessageInfo> = ms
            .query_map(params![session_id], |r| r.get::<_, String>(0))
            .unwrap()
            .filter_map(|r| r.ok().and_then(|j| serde_json::from_str(&j).ok()))
            .collect();
        let mut ps = c.prepare("SELECT json FROM part WHERE session_id=?1 ORDER BY time_created").unwrap();
        let parts: Vec<serde_json::Value> = ps
            .query_map(params![session_id], |r| r.get::<_, String>(0))
            .unwrap()
            .filter_map(|r| r.ok().and_then(|j| serde_json::from_str(&j).ok()))
            .collect();
        infos
            .into_iter()
            .map(|info| {
                let parts = parts
                    .iter()
                    .filter(|p| p["messageID"].as_str() == Some(info.id.as_str()))
                    .cloned()
                    .collect();
                MessageWithParts { info, parts }
            })
            .collect()
    }

    pub fn fork(&self, src: &Session, up_to: Option<&str>) -> Session {
        let mut s = src.clone();
        s.id = crate::types::hex_id("ses_");
        s.slug = crate::types::slug();
        s.title = format!("{} (fork)", src.title);
        s.time = crate::types::Time { created: now_ms(), updated: Some(now_ms()) };
        s.parent_id = Some(src.id.clone());
        self.create_session(&s);
        for m in self.messages(&src.id) {
            if let Some(u) = up_to {
                if m.info.id == u {
                    self.add_message(&m);
                    break;
                }
            }
            self.add_message(&m);
        }
        s
    }
}
