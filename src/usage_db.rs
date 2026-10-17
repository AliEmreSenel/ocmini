use crate::config::data_home;
use rusqlite::{params, Connection};
use std::sync::Mutex;

pub struct UsageDb {
    conn: Mutex<Option<Connection>>,
}

impl UsageDb {
    pub fn open() -> Self {
        let dir = format!("{}/opencode", data_home());
        std::fs::create_dir_all(&dir).ok();
        let conn = Connection::open(format!("{dir}/opencode-ocmini.db")).ok();
        if let Some(c) = &conn {
            c.pragma_update(None, "journal_mode", "WAL").ok();
            c.execute_batch("CREATE TABLE IF NOT EXISTS message(id TEXT PRIMARY KEY, session_id TEXT, data TEXT NOT NULL, time_created INTEGER NOT NULL);")
                .ok();
        }
        Self { conn: Mutex::new(conn) }
    }

    pub fn record(
        &self,
        msg_id: &str,
        session_id: &str,
        provider_id: &str,
        model_id: &str,
        tokens: &crate::types::Tokens,
        created: i64,
    ) {
        let guard = self.conn.lock().unwrap();
        let Some(c) = &*guard else { return };
        let data = serde_json::json!({
            "role": "assistant",
            "model": {"providerID": provider_id, "modelID": model_id},
            "tokens": {
                "input": tokens.input,
                "output": tokens.output,
                "reasoning": tokens.reasoning,
                "cache": {"read": tokens.cache.read, "write": tokens.cache.write}
            }
        });
        c.execute(
            "INSERT OR REPLACE INTO message(id, session_id, data, time_created) VALUES(?1,?2,?3,?4)",
            params![msg_id, session_id, data.to_string(), created],
        )
        .ok();
    }
}
