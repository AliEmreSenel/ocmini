use super::*;

impl Store {
    pub fn v2_state(&self, sid: &str) -> Result<Value, OperationError> {
        self.with_read(|c| {
            let raw: Option<String> = c
                .query_row(
                    "SELECT json FROM v2_state WHERE session_id=?1",
                    [sid],
                    |r| r.get(0),
                )
                .optional()?;
            raw.map(|raw| serde_json::from_str(&raw).map_err(data_error))
                .transpose()
                .map(|v| v.unwrap_or_else(|| json!({})))
        })
    }
    pub fn v2_enabled(&self, sid: &str) -> Result<bool, OperationError> {
        self.with_read(|c| {
            c.prepare_cached("SELECT EXISTS(SELECT 1 FROM v2_state WHERE session_id=?1)")?
                .query_row([sid], |r| r.get(0))
                .map_err(Into::into)
        })
    }
    pub fn v2_update(&self, sid: &str, patch: &Value) -> Result<Value, OperationError> {
        self.transaction(|tx| update_state(tx, sid, patch))
    }
    pub fn v2_sequence(
        &self,
        sid: &str,
        next: i64,
        end: i64,
        reserve: i64,
    ) -> Result<(i64, i64), OperationError> {
        if next > 0 && next <= end {
            let current: i64 = self.with_read(|c| {
                c.prepare_cached("SELECT seq FROM v2_state WHERE session_id=?1")?
                    .query_row([sid], |r| r.get(0))
                    .map_err(OperationError::from)
            })?;
            if current == end {
                return Ok((next, end));
            }
        }
        self.transaction(|tx| {
            let end: i64 = tx
                .prepare_cached("UPDATE v2_state SET seq=seq+?2 WHERE session_id=?1 RETURNING seq")?
                .query_row(params![sid, reserve], |r| r.get(0))?;
            Ok((end - reserve + 1, end))
        })
    }
    pub fn v2_inbox(&self, sid: &str) -> Result<Vec<Value>, OperationError> {
        self.with_read(|c| {
            let mut q=c.prepare("SELECT i.request,m.time_created FROM inbox i JOIN message m ON m.id=i.message_id WHERE i.session_id=?1 AND i.started=0 ORDER BY i.seq")?;
            let rows=q.query_map([sid],|r|Ok((r.get::<_,String>(0)?,r.get::<_,i64>(1)?)))?;
            rows.map(|row| { let(raw,created)=row?; let request:PromptRequest=serde_json::from_str(&raw).map_err(data_error)?; Ok(crate::v2::wire::inbox(sid,&request,created)) }).collect()
        })
    }
    pub fn v2_cancel_input(&self, sid: &str, id: &str) -> Result<(), OperationError> {
        self.transaction(|tx| {
            let changed=tx.execute("DELETE FROM inbox WHERE session_id=?1 AND message_id=?2 AND started=0",params![sid,id])?;
            if changed==0 { return Err(OperationError::not_found("Pending inbox item not found")); }
            tx.execute("UPDATE message SET json=json_set(json,'$.ocminiV2Cancelled',json('true')) WHERE id=?1",[id])?;
            tx.execute("UPDATE hook_turn SET state='done',reason='cancelled' WHERE message_id=?1",[id])?;
            Ok(())
        })
    }
    pub fn has_steering(&self, sid: &str) -> Result<bool, OperationError> {
        self.with_read(|c| c.query_row("SELECT EXISTS(SELECT 1 FROM inbox WHERE session_id=?1 AND started=0 AND coalesce(json_extract(request,'$.v2.delivery'),'steer')='steer')",[sid],|r|r.get(0)).map_err(Into::into))
    }
    pub fn v2_transition(
        &self,
        sid: &str,
        patch: &Value,
        mut value: Value,
    ) -> Result<Value, OperationError> {
        let id = crate::types::hex_id("msg_");
        value["id"] = json!(id);
        value["time"] = json!({"created":now_ms()});
        let info = MessageInfo {
            id,
            session_id: sid.into(),
            role: "control".into(),
            time: crate::types::MessageTime {
                created: now_ms(),
                completed: Some(now_ms()),
            },
            extra: serde_json::Map::from_iter([("ocminiV2".into(), value.clone())]),
            ..Default::default()
        };
        self.transaction(|tx| {
            self.check_claim(tx, sid)?;
            update_state(tx, sid, patch)?;
            save_message(
                tx,
                &MessageWithParts {
                    info,
                    parts: vec![],
                },
            )
        })?;
        Ok(value)
    }
    pub fn v2_revert(&self, sid: &str, boundary: Option<&str>) -> Result<(), OperationError> {
        self.transaction(|tx| {
            self.check_claim(tx,sid)?;
            if let Some(id)=boundary {
                let row:i64=tx.query_row("SELECT rowid FROM message WHERE session_id=?1 AND id=?2",params![sid,id],|r|r.get(0)).optional()?.ok_or_else(||OperationError::not_found("Revert message not found"))?;
                tx.execute("DELETE FROM message WHERE session_id=?1 AND rowid>=?2",params![sid,row])?;
                tx.execute("DELETE FROM inbox WHERE session_id=?1 AND message_id NOT IN(SELECT id FROM message)",[sid])?;
            }
            tx.execute("UPDATE v2_state SET json=json_remove(json,'$.revert') WHERE session_id=?1",[sid])?;
            Ok(())
        })
    }
    pub fn v2_move(&self, sid: &str, directory: &str) -> Result<(), OperationError> {
        let mut session = self
            .get_session(sid)?
            .ok_or_else(|| OperationError::not_found("Session not found"))?;
        session.directory = directory.into();
        session.path = directory.into();
        self.update_session(&session)?;
        self.scopes.lock().unwrap().remove(sid);
        Ok(())
    }
    pub fn v2_instructions(&self, sid: &str) -> Result<String, OperationError> {
        let state = self.v2_state(sid)?;
        Ok(state["instructions"]
            .as_object()
            .map(|entries| {
                entries
                    .iter()
                    .map(|(k, v)| {
                        format!(
                            "\n\n[Instruction: {k}]\n{}",
                            v.as_str()
                                .map(str::to_owned)
                                .unwrap_or_else(|| v.to_string())
                        )
                    })
                    .collect::<String>()
            })
            .unwrap_or_default())
    }
    pub fn v2_page(
        &self,
        sid: &str,
        after: Option<i64>,
        descending: bool,
        limit: usize,
    ) -> Result<Vec<(i64, MessageWithParts)>, OperationError> {
        self.with_read(|c| {
            let direction=if descending {"DESC"} else {"ASC"};
            let compare=if descending {"<"} else {">"};
            let sql=format!("SELECT m.rowid,m.id FROM message m LEFT JOIN inbox i ON i.message_id=m.id AND i.started=0 WHERE m.session_id=?1 AND i.message_id IS NULL AND coalesce(json_extract(m.json,'$.ocminiV2Cancelled'),0)=0 AND (?2 IS NULL OR m.rowid {compare} ?2) ORDER BY m.rowid {direction} LIMIT ?3");
            let mut q=c.prepare(&sql)?;
            let ids=q.query_map(params![sid,after,limit as i64],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,String>(1)?)))?.collect::<Result<Vec<_>,_>>()?;
            ids.into_iter().map(|(seq,id)|read_message(c,sid,&id)?.map(|m|(seq,m)).ok_or_else(||OperationError::not_found("Message disappeared"))).collect()
        })
    }
}

impl Store {
    pub fn v2_instruction(
        &self,
        sid: &str,
        key: &str,
        value: Option<&Value>,
    ) -> Result<(), OperationError> {
        self.transaction(|tx| {
            let raw: String = tx.query_row(
                "SELECT json FROM v2_state WHERE session_id=?1",
                [sid],
                |r| r.get(0),
            )?;
            let mut state: Value = serde_json::from_str(&raw).map_err(data_error)?;
            if !state["instructions"].is_object() {
                state["instructions"] = json!({});
            }
            let entries = state["instructions"]
                .as_object_mut()
                .expect("instruction map");
            if let Some(value) = value {
                entries.insert(key.into(), value.clone());
            } else {
                entries.remove(key);
            }
            tx.execute(
                "UPDATE v2_state SET json=?2 WHERE session_id=?1",
                params![sid, state.to_string()],
            )?;
            Ok(())
        })
    }
}

impl Store {
    pub fn v2_sessions_page(
        &self,
        filter: &crate::v2::list::Filter,
        cursor: Option<&crate::v2::list::Cursor>,
        limit: usize,
    ) -> Result<Vec<(Session, Value)>, OperationError> {
        self.with_read(|c| {
            let ascending=filter.ascending ^ cursor.is_some_and(|c| c.previous);
            let (order,compare)=if ascending { ("ASC",">") } else { ("DESC","<") };
            let sql=format!("SELECT s.json,coalesce(v.json,'{{}}') FROM session s LEFT JOIN v2_state v ON v.session_id=s.id WHERE (?1 IS NULL OR s.directory=?1) AND (?2 IS NULL OR s.json->>'$.projectID'=?2) AND (?3 IS NULL OR s.json->>'$.title' LIKE '%'||?3||'%') AND (?4 IS NULL OR CASE WHEN ?4='null' THEN s.parent_id IS NULL ELSE s.parent_id=?4 END) AND (?5 IS NULL OR (s.json->>'$.time.archived' IS NOT NULL)=?5) AND (?6 IS NULL OR s.time_updated {compare} ?6 OR (s.time_updated=?6 AND s.id {compare} ?7)) ORDER BY s.time_updated {order},s.id {order} LIMIT ?8");
            let mut stmt=c.prepare(&sql)?;
            let rows=stmt.query_map(params![filter.directory,filter.project,filter.search,filter.parent,filter.archived,cursor.map(|c|c.time),cursor.map(|c|c.id.as_str()),limit.saturating_add(1).min(i64::MAX as usize) as i64], |r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?)))?;
            rows.map(|row| { let (session,state)=row?; Ok((serde_json::from_str(&session).map_err(data_error)?,serde_json::from_str(&state).map_err(data_error)?)) }).collect()
        })
    }
}

fn update_state(
    tx: &rusqlite::Transaction<'_>,
    sid: &str,
    patch: &Value,
) -> Result<Value, OperationError> {
    let raw: Option<String> = tx
        .query_row(
            "SELECT json FROM v2_state WHERE session_id=?1",
            [sid],
            |r| r.get(0),
        )
        .optional()?;
    let mut state: Value = raw
        .map(|raw| serde_json::from_str(&raw).map_err(data_error))
        .transpose()?
        .unwrap_or_else(|| json!({}));
    for (key, value) in patch.as_object().ok_or("Invalid session state")? {
        state[key] = value.clone();
    }
    if tx.execute("INSERT INTO v2_state(session_id,json) SELECT ?1,?2 FROM session WHERE id=?1 ON CONFLICT(session_id) DO UPDATE SET json=excluded.json",params![sid,state.to_string()])? == 0 { return Err(OperationError::not_found("Session not found")); }
    Ok(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sequence_ranges_reuse_reads_and_recover_after_sibling_writes_and_restart() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("state.db");
        let store = Store::open(&path, "first").unwrap();
        let s = Session::new("Sequences", "/tmp");
        store.create_session(&s).unwrap();
        store.v2_update(&s.id, &json!({})).unwrap();
        let (first, end) = store.v2_sequence(&s.id, 0, 0, 4096).unwrap();
        assert_eq!(first, 1);
        let before = store.conn.lock().unwrap().total_changes();
        for next in 2..100 {
            assert_eq!(
                store.v2_sequence(&s.id, next, end, 4096).unwrap(),
                (next, end)
            );
        }
        assert_eq!(
            store.conn.lock().unwrap().total_changes(),
            before,
            "cached IDs must not write SQLite"
        );
        let sibling = Store::open(&path, "sibling").unwrap();
        let (other, _) = sibling.v2_sequence(&s.id, 0, 0, 1).unwrap();
        assert!(other > end);
        let (next, new_end) = store.v2_sequence(&s.id, 100, end, 4096).unwrap();
        assert!(
            next > other,
            "another writer invalidates previously reserved IDs"
        );
        drop(store);
        let reopened = Store::open(&path, "restarted").unwrap();
        assert!(reopened.v2_sequence(&s.id, 0, 0, 1).unwrap().0 > new_end);
        let (first, end) = reopened.v2_sequence(&s.id, 0, 0, 2).unwrap();
        assert_eq!(
            reopened.v2_sequence(&s.id, first + 1, end, 2).unwrap().0,
            end
        );
        assert!(
            reopened.v2_sequence(&s.id, end + 1, end, 2).unwrap().0 > end,
            "range exhaustion must allocate, never drop work"
        );
    }
}
