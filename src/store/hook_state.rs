use super::*;
#[cfg(test)]
use crate::events as event;
use crate::types::TurnResult;

pub enum AdmissionReservation {
    Acquired(String),
    Waiting,
    Existing,
    Rejected(String),
}

pub struct InterruptedTurn {
    pub message_id: String,
    pub session_id: String,
    pub state: String,
    pub reason: Option<String>,
    pub started: bool,
    pub session: String,
    pub message: Option<String>,
    pub prepared: Option<String>,
    pub snapshot: Option<String>,
    pub request: Option<String>,
}
pub struct RecoveredTurn {
    pub source: InterruptedTurn,
    pub events: Vec<std::sync::Arc<crate::types::BusEvent>>,
    pub deliveries: Vec<OutboxIntent>,
}

impl Store {
    pub fn interrupted_turns(&self) -> Result<Vec<InterruptedTurn>, OperationError> {
        self.with_read(|connection| {
        let mut statement = connection.prepare("SELECT h.message_id,h.session_id,h.state,h.reason,h.started,s.json,m.json,p.json,h.snapshot,i.request FROM hook_turn h JOIN session s ON s.id=h.session_id LEFT JOIN message m ON m.id=h.message_id LEFT JOIN hook_pending_events p ON p.message_id=h.message_id LEFT JOIN inbox i ON i.message_id=h.message_id LEFT JOIN owner o ON o.session_id=h.session_id WHERE h.state IN ('running','finalizing') AND (o.session_id IS NULL OR o.lease_expires<?2)")?;
        let rows = statement
            .query_map(params![self.instance, now_ms()], |r| {
                Ok(InterruptedTurn {
                    message_id: r.get(0)?, session_id: r.get(1)?, state: r.get(2)?, reason: r.get(3)?,
                    started: r.get(4)?, session: r.get(5)?, message: r.get(6)?, prepared: r.get(7)?,
                    snapshot: r.get(8)?, request: r.get(9)?,
                })
            })
            ?;
            rows.map(|row| row.map_err(OperationError::from)).collect()
        })
    }

    pub fn reserve_hook_admission(
        &self,
        mid: &str,
        sid: &str,
        deadline: i64,
    ) -> Result<AdmissionReservation, OperationError> {
        self.transaction(|tx| {
            if let Some((owner, role)) = tx
                .query_row(
                    "SELECT session_id,role FROM message WHERE id=?1",
                    params![mid],
                    |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
                )
                .optional()
                ?
            {
                return if owner == sid && role == "user" {
                    Ok(AdmissionReservation::Existing)
                } else {
                    Err(OperationError::invalid("messageID already belongs to another session or message type"))
                };
            }
            if let Some((owner,state,error,expires))=tx.query_row("SELECT session_id,state,error,deadline FROM hook_admission WHERE message_id=?1",params![mid],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,Option<String>>(2)?,r.get::<_,i64>(3)?))).optional()? {
                if owner!=sid {return Err(OperationError::invalid("messageID already belongs to another session"));}
                if state=="rejected" {return Ok(AdmissionReservation::Rejected(error.unwrap_or_else(||"Prompt rejected".into())));}
                if state=="reserved" && expires<now_ms() {
                    let error="Prompt admission expired or was interrupted; use a new messageID";
                    tx.execute("UPDATE hook_admission SET state='rejected',error=?2 WHERE message_id=?1",params![mid,error])?;
                    return Ok(AdmissionReservation::Rejected(error.into()));
                }
                return Ok(AdmissionReservation::Waiting);
            }
            let token = crate::types::hex_id("adm_");
            tx.execute("INSERT INTO hook_admission(message_id,session_id,token,state,deadline) VALUES(?1,?2,?3,'reserved',?4)",params![mid,sid,token,deadline])?;
            Ok(AdmissionReservation::Acquired(token))
        })
    }

    pub fn hook_admission_ready(&self, mid: &str, token: &str) -> Result<bool, OperationError> {
        self.with_read(|c| {
            let (state,deadline,seq,sid):(String,i64,i64,String)=c.query_row("SELECT state,deadline,seq,session_id FROM hook_admission WHERE message_id=?1 AND token=?2",params![mid,token],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))?;
            if state!="reserved"||deadline<now_ms(){return Err("Prompt admission was aborted or expired".into());}
            let ahead:bool=c.query_row("SELECT EXISTS(SELECT 1 FROM hook_admission WHERE session_id=?1 AND state='reserved' AND seq<?2 AND deadline>=?3)",params![sid,seq,now_ms()],|r|r.get(0))?;
            Ok(!ahead)
        })
    }

    pub fn reject_hook_admission(
        &self,
        mid: &str,
        token: &str,
        error: &str,
    ) -> Result<(), OperationError> {
        self.execute("UPDATE hook_admission SET state='rejected',error=?3 WHERE message_id=?1 AND token=?2 AND state='reserved'",params![mid,token,error])?;
        Ok(())
    }

    pub fn cancel_hook_admissions(&self, sid: &str) -> Result<(), OperationError> {
        self.execute("UPDATE hook_admission SET state='rejected',error='Aborted' WHERE session_id=?1 AND state='reserved'",params![sid])?;
        Ok(())
    }

    pub fn turn_snapshot(&self, mid: &str) -> Result<Option<Value>, OperationError> {
        self.with_read(|c| {
            let raw: Option<String> = c
                .query_row(
                    "SELECT snapshot FROM hook_turn WHERE message_id=?1",
                    params![mid],
                    |r| r.get(0),
                )
                .optional()?
                .flatten();
            raw.map(|s| serde_json::from_str(&s).map_err(data_error))
                .transpose()
        })
    }

    /// A state-only check for duplicates waiting on a sibling finalizer.
    pub fn hook_turn_finalizing(&self, mid: &str) -> Result<bool, OperationError> {
        self.with_read(|c| {
            c.query_row(
                "SELECT state='finalizing' FROM hook_turn WHERE message_id=?1",
                params![mid],
                |r| r.get(0),
            )
            .map_err(OperationError::from)
        })
    }

    /// Only the exact response ID/error is duplicated, never a full transcript.
    pub fn turn_result(&self, mid: &str) -> Result<Option<(TurnResult, bool)>, OperationError> {
        let row=self.with_read(|c|c.query_row("SELECT session_id,result,state FROM hook_turn WHERE message_id=?1 AND result IS NOT NULL",params![mid],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?))).optional().map_err(OperationError::from))?;
        let Some((sid, raw, state)) = row else {
            return Ok(None);
        };
        let result: Value = serde_json::from_str(&raw).map_err(data_error)?;
        let value = if let Some(id) = result["responseID"].as_str() {
            Ok(self
                .message(&sid, id)?
                .ok_or("Recorded response not found")?)
        } else {
            Err(result["error"]
                .as_str()
                .unwrap_or("Recorded turn failed")
                .into())
        };
        Ok(Some((value, state == "finalizing")))
    }

    pub fn settle_hook_turn(
        &self,
        mid: &str,
        sid: &str,
        value: &TurnResult,
        reason: &str,
        started: bool,
        events: &[std::sync::Arc<crate::types::BusEvent>],
    ) -> Result<(), OperationError> {
        self.transaction(|tx| {
            let result = match value {
                Ok(message) => json!({"responseID":message.info.id}),
                Err(error) => json!({"error":error.message}),
            };
            self.check_claim(tx, sid)?;
            tx.execute("INSERT INTO hook_turn(message_id,session_id,state,result,reason,started) VALUES(?1,?2,'finalizing',?3,?4,?5) ON CONFLICT(message_id) DO UPDATE SET state='finalizing',result=excluded.result,reason=excluded.reason,started=excluded.started",params![mid,sid,result.to_string(),reason,started])?;
            if !started {
                tx.execute("UPDATE part SET json=json_set(json,'$.ignored',json('true')) WHERE message_id=?1",params![mid])?;
            }
            tx.execute("DELETE FROM inbox WHERE message_id=?1", params![mid])
                ?;
            tx.execute("INSERT INTO hook_pending_events(message_id,session_id,json) VALUES(?1,?2,?3) ON CONFLICT(message_id) DO UPDATE SET json=excluded.json",params![mid,sid,serde_json::to_string(&events.iter().map(std::sync::Arc::as_ref).collect::<Vec<_>>()).map_err(data_error)?])?;
            Ok(())
        })
    }

    pub fn finalizer_started(&self, mid: &str) -> Result<(), OperationError> {
        self.execute(
            "UPDATE hook_turn SET finalizer='started' WHERE message_id=?1 AND state='finalizing'",
            params![mid],
        )?;
        Ok(())
    }

    pub fn finish_hook_turn(
        &self,
        mid: &str,
        diagnostic: Option<&str>,
        deliveries: &[OutboxIntent],
    ) -> Result<Vec<String>, OperationError> {
        self.transaction(|tx| {
            let sid: String = tx.query_row(
                "SELECT session_id FROM hook_turn WHERE message_id=?1",
                params![mid],
                |row| row.get(0),
            )?;
            self.check_claim(tx, &sid)?;
            tx.execute(
                "UPDATE hook_turn SET state='done',finalizer=?2 WHERE message_id=?1",
                params![mid, diagnostic.unwrap_or("completed")],
            )?;
            // Optional notification capacity cannot change a committed turn's
            // outcome. Successful enqueues share this exact domain transaction;
            // rejected deliveries are reported after commit by the caller.
            let failures = Self::persist_outbox_tx(tx, deliveries)?;
            tx.execute(
                "DELETE FROM hook_pending_events WHERE message_id=?1",
                params![mid],
            )?;
            Ok(failures)
        })
    }

    pub fn queued_hook_prompts(&self, sid: &str) -> Result<Vec<PromptRequest>, OperationError> {
        self.with_read(|c| {
            let mut s = c.prepare(
                "SELECT request FROM inbox WHERE session_id=?1 AND started=0 ORDER BY seq",
            )?;
            let rows = s.query_map(params![sid], |r| r.get::<_, String>(0))?;
            rows.map(|raw| serde_json::from_str(&raw?).map_err(data_error))
                .collect()
        })
    }

    pub fn append_hook_feedback(
        &self,
        mid: &str,
        assistant: &str,
        text: &str,
    ) -> Result<u32, OperationError> {
        self.transaction(|tx| {
            let raw: String = tx.query_row(
                "SELECT json FROM message WHERE id=?1 AND json_extract(json,'$.parentID')=?2",
                params![assistant, mid],
                |r| r.get(0),
            )?;
            let mut info: Value = serde_json::from_str(&raw).map_err(data_error)?;
            let feedback = info
                .as_object_mut()
                .ok_or("Invalid stored message")?
                .entry("ocminiHookFeedback")
                .or_insert_with(|| json!([]));
            feedback
                .as_array_mut()
                .ok_or("Invalid hook feedback")?
                .push(json!(text));
            tx.execute(
                "UPDATE message SET json=?2 WHERE id=?1",
                params![assistant, info.to_string()],
            )?;
            tx.execute(
                "UPDATE hook_turn SET continuations=continuations+1 WHERE message_id=?1",
                params![mid],
            )?;
            let count = tx.query_row(
                "SELECT continuations FROM hook_turn WHERE message_id=?1",
                params![mid],
                |r| r.get(0),
            )?;
            Ok(count)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn prepared(
        directory: &Path,
        path: &Path,
    ) -> (
        Store,
        Arc<crate::hooks::Hooks>,
        String,
        String,
        Vec<Arc<crate::types::BusEvent>>,
    ) {
        let store = Store::open_recovered(path, "before").unwrap();
        let dir = directory.to_str().unwrap();
        let cfg:crate::config::Config=serde_json::from_value(json!({"ocmini":{"hooks":{"observers":[{"id":"recovery","on":"ocmini.turn.ended","delivery":"durable","command":["true"]}]}}})).unwrap();
        let hooks = crate::hooks::Hooks::for_config(&cfg, dir).unwrap();
        let session = store.create_session_owned("recovery", dir);
        let mid = "msg_finalizer_recovery".to_owned();
        store
            .add_message(&MessageWithParts {
                info: MessageInfo {
                    id: mid.clone(),
                    session_id: session.id.clone(),
                    role: "user".into(),
                    agent: Some("build".into()),
                    ..Default::default()
                },
                parts: vec![],
            })
            .unwrap();
        let event = hooks.event(
            event::Turn::Ended {
                reason: "error",
                started: true,
                response_id: None,
                error: Some("original failure"),
                finalizer_error: None,
            },
            &json!({"sessionID":session.id,"turnID":mid,"turnKind":"primary"}),
        );
        let events = vec![event];
        store
            .settle_hook_turn(
                &mid,
                &session.id,
                &Err("original failure".into()),
                "error",
                true,
                &events,
            )
            .unwrap();
        store
            .conn
            .lock()
            .unwrap()
            .execute(
                "UPDATE hook_turn SET snapshot=?2 WHERE message_id=?1",
                params![mid, hooks.snapshot().to_string()],
            )
            .unwrap();
        (store, hooks, mid, session.id, events)
    }

    #[test]
    fn readonly_inspection_preserves_finalization_and_recovery_enqueues_its_snapshot() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("state.db");
        let (store, _hooks, mid, _sid, events) = prepared(root.path(), &path);
        assert!(
            Store::inspect_hook_deliveries(&path, root.path().to_str().unwrap())
                .unwrap()
                .is_empty()
        );
        assert!(
            store.turn_result(&mid).unwrap().unwrap().1,
            "debug inspection must not recover a turn"
        );
        drop(store);
        let recovered = Store::open_recovered(&path, "after").unwrap();
        assert!(!recovered.turn_result(&mid).unwrap().unwrap().1);
        let metadata = recovered
            .hook_deliveries(root.path().to_str().unwrap())
            .unwrap();
        assert_eq!(metadata.len(), 1);
        let key = metadata[0]["subscriptionKey"].as_str().unwrap();
        let claimed = recovered
            .claim_hook_delivery(key, 1000, 1)
            .unwrap()
            .unwrap();
        assert_eq!(claimed.event_id, events[0].id);
        assert_eq!(
            claimed.attempts, 1,
            "a one-attempt delivery must actually execute once"
        );
        let event: Value = serde_json::from_str(&claimed.event_json).unwrap();
        assert_eq!(event["properties"]["finalizerError"], "interrupted");
        assert_eq!(event["properties"]["error"], "original failure");
        let pending: i64 = recovered
            .conn
            .lock()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM hook_pending_events", [], |r| r.get(0))
            .unwrap();
        assert_eq!(pending, 0);
    }

    #[test]
    fn rollback_cannot_commit_outbox_without_domain_finalization() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("state.db");
        let (store, _hooks, mid, _sid, events) = prepared(root.path(), &path);
        store.conn.lock().unwrap().execute_batch("CREATE TRIGGER rollback_delivery AFTER INSERT ON hook_delivery BEGIN SELECT RAISE(ROLLBACK,'simulated storage failure'); END;").unwrap();
        assert!(store
            .finish_hook_turn(
                &mid,
                None,
                &crate::hooks::prepare_deliveries(&events).unwrap()
            )
            .is_err());
        assert!(store.turn_result(&mid).unwrap().unwrap().1);
        let pending: i64 = store
            .conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM hook_pending_events WHERE message_id=?1",
                params![mid],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(pending, 1, "rollback must preserve the recovery snapshot");
        assert!(store
            .hook_deliveries(root.path().to_str().unwrap())
            .unwrap()
            .is_empty());
        store
            .conn
            .lock()
            .unwrap()
            .execute_batch("DROP TRIGGER rollback_delivery")
            .unwrap();
        assert!(store
            .finish_hook_turn(
                &mid,
                None,
                &crate::hooks::prepare_deliveries(&events).unwrap()
            )
            .unwrap()
            .is_empty());
        assert!(!store.turn_result(&mid).unwrap().unwrap().1);
        assert_eq!(
            store
                .hook_deliveries(root.path().to_str().unwrap())
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn recovery_enqueues_another_directory_without_live_subscribers() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        let launch = root.path().join("launch");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::create_dir_all(&launch).unwrap();
        let path = root.path().join("state.db");
        let (store, hooks, mid, _, events) = prepared(&source, &path);
        // Simulate a restart where only the launch directory has observers.
        let _retired = crate::hooks::Hooks::for_config(
            &crate::config::Config::default(),
            source.to_str().unwrap(),
        )
        .unwrap();
        let _launch = crate::hooks::Hooks::for_config(
            &crate::config::Config::default(),
            launch.to_str().unwrap(),
        )
        .unwrap();
        drop(hooks);
        drop(store);
        let recovered = Store::open_recovered(&path, "other-directory").unwrap();
        assert!(!recovered.turn_result(&mid).unwrap().unwrap().1);
        let deliveries = recovered.hook_deliveries(source.to_str().unwrap()).unwrap();
        assert_eq!(deliveries.len(), 1);
        let delivery = recovered
            .claim_hook_delivery(deliveries[0]["subscriptionKey"].as_str().unwrap(), 1000, 1)
            .unwrap()
            .unwrap();
        assert_eq!(delivery.event_id, events[0].id);
        assert!(recovered
            .hook_deliveries(launch.to_str().unwrap())
            .unwrap()
            .is_empty());
    }

    #[test]
    fn recovery_rollback_preserves_pending_events_and_finalizing_state() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("state.db");
        let (store, _hooks, mid, _, _) = prepared(root.path(), &path);
        store.conn.lock().unwrap().execute_batch(
            "CREATE TRIGGER rollback_recovery AFTER INSERT ON hook_delivery BEGIN SELECT RAISE(ROLLBACK,'simulated recovery failure'); END;",
        ).unwrap();
        drop(store);
        assert!(Store::open_recovered(&path, "failed-recovery").is_err());
        let connection = Connection::open(&path).unwrap();
        let state: String = connection
            .query_row(
                "SELECT state FROM hook_turn WHERE message_id=?1",
                params![mid],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(state, "finalizing");
        let pending: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM hook_pending_events WHERE message_id=?1",
                params![mid],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(pending, 1);
        connection
            .execute_batch("DROP TRIGGER rollback_recovery")
            .unwrap();
        drop(connection);
        let recovered = Store::open_recovered(&path, "successful-recovery").unwrap();
        assert!(!recovered.turn_result(&mid).unwrap().unwrap().1);
        assert_eq!(
            recovered
                .hook_deliveries(root.path().to_str().unwrap())
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn recovered_running_turns_keep_model_and_compaction_match_metadata() {
        for command in [None, Some("__ocmini_compact")] {
            let root = tempfile::tempdir().unwrap();
            let path = root.path().join("state.db");
            let directory = root.path().to_str().unwrap();
            let kind = if command.is_some() {
                "compaction"
            } else {
                "primary"
            };
            let cfg: crate::config::Config = serde_json::from_value(json!({"ocmini":{"hooks":{"observers":[{
                "id":"filtered-recovery", "on":"ocmini.turn.ended", "delivery":"durable", "command":["true"],
                "match":{"model":"mock/test-model", "turnKind":kind, "agent":"build"}
            }]}}})).unwrap();
            let hooks = crate::hooks::Hooks::for_config(&cfg, directory).unwrap();
            let store = Store::open_recovered(&path, "running-before").unwrap();
            let session = store.create_session_owned("running", directory);
            let model = ModelRef {
                provider_id: "mock".into(),
                model_id: "test-model".into(),
            };
            let request = PromptRequest {
                message_id: Some("msg_running_recovery".into()),
                agent: Some("build".into()),
                model: Some(model.clone()),
                command: command.map(str::to_string),
                hook_snapshot: Some(hooks.snapshot()),
                ..Default::default()
            };
            let message = MessageWithParts {
                info: MessageInfo {
                    id: request.message_id.clone().unwrap(),
                    session_id: session.id.clone(),
                    role: "user".into(),
                    model: Some(model),
                    agent: Some("build".into()),
                    ..Default::default()
                },
                parts: vec![],
            };
            assert!(store.admit(&request, &message).unwrap());
            assert!(store.next_prompt(&session.id).unwrap().is_some());
            let _retired =
                crate::hooks::Hooks::for_config(&crate::config::Config::default(), directory)
                    .unwrap();
            drop(store);
            let recovered = Store::open_recovered(&path, "running-after").unwrap();
            let deliveries = recovered.hook_deliveries(directory).unwrap();
            assert_eq!(
                deliveries.len(),
                1,
                "the recovered {kind} event must match its subscription"
            );
            let delivery = recovered
                .claim_hook_delivery(deliveries[0]["subscriptionKey"].as_str().unwrap(), 1000, 1)
                .unwrap()
                .unwrap();
            let event: Value = serde_json::from_str(&delivery.event_json).unwrap();
            assert_eq!(event["properties"]["turnKind"], kind);
            assert_eq!(
                event["properties"]["model"],
                json!({"providerID":"mock","modelID":"test-model"})
            );
        }
    }
}
