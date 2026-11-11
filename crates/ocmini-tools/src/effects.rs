//! Stateful file preparation belongs to the tool layer, in the process that
//! performs effects. Handles keep transaction locks across hooks and approvals.
use crate::{changes, types::OperationError};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc, Mutex,
};
use tokio_util::sync::CancellationToken;
pub enum EffectResult {
    Json(Value),
    Output(crate::types::ToolOutput),
}
impl EffectResult {
    pub fn json(self) -> Result<Value, OperationError> {
        match self {
            Self::Json(v) => Ok(v),
            Self::Output(out) => Ok(Value::Object(serde_json::Map::from_iter([
                ("output".into(), Value::String(out.output)),
                ("title".into(), Value::String(out.title)),
                ("metadata".into(), out.metadata),
            ]))),
        }
    }
    pub fn output(self) -> Result<crate::types::ToolOutput, OperationError> {
        match self {
            Self::Output(out) => Ok(out),
            Self::Json(v) => {
                serde_json::from_value(v).map_err(|e| OperationError::from(e.to_string()))
            }
        }
    }
    pub fn from_wire(method: &str, value: Value) -> Result<Self, OperationError> {
        if matches!(method, "run" | "commit" | "skill") {
            serde_json::from_value(value)
                .map(Self::Output)
                .map_err(|e| OperationError::from(e.to_string()))
        } else {
            Ok(Self::Json(value))
        }
    }
}
enum FileState {
    Reserved(changes::Reservation),
    Prepared(changes::Plan),
}
impl FileState {
    fn prepared(self) -> Result<changes::Plan, OperationError> {
        match self {
            Self::Prepared(p) => Ok(p),
            _ => Err("File plan is not prepared".into()),
        }
    }
}
struct RemotePlan {
    paths: Vec<std::path::PathBuf>,
    edit: Option<tokio::sync::OwnedMutexGuard<()>>,
}
#[derive(Default)]
pub struct Effects {
    next: AtomicU64,
    plans: Mutex<HashMap<(String, u64), FileState>>,
    remote: Mutex<HashMap<(String, u64), RemotePlan>>,
}
impl Effects {
    pub fn release_remote(&self, session: &str, handle: u64) {
        self.remote
            .lock()
            .unwrap()
            .remove(&(session.into(), handle));
    }
    pub async fn before_remote(
        &self,
        session: &str,
        method: &str,
        args: &Value,
        cancel: &CancellationToken,
    ) -> Result<Option<Box<dyn Send>>, OperationError> {
        if method != "commit" {
            return Ok(None);
        }
        let handle = args["handle"].as_u64().ok_or("Missing file plan")?;
        let plan = self
            .remote
            .lock()
            .unwrap()
            .remove(&(session.into(), handle))
            .ok_or("File plan no longer exists")?;
        let guards = match plan.edit {
            Some(guard) => vec![guard],
            None => changes::coordinate_commit(&plan.paths, cancel).await?,
        };
        Ok(Some(Box::new(guards)))
    }
    pub async fn after_remote(
        &self,
        session: &str,
        method: &str,
        args: &Value,
        value: &Value,
        cancel: &CancellationToken,
    ) -> Result<(), OperationError> {
        if !matches!(method, "paths" | "transform_paths") {
            return Ok(());
        }
        let handle = value["handle"]
            .as_u64()
            .or(args["handle"].as_u64())
            .ok_or("Missing file plan")?;
        let targets: Vec<(std::path::PathBuf, std::path::PathBuf)> =
            serde_json::from_value(value["targets"].clone())
                .map_err(|e| OperationError::from(e.to_string()))?;
        let paths: Vec<_> = targets.into_iter().map(|(_, p)| p).collect();
        let old = self
            .remote
            .lock()
            .unwrap()
            .remove(&(session.into(), handle));
        let edit = if args["name"] == "edit" {
            let path = paths.first().ok_or("Edit needs a target")?;
            match old {
                Some(plan) if plan.paths.first() == Some(path) => plan.edit,
                old => {
                    drop(old);
                    Some(changes::coordinate_edit(path, cancel).await?)
                }
            }
        } else {
            None
        };
        self.remote
            .lock()
            .unwrap()
            .insert((session.into(), handle), RemotePlan { paths, edit });
        Ok(())
    }
    pub fn discard(&self, session: &str, handle: u64) {
        self.plans.lock().unwrap().remove(&(session.into(), handle));
    }
    pub fn end(&self, session: &str) {
        self.plans.lock().unwrap().retain(|(s, _), _| s != session);
        self.remote.lock().unwrap().retain(|(s, _), _| s != session);
        crate::shell::cancel_background_jobs(session);
    }
    pub async fn dispatch(
        &self,
        directory: &str,
        session: &str,
        method: &str,
        args: Value,
        cancel: CancellationToken,
    ) -> Result<EffectResult, OperationError> {
        let name = args["name"].as_str().unwrap_or_default();
        let handle = args["handle"].as_u64().unwrap_or_default();
        let value = match method {
            "paths" => {
                let reservation = changes::reserve(directory, name, &args["args"], &cancel).await?;
                let id = self.next.fetch_add(1, Ordering::Relaxed) + 1;
                let response = json!({"handle":id,"targets":reservation.targets});
                self.plans
                    .lock()
                    .unwrap()
                    .insert((session.into(), id), FileState::Reserved(reservation));
                response
            }
            "prepare" => {
                let id = if handle == 0 {
                    self.next.fetch_add(1, Ordering::Relaxed) + 1
                } else {
                    handle
                };
                let reserved = self.plans.lock().unwrap().remove(&(session.into(), handle));
                let plan = match reserved {
                    Some(FileState::Reserved(r)) => r.prepare(directory, cancel).await?,
                    Some(_) => return Err("File plan already prepared".into()),
                    None if handle == 0 => {
                        changes::prepare_plain(directory, name, &args["args"], cancel).await?
                    }
                    None => return Err("File reservation no longer exists".into()),
                };
                let draft = json!({"handle":id,"changes":if args["includeChanges"]==true{plan.wire_changes()?}else{Value::Null},"paths":plan.paths().collect::<Vec<_>>()});
                self.plans
                    .lock()
                    .unwrap()
                    .insert((session.into(), id), FileState::Prepared(plan));
                draft
            }
            "transform_paths" => {
                let plans = self.plans.lock().unwrap();
                let Some(FileState::Prepared(plan)) = plans.get(&(session.into(), handle)) else {
                    return Err("File plan no longer exists".into());
                };
                plan.transform_targets(directory, name, &args["changes"])?
            }
            "transform" => {
                let mut plan = self
                    .plans
                    .lock()
                    .unwrap()
                    .remove(&(session.into(), handle))
                    .ok_or("File plan no longer exists")?
                    .prepared()?;
                plan.transform(directory, name, &args["changes"], cancel)
                    .await?;
                let draft = json!({"handle":handle,"changes":plan.wire_changes()?,"paths":plan.paths().collect::<Vec<_>>()});
                self.plans
                    .lock()
                    .unwrap()
                    .insert((session.into(), handle), FileState::Prepared(plan));
                draft
            }
            "commit" => {
                let plan = self
                    .plans
                    .lock()
                    .unwrap()
                    .remove(&(session.into(), handle))
                    .ok_or("File plan no longer exists")?
                    .prepared()?;
                return Ok(EffectResult::Output(Arc::new(plan).run(cancel).await?));
            }
            "stop_jobs" => {
                self.end(session);
                Value::Null
            }

            "discard" => {
                self.discard(session, handle);
                Value::Null
            }
            "run" => {
                return Ok(EffectResult::Output(
                    crate::run_effect(directory, session, name, &args["args"], cancel).await?,
                ))
            }
            "skill" => {
                return Ok(EffectResult::Output(crate::skills::load_content(
                    directory,
                    &serde_json::from_value::<Vec<String>>(args["paths"].clone())
                        .map_err(|e| OperationError::from(e.to_string()))?,
                    name,
                )?))
            }
            _ => return Err(format!("Unknown tool operation: {method}").into()),
        };
        Ok(EffectResult::Json(value))
    }
}
