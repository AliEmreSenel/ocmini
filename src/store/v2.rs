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
    pub fn v2_sessions(&self) -> Result<Vec<String>, OperationError> {
        self.with_read(|c| {
            let mut q = c.prepare("SELECT session_id FROM v2_state")?;
            let rows = q.query_map([], |r| r.get(0))?;
            rows.collect::<Result<_, _>>().map_err(Into::into)
        })
    }
    pub fn v2_update(&self, sid: &str, patch: &Value) -> Result<Value, OperationError> {
        self.transaction(|tx| {
            let raw: Option<String> = tx.query_row("SELECT json FROM v2_state WHERE session_id=?1", [sid], |r| r.get(0)).optional()?;
            let mut state: Value = raw.map(|raw| serde_json::from_str(&raw).map_err(data_error)).transpose()?.unwrap_or_else(|| json!({}));
            for (key,value) in patch.as_object().ok_or("Invalid session state")? { state[key] = value.clone(); }
            tx.execute("INSERT INTO v2_state(session_id,json) VALUES(?1,?2) ON CONFLICT(session_id) DO UPDATE SET json=excluded.json",params![sid,state.to_string()])?;
            Ok(state)
        })
    }
    pub fn v2_sequence(&self, sid: &str) -> Result<i64, OperationError> {
        self.transaction(|tx| {
            tx.execute("UPDATE v2_state SET seq=seq+1 WHERE session_id=?1", [sid])?;
            tx.query_row("SELECT seq FROM v2_state WHERE session_id=?1", [sid], |r| {
                r.get(0)
            })
            .map_err(Into::into)
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
    pub fn v2_control(&self, sid: &str, mut value: Value) -> Result<Value, OperationError> {
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
        self.add_message(&MessageWithParts {
            info,
            parts: vec![],
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
