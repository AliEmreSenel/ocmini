//! Session ownership and the binary's tool handler for the sandbox transport.
use crate::{
    config::Config,
    types::{OperationError, Session},
};
use ocmini_sandbox::config::{Policy, Selection};
#[cfg(target_os = "linux")]
use ocmini_sandbox::wire::{self, Control};
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
#[cfg(target_os = "linux")]
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;
#[derive(Clone)]
struct Entry {
    parent: Option<String>,
    policy: Option<Policy>,
    runner: Option<Arc<Runner>>,
}
pub struct Registry {
    entries: Mutex<HashMap<String, Entry>>,
    folder: PathBuf,
}
impl Registry {
    pub fn new(config: &Config) -> Result<Self, String> {
        let folder = PathBuf::from(crate::config::config_home()).join("opencode/sandbox/presets");
        let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| cwd.clone());
        ocmini_sandbox::config::initialize(&folder, &cwd, &home)
            .map_err(ocmini_sandbox::refusal)?;
        let registry = Self {
            entries: Default::default(),
            folder,
        };
        registry.policy(config)?;
        Ok(registry)
    }
    fn policy(&self, config: &Config) -> Result<Option<Policy>, String> {
        let mut selection: Selection = serde_json::from_value(
            config
                .extra
                .get("sandbox")
                .cloned()
                .unwrap_or_else(|| serde_json::json!({})),
        )
        .map_err(ocmini_sandbox::refusal)?;
        if let Ok(name) = std::env::var("OCMINI_SANDBOX_PRESET") {
            selection.preset = name;
        }
        let policy = ocmini_sandbox::config::load(&self.folder, selection.name())
            .map_err(ocmini_sandbox::refusal)?;
        #[cfg(not(target_os = "linux"))]
        if policy.is_some() {
            return Err(ocmini_sandbox::refusal("sandbox requires Linux"));
        }
        #[cfg(target_os = "linux")]
        if let Some(policy) = &policy {
            ocmini_sandbox::jail::probe(policy).map_err(ocmini_sandbox::refusal)?;
        }
        Ok(policy)
    }
    pub fn start(&self, session: &Session, config: &Config) -> Result<(), OperationError> {
        let mut entries = self.entries.lock().unwrap();
        if entries.contains_key(&session.id) {
            return Ok(());
        }
        let policy = match session.parent_id.as_ref().and_then(|p| entries.get(p)) {
            Some(parent) => parent.policy.clone(),
            None => self.policy(config)?,
        };
        let runner = match &policy {
            Some(policy) => Some(Runner::start(session, policy.clone())?),
            None => None,
        };
        entries.insert(
            session.id.clone(),
            Entry {
                parent: session.parent_id.clone(),
                policy,
                runner,
            },
        );
        Ok(())
    }
    pub fn enabled(&self, id: &str) -> bool {
        self.entries
            .lock()
            .unwrap()
            .get(id)
            .is_some_and(|e| e.policy.is_some())
    }
    pub async fn request(
        &self,
        id: &str,
        method: &str,
        args: Value,
        cancel: CancellationToken,
    ) -> Result<Value, OperationError> {
        self.request_retained(id, method, args, cancel, None).await
    }
    pub async fn request_retained(
        &self,
        id: &str,
        method: &str,
        args: Value,
        cancel: CancellationToken,
        lease: Option<Box<dyn Send>>,
    ) -> Result<Value, OperationError> {
        let runner = self
            .entries
            .lock()
            .unwrap()
            .get(id)
            .and_then(|e| e.runner.clone())
            .ok_or("Runner is unavailable")?;
        runner.request(method, args, cancel, lease).await
    }
    pub fn stop_jobs(self: &Arc<Self>, id: &str) {
        let mut ids = vec![id.to_owned()];
        {
            let entries = self.entries.lock().unwrap();
            let mut at = 0;
            while at < ids.len() {
                let children: Vec<_> = entries
                    .iter()
                    .filter(|(_, e)| e.parent.as_deref() == Some(ids[at].as_str()))
                    .map(|(id, _)| id.clone())
                    .collect();
                ids.extend(children);
                at += 1;
            }
        }
        for id in ids {
            if self.enabled(&id) {
                let registry = self.clone();
                tokio::spawn(async move {
                    let _ = registry
                        .request(
                            &id,
                            "stop_jobs",
                            serde_json::json!({}),
                            CancellationToken::new(),
                        )
                        .await;
                });
            }
        }
    }
    pub fn end(&self, id: &str) {
        let mut entries = self.entries.lock().unwrap();
        let mut ids = vec![id.to_owned()];
        let mut at = 0;
        while at < ids.len() {
            let children: Vec<_> = entries
                .iter()
                .filter(|(_, e)| e.parent.as_deref() == Some(ids[at].as_str()))
                .map(|(id, _)| id.clone())
                .collect();
            ids.extend(children);
            at += 1;
        }
        for id in ids {
            if let Some(entry) = entries.remove(&id) {
                if let Some(runner) = entry.runner {
                    runner.fail("Session ended");
                }
            }
        }
    }
}
impl Drop for Registry {
    fn drop(&mut self) {
        for entry in self.entries.get_mut().unwrap().values() {
            if let Some(runner) = &entry.runner {
                runner.fail("Runtime ended");
            }
        }
    }
}
#[cfg(target_os = "linux")]
struct Pending {
    tx: tokio::sync::oneshot::Sender<Result<Value, OperationError>>,
    _lease: Option<Box<dyn Send>>,
}
#[cfg(target_os = "linux")]
struct Runner {
    #[cfg(target_os = "linux")]
    child: Mutex<Option<ocmini_sandbox::jail::Child>>,
    writer: Arc<tokio::sync::Mutex<tokio::net::unix::OwnedWriteHalf>>,
    pending: Mutex<HashMap<u64, Pending>>,
    next: AtomicU64,
    failure: Mutex<Option<String>>,
    cancel: CancellationToken,
    spill: wire::Spill,
}
#[cfg(target_os = "linux")]
struct PendingGuard {
    runner: Arc<Runner>,
    id: u64,
    complete: bool,
}
#[cfg(target_os = "linux")]
impl Drop for PendingGuard {
    fn drop(&mut self) {
        if !self.complete {
            let state = self.runner.clone();
            let id = self.id;
            tokio::spawn(async move {
                let mut writer = state.writer.lock().await;
                if let Err(e) = wire::write_control(&mut *writer, &Control::Cancel { id }).await {
                    state.fail(&format!("Runner failed: {e}"));
                }
            });
        }
    }
}

#[cfg(target_os = "linux")]
impl Runner {
    #[cfg(target_os = "linux")]
    fn start(session: &Session, policy: Policy) -> Result<Arc<Self>, OperationError> {
        let data = PathBuf::from(crate::config::data_home()).join("ocmini");
        let spill = data.join("tool-output").join(&session.id);
        let scratch = data.join("sandbox").join(&session.id);
        std::fs::create_dir_all(&spill).map_err(|e| e.to_string())?;
        let wire_spill = wire::Spill::new(spill.clone()).map_err(|e| e.to_string())?;
        let child = ocmini_sandbox::jail::spawn(ocmini_sandbox::jail::RuntimeSpec {
            policy: policy.clone(),
            scratch,
            spill: spill.clone(),
            directory: session.directory.clone().into(),
        })?;
        let socket = child.control.try_clone().map_err(|e| e.to_string())?;
        socket.set_nonblocking(true).map_err(|e| e.to_string())?;
        let socket = tokio::net::UnixStream::from_std(socket).map_err(|e| e.to_string())?;
        let (mut reader, writer) = socket.into_split();
        let packets = child
            .packets
            .as_ref()
            .map(|s| s.try_clone())
            .transpose()
            .map_err(|e| e.to_string())?;
        let host_pid = child.pid;
        let runner = Arc::new(Self {
            child: Mutex::new(Some(child)),
            writer: Arc::new(tokio::sync::Mutex::new(writer)),
            pending: Default::default(),
            next: AtomicU64::new(1),
            failure: Default::default(),
            cancel: CancellationToken::new(),
            spill: wire_spill,
        });
        let state = runner.clone();
        tokio::spawn(async move {
            loop {
                let result = tokio::select! {r=wire::read_control(&mut reader)=>r,_=state.cancel.cancelled()=>break};
                match result {
                    Ok(Control::Result { id, value }) => {
                        let result = state
                            .spill
                            .unpack_async(value)
                            .await
                            .map_err(|e| OperationError::from(e.to_string()));
                        let pending = state.pending.lock().unwrap().remove(&id);
                        if let Some(pending) = pending {
                            if let Err(Ok(value)) = pending.tx.send(result) {
                                if let Some(handle) = value["handle"].as_u64() {
                                    let state = state.clone();
                                    tokio::spawn(async move {
                                        let _ = state
                                            .request(
                                                "discard",
                                                serde_json::json!({"handle":handle}),
                                                CancellationToken::new(),
                                                None,
                                            )
                                            .await;
                                    });
                                }
                            }
                        }
                    }
                    Ok(Control::Error { id, message }) => {
                        let message =
                            match state.spill.unpack_async(message).await.and_then(|value| {
                                serde_json::from_value::<String>(value)
                                    .map_err(std::io::Error::other)
                            }) {
                                Ok(message) => message,
                                Err(error) => {
                                    state.fail(&format!("Invalid runner error: {error}"));
                                    break;
                                }
                            };
                        if let Some(pending) = state.pending.lock().unwrap().remove(&id) {
                            let _ = pending.tx.send(Err(message.into()));
                        }
                    }
                    Ok(_) => {
                        state.fail("Invalid runner response");
                        break;
                    }
                    Err(e) => {
                        state.fail(&format!("Runner failed: {e}"));
                        break;
                    }
                }
            }
        });
        if let Some(packets) = packets {
            let state = runner.clone();
            tokio::spawn(async move {
                use futures::FutureExt;
                let result = std::panic::AssertUnwindSafe(ocmini_sandbox::broker::run(
                    packets,
                    host_pid,
                    policy,
                    state.cancel.clone(),
                ))
                .catch_unwind()
                .await
                .unwrap_or_else(|_| Err(std::io::Error::other("network broker stopped")));
                if let Err(e) = result {
                    state.fail(&format!("Network broker failed: {e}"));
                }
            });
        }
        Ok(runner)
    }
    #[cfg(not(target_os = "linux"))]
    fn start(_session: &Session, _policy: Policy) -> Result<Arc<Self>, OperationError> {
        Err(ocmini_sandbox::refusal("sandbox requires Linux").into())
    }
    fn fail(&self, message: &str) {
        let mut failure = self.failure.lock().unwrap();
        if failure.is_some() {
            return;
        }
        *failure = Some(message.into());
        self.cancel.cancel();
        // File transaction leases outlive callers and release only after the
        // runner's work is terminal, including cancellation and runner death.
        drop(self.child.lock().unwrap().take());
        for (_, pending) in self.pending.lock().unwrap().drain() {
            let _ = pending.tx.send(Err(message.into()));
        }
    }
    async fn request(
        self: &Arc<Self>,
        method: &str,
        args: Value,
        cancel: CancellationToken,
        lease: Option<Box<dyn Send>>,
    ) -> Result<Value, OperationError> {
        if cancel.is_cancelled() {
            return Err("Tool cancelled".into());
        }
        // Reserve the serialized writer before spawning so concurrent file
        // preparations retain their original order. The owned guard also
        // makes partial frame writes independent of the caller's lifetime.
        let writer = self.writer.clone();
        let mut writer = tokio::select! {writer=writer.lock_owned()=>writer,_=cancel.cancelled()=>return Err("Tool cancelled".into())};
        if let Some(error) = self.failure.lock().unwrap().as_ref() {
            return Err(error.clone().into());
        }
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.pending
            .lock()
            .unwrap()
            .insert(id, Pending { tx, _lease: lease });
        let state = self.clone();
        let method = method.to_owned();
        tokio::spawn(async move {
            let args = match state.spill.pack_async(args).await {
                Ok(args) => args,
                Err(error) => {
                    if let Some(pending) = state.pending.lock().unwrap().remove(&id) {
                        let _ = pending.tx.send(Err(error.to_string().into()));
                    }
                    return;
                }
            };
            if let Err(error) =
                wire::write_control(&mut *writer, &Control::Request { id, method, args }).await
            {
                state.fail(&format!("Runner failed: {error}"));
            }
        });
        let mut guard = PendingGuard {
            runner: self.clone(),
            id,
            complete: false,
        };
        tokio::select! {result=rx=>{guard.complete=true;result.map_err(|_|OperationError::from("Runner ended without a result"))?},_=cancel.cancelled()=>Err("Tool cancelled".into())}
    }
}

#[cfg(target_os = "linux")]
pub fn runner_main() -> Result<(), String> {
    use std::os::fd::FromRawFd;
    let spec = ocmini_sandbox::jail::inherited_spec().map_err(|e| e.to_string())?;
    ocmini_tools::output::set_directory(spec.spill.clone())
        .map_err(|_| "Runner output already initialized")?;
    let control = unsafe { std::os::unix::net::UnixStream::from_raw_fd(3) };
    control.set_nonblocking(true).map_err(|e| e.to_string())?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    {
        let _entered = runtime.enter();
        let _child = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::child())
            .map_err(|e| e.to_string())?;
        let _term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .map_err(|e| e.to_string())?;
    }
    ocmini_sandbox::jail::finish_runtime(&spec.policy).map_err(|e| e.to_string())?;
    runtime.block_on(tokio::task::LocalSet::new().run_until(async move{
        let socket=tokio::net::UnixStream::from_std(control).map_err(|e|e.to_string())?;
        let relay=if spec.policy.network.mode.mediated(){Some(ocmini_sandbox::broker::relay_inherited().map_err(|e|e.to_string())?)}else{None};
        if spec.policy.process_isolation==ocmini_sandbox::config::ProcessIsolation::Full {
            let mut children=tokio::signal::unix::signal(tokio::signal::unix::SignalKind::child()).map_err(|e|e.to_string())?;
            let mut table=unsafe{std::fs::File::from_raw_fd(0)};
            tokio::task::spawn_local(async move{use std::io::{Read,Seek,SeekFrom};while children.recv().await.is_some(){
                let mut list=String::new();
                if table.seek(SeekFrom::Start(0)).and_then(|_|table.read_to_string(&mut list)).is_ok(){for pid in list.split_whitespace().filter_map(|s|s.parse::<u32>().ok()){
                    if !ocmini_tools::process_group::tracked(pid) && !ocmini_tools::shell::tracked_background(pid){unsafe{libc::waitpid(pid as i32,std::ptr::null_mut(),libc::WNOHANG);}}
                }}
            }});
        }
        let effects=Arc::new(ocmini_tools::effects::Effects::default());let directory=spec.directory.to_string_lossy().into_owned();let spill=wire::Spill::new(spec.spill).map_err(|e|e.to_string())?;
        let mut term=tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).map_err(|e|e.to_string())?;
        let relay_end=async move{match relay{Some(mut relay)=>relay.wait().await,None=>std::future::pending().await}};
        tokio::select!{r=wire::serve(socket,spill,move|method,args,cancel|{let effects=effects.clone();let directory=directory.clone();async move{effects.dispatch(&directory,"runner",&method,args,cancel).await.and_then(ocmini_tools::effects::EffectResult::json).map_err(String::from)}})=>r.map_err(|e|e.to_string()),r=relay_end=>r.map_err(|e|e.to_string()),_=term.recv()=>Ok(())}
    }))
}

#[cfg(not(target_os = "linux"))]
struct Runner;
#[cfg(not(target_os = "linux"))]
impl Runner {
    fn start(_: &Session, _: Policy) -> Result<Arc<Self>, OperationError> {
        Err(ocmini_sandbox::refusal("sandbox requires Linux").into())
    }
    fn fail(&self, _: &str) {}
    async fn request(
        &self,
        _: &str,
        _: Value,
        _: CancellationToken,
        _: Option<Box<dyn Send>>,
    ) -> Result<Value, OperationError> {
        Err(ocmini_sandbox::refusal("sandbox requires Linux").into())
    }
}
