use crate::config::data_home;
use crate::config::Config;
use crate::engine::App;
use crate::events as event;
use crate::types::{ModelRef, Tokens};
use rusqlite::{params, Connection};
use serde_json::json;
use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

/// T3's usage reader discovers this OpenCode-shaped usage mirror. Conversation
/// state remains in Store; mirror failure must not replay a completed operation.
pub struct UsageDb {
    conn: Mutex<Option<Connection>>,
}

impl UsageDb {
    pub fn open() -> Self {
        crate::blocking::run(|| {
            let path = Path::new(&data_home()).join("opencode/opencode-ocmini.db");
            let conn = match Self::connect(&path) {
                Ok(conn) => Some(conn),
                Err(error) => {
                    eprintln!(
                        "ocmini: usage statistics unavailable at {}: {error}",
                        path.display()
                    );
                    None
                }
            };
            Self {
                conn: Mutex::new(conn),
            }
        })
    }

    fn connect(path: &Path) -> Result<Connection, String> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let conn = Connection::open(path).map_err(|e| e.to_string())?;
        conn.busy_timeout(Duration::from_secs(5))
            .map_err(|e| e.to_string())?;
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(|e| e.to_string())?;
        conn.pragma_update(None, "synchronous", "NORMAL")
            .map_err(|e| e.to_string())?;
        conn.pragma_update(None, "journal_size_limit", 1024 * 1024)
            .map_err(|e| e.to_string())?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS message(id TEXT PRIMARY KEY, session_id TEXT, data TEXT NOT NULL, time_created INTEGER NOT NULL);
             CREATE INDEX IF NOT EXISTS message_time_created ON message(time_created);"
        ).map_err(|e| e.to_string())?;
        Ok(conn)
    }

    pub fn record(
        &self,
        msg_id: &str,
        session_id: &str,
        provider_id: &str,
        model_id: &str,
        tokens: &Tokens,
        created: i64,
    ) {
        crate::blocking::run(|| {
            let guard = self.conn.lock().unwrap();
            let Some(c) = &*guard else {
                return;
            };
            let data = serde_json::json!({
                "role": "assistant",
                "model": {"providerID": provider_id, "modelID": model_id},
                "tokens": tokens
            });
            if let Err(error) = c.execute(
            "INSERT INTO message(id, session_id, data, time_created) VALUES(?1,?2,?3,?4)
             ON CONFLICT(id) DO UPDATE SET session_id=excluded.session_id,data=excluded.data,time_created=excluded.time_created",
            params![msg_id, session_id, data.to_string(), created],
        ) {
            eprintln!("ocmini: could not record usage for {msg_id}: {error}");
        }
        })
    }
}

/// All paid requests, including titles and summaries, use the same sidecar and
/// session totals. The returned cost also belongs on the persisted model step.
pub fn record(
    app: &App,
    cfg: &Config,
    model: &ModelRef,
    sid: &str,
    request_id: &str,
    usage: &Tokens,
    created: i64,
) -> Result<f64, crate::types::OperationError> {
    let cost = cost_for(cfg, model, usage);
    app.usage.record(
        request_id,
        sid,
        &model.provider_id,
        &model.model_id,
        usage,
        created,
    );
    let Some(session) = app.store.mutate_session(sid, |session| {
        session.cost += cost;
        session.tokens.input += usage.input;
        session.tokens.output += usage.output;
        session.tokens.reasoning += usage.reasoning;
        session.tokens.cache.read += usage.cache.read;
        session.tokens.cache.write += usage.cache.write;
        Ok(true)
    })?
    else {
        return Ok(cost);
    };
    app.hooks.emit(
        event::Session::Updated { info: &session },
        &app.session_context(sid, json!({})),
    );
    Ok(cost)
}

pub fn cost_for(cfg: &Config, model: &ModelRef, usage: &Tokens) -> f64 {
    let Some(info) = cfg
        .provider(&model.provider_id)
        .and_then(|p| p.models.get(&model.model_id))
    else {
        return 0.0;
    };
    let rates = info.extra.get("cost").cloned().unwrap_or_default();
    let price = |name: &str| {
        rates[name]
            .as_f64()
            .filter(|n| n.is_finite() && *n >= 0.0)
            .unwrap_or(0.0)
    };
    (usage.input as f64 * price("input")
        + (usage.output + usage.reasoning) as f64 * price("output")
        + usage.cache.read as f64 * price("cache_read")
        + usage.cache.write as f64 * price("cache_write"))
        / 1_000_000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_mirror_uses_t3_schema_and_updates_without_duplicate_usage() {
        let conn = UsageDb::connect(Path::new(":memory:")).unwrap();
        let db = UsageDb {
            conn: Mutex::new(Some(conn)),
        };
        let mut tokens = Tokens {
            input: 10,
            output: 20,
            reasoning: 5,
            ..Default::default()
        };
        tokens.cache.read = 7;
        db.record("msg_1", "ses_1", "custom", "model", &tokens, 123);
        tokens.output = 25;
        db.record("msg_1", "ses_1", "custom", "model", &tokens, 123);
        let guard = db.conn.lock().unwrap();
        let conn = guard.as_ref().unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM message", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);
        let raw: String = conn
            .query_row("SELECT data FROM message WHERE id='msg_1'", [], |row| {
                row.get(0)
            })
            .unwrap();
        let data: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(data["model"]["providerID"], "custom");
        assert_eq!(data["tokens"]["output"], 25);
        assert_eq!(data["tokens"]["reasoning"], 5);
        assert_eq!(data["tokens"]["cache"]["read"], 7);
    }
}
