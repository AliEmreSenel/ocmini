//! Session ownership and the binary's tool handler for the sandbox transport.
use crate::{
    config::Config,
    types::{OperationError, Session},
};
use ocmini_sandbox::config::{Overrides, Policy, Settings};
#[cfg(target_os = "linux")]
use ocmini_sandbox::wire::{self, Control};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
#[cfg(target_os = "linux")]
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;
#[derive(Clone)]
struct Entry {
    parent: Option<String>,
    settings: Arc<Value>,
    policy: Option<Policy>,
    runner: Option<Arc<Runner>>,
}
pub struct Registry {
    entries: Mutex<HashMap<String, Entry>>,
    folder: PathBuf,
    overrides: Overrides,
    preset: Option<String>,
}
impl Registry {
    pub fn new(config: &Config) -> Result<Self, OperationError> {
        Self::new_with_overrides(config, Overrides::default())
    }

    pub fn new_with_overrides(
        config: &Config,
        overrides: Overrides,
    ) -> Result<Self, OperationError> {
        let folder = PathBuf::from(crate::config::config_home()).join("opencode/sandbox/presets");
        let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
        ocmini_sandbox::config::initialize(&folder)
            .map_err(|e| OperationError::sandbox_refused(ocmini_sandbox::refusal(e)))?;
        let registry = Self {
            entries: Default::default(),
            folder,
            overrides,
            preset: std::env::var("OCMINI_SANDBOX_PRESET")
                .ok()
                .filter(|value| !value.is_empty()),
        };
        config
            .sandbox_preset()
            .map_err(|e| OperationError::sandbox_refused(ocmini_sandbox::refusal(e)))?;
        // Both defaults and explicit CLI additions need the thread's saved
        // settings before selection. Validate the library now, then validate
        // the effective policy when the selected session is known.
        let context = ocmini_sandbox::config::PathContext::current(&cwd)
            .map_err(|e| OperationError::sandbox_refused(ocmini_sandbox::refusal(e)))?;
        ocmini_sandbox::config::validate_folder(&registry.folder, &context)
            .map_err(|e| OperationError::sandbox_refused(ocmini_sandbox::refusal(e)))?;
        Ok(registry)
    }
    /// A server's process-wide CLI policy must also work for new sessions.
    pub fn validate_cli_defaults(&self, config: &Config) -> Result<(), OperationError> {
        if self.preset.is_some() || !self.overrides.is_empty() {
            let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
            self.settings(config, &cwd, None, &self.overrides)?;
        }
        Ok(())
    }
    fn settings(
        &self,
        config: &Config,
        directory: &std::path::Path,
        saved: Option<&Value>,
        overrides: &Overrides,
    ) -> Result<(Arc<Value>, Option<Policy>), OperationError> {
        let configured = config
            .sandbox_preset()
            .map_err(|e| OperationError::sandbox_refused(ocmini_sandbox::refusal(e)))?;
        let saved: Option<Settings> = saved
            .filter(|saved| {
                self.preset
                    .as_deref()
                    .is_none_or(|name| saved["preset"].as_str() == Some(name))
            })
            .cloned()
            .map(serde_json::from_value)
            .transpose()
            .map_err(|e| {
                OperationError::sandbox_refused(ocmini_sandbox::refusal(format!(
                    "Invalid saved sandbox settings: {e}"
                )))
            })?;
        let context = ocmini_sandbox::config::PathContext::current(directory)
            .map_err(|e| OperationError::sandbox_refused(ocmini_sandbox::refusal(e)))?;
        let (settings, policy) = match saved {
            Some(mut saved) => {
                let policy = saved
                    .apply(&context, overrides)
                    .map_err(|e| OperationError::sandbox_refused(ocmini_sandbox::refusal(e)))?;
                (saved, policy)
            }
            _ => ocmini_sandbox::config::load_settings(
                &self.folder,
                self.preset.as_deref().unwrap_or(configured),
                &context,
                overrides,
            )
            .map_err(|e| OperationError::sandbox_refused(ocmini_sandbox::refusal(e)))?,
        };
        #[cfg(not(target_os = "linux"))]
        if policy.is_some() {
            return Err(OperationError::sandbox_refused(ocmini_sandbox::refusal(
                "sandbox requires Linux",
            )));
        }
        #[cfg(target_os = "linux")]
        if let Some(policy) = &policy {
            ocmini_sandbox::jail::probe(policy)
                .map_err(|e| OperationError::sandbox_refused(ocmini_sandbox::refusal(e)))?;
        }
        let saved = serde_json::to_value(settings)
            .map_err(|e| OperationError::sandbox_refused(ocmini_sandbox::refusal(e.to_string())))?;
        Ok((Arc::new(saved), policy))
    }
    /// Preset files available for selection, by name.
    pub fn presets(&self) -> Result<Vec<String>, String> {
        let mut names = Vec::new();
        let entries = std::fs::read_dir(&self.folder)
            .map_err(|e| format!("{}: {e}", self.folder.display()))?;
        for entry in entries {
            let path = entry.map_err(|e| e.to_string())?.path();
            if path.extension().and_then(|s| s.to_str()) == Some("json") {
                if let Some(name) = path.file_stem().and_then(|s| s.to_str()) {
                    names.push(name.to_owned());
                }
            }
        }
        names.sort();
        Ok(names)
    }
    /// A process-wide `--sandbox-preset` that every session must use.
    pub fn forced(&self) -> Option<&str> {
        self.preset.as_deref()
    }
    /// The preset a session without saved settings starts with.
    pub fn default_preset<'a>(&'a self, config: &'a Config) -> Result<&'a str, String> {
        match self.preset.as_deref() {
            Some(preset) => Ok(preset),
            None => config.sandbox_preset(),
        }
    }
    /// Saved settings for a root session in `directory` using `preset`,
    /// validated (including process overrides and the kernel probe) as a
    /// launch would. Saving them before the first turn avoids starting a jail
    /// under the default policy only to replace it.
    pub fn settings_for(&self, directory: &str, preset: &str) -> Result<Value, OperationError> {
        if let Some(forced) = self.preset.as_deref().filter(|forced| *forced != preset) {
            return Err(OperationError::invalid(format!(
                "This server was started with --sandbox-preset {forced}"
            )));
        }
        let context = ocmini_sandbox::config::PathContext::current(std::path::Path::new(directory))
            .map_err(|e| OperationError::sandbox_refused(ocmini_sandbox::refusal(e)))?;
        let (settings, _) = ocmini_sandbox::config::load_settings(
            &self.folder,
            preset,
            &context,
            &Overrides::default(),
        )
        .map_err(|e| OperationError::sandbox_refused(ocmini_sandbox::refusal(e)))?;
        let saved = serde_json::to_value(&settings)
            .map_err(|e| OperationError::sandbox_refused(ocmini_sandbox::refusal(e.to_string())))?;
        let policy = settings
            .clone()
            .apply(&context, &self.overrides)
            .map_err(|e| OperationError::sandbox_refused(ocmini_sandbox::refusal(e)))?;
        #[cfg(not(target_os = "linux"))]
        if policy.is_some() {
            return Err(OperationError::sandbox_refused(ocmini_sandbox::refusal(
                "sandbox requires Linux",
            )));
        }
        #[cfg(target_os = "linux")]
        if let Some(policy) = &policy {
            ocmini_sandbox::jail::probe(policy)
                .map_err(|e| OperationError::sandbox_refused(ocmini_sandbox::refusal(e)))?;
        }
        Ok(saved)
    }
    /// Changes an idle root session's saved sandbox. Its runner (and any
    /// background jobs) stops; the next turn starts from the new settings.
    pub fn select(
        &self,
        session: &Session,
        preset: &str,
        app: &crate::engine::App,
    ) -> Result<Value, OperationError> {
        if session.parent_id.is_some() {
            return Err(OperationError::invalid(
                "A subagent session uses its parent's sandbox",
            ));
        }
        let settings = self.settings_for(&session.directory, preset)?;
        let mut tree = vec![session.id.clone()];
        let mut at = 0;
        while at < tree.len() {
            tree.extend(app.store.children(&tree[at])?.into_iter().map(|c| c.id));
            at += 1;
        }
        for id in &tree {
            if app.store.is_busy(id)? {
                return Err(OperationError::busy(
                    "Stop the session before changing its sandbox",
                ));
            }
        }
        app.store.mutate_session(&session.id, |current| {
            if current.sandbox.as_ref() == Some(&settings) {
                return Ok(false);
            }
            current.sandbox = Some(settings.clone());
            Ok(true)
        })?;
        self.end(&session.id);
        Ok(settings)
    }
    pub fn start(&self, session: &Session, app: &crate::engine::App) -> Result<(), OperationError> {
        self.start_inner(session, app, true).map(|_| ())
    }
    pub fn prepare(
        &self,
        session: &mut Session,
        app: &crate::engine::App,
    ) -> Result<(), OperationError> {
        session.sandbox = Some((*self.start_inner(session, app, false)?).clone());
        Ok(())
    }
    fn verify_saved(
        entries: &HashMap<String, Entry>,
        id: &str,
        session: Option<&Session>,
        app: &crate::engine::App,
    ) -> Result<(), OperationError> {
        let mut root_id = id;
        let mut root = entries.get(root_id).ok_or("Sandbox entry is unavailable")?;
        while let Some(parent) = &root.parent {
            root_id = parent;
            root = entries
                .get(root_id)
                .ok_or("Sandbox ancestor is unavailable")?;
        }
        // The caller already loaded a root session. Descendants check the
        // authoritative root record rather than their older copied snapshot.
        if session
            .is_some_and(|s| s.id == root_id && s.sandbox.as_ref() == Some(root.settings.as_ref()))
        {
            return Ok(());
        }
        let current = app.store.get_session(root_id)?;
        if current.as_ref().and_then(|s| s.sandbox.as_ref()) != Some(root.settings.as_ref()) {
            return Err(OperationError::sandbox_refused(ocmini_sandbox::refusal(
                "Saved sandbox settings changed; restart ocmini before resuming this session",
            )));
        }
        Ok(())
    }
    fn start_inner(
        &self,
        session: &Session,
        app: &crate::engine::App,
        persist: bool,
    ) -> Result<Arc<Value>, OperationError> {
        let mut entries = self.entries.lock().unwrap();
        if entries.contains_key(&session.id) {
            Self::verify_saved(&entries, &session.id, Some(session), app)?;
            let entry = entries.get_mut(&session.id).unwrap();
            // An ancestor can have a frozen policy without having run yet.
            // Failed runners remain present and are never respawned.
            if entry.runner.is_none() {
                if let Some(policy) = &entry.policy {
                    entry.runner = Some(Runner::start(session, policy.clone())?);
                }
            }
            return Ok(entry.settings.clone());
        }
        let mut ancestors = Vec::new();
        let mut next = session.parent_id.clone();
        let mut seen = HashSet::new();
        let inherited = loop {
            let Some(parent) = next else {
                break None;
            };
            if let Some(entry) = entries.get(&parent) {
                Self::verify_saved(&entries, &parent, None, app)?;
                break Some((entry.settings.clone(), entry.policy.clone()));
            }
            if parent == session.id || !seen.insert(parent.clone()) {
                return Err(OperationError::invalid("Session ancestry contains a cycle"));
            }
            let ancestor = app
                .store
                .get_session(&parent)?
                .ok_or_else(|| OperationError::not_found("Parent session not found"))?;
            next = ancestor.parent_id.clone();
            ancestors.push(ancestor);
        };
        let (settings, policy) = match inherited {
            Some(inherited) => inherited,
            None => {
                let root = ancestors.last().unwrap_or(session);
                let config = if root.directory == session.directory {
                    None
                } else {
                    Some(Config::load_for(&root.directory).map_err(OperationError::invalid)?)
                };
                self.settings(
                    config.as_ref().unwrap_or(&app.config),
                    std::path::Path::new(&root.directory),
                    root.sandbox.as_ref(),
                    &self.overrides,
                )?
            }
        };
        let runner = match &policy {
            Some(policy) => Some(Runner::start(session, policy.clone())?),
            None => None,
        };
        let save = || -> Result<(), OperationError> {
            for saved in ancestors.iter().chain(persist.then_some(session)) {
                if saved.sandbox.as_ref() != Some(settings.as_ref()) {
                    if saved.sandbox.is_some() && app.store.is_busy(&saved.id)? {
                        return Err(OperationError::busy(
                            "Cannot change sandbox settings while the session tree is busy",
                        ));
                    }
                    app.store.mutate_session(&saved.id, |current| {
                        if current.sandbox.as_ref() == Some(settings.as_ref()) {
                            return Ok(false);
                        }
                        current.sandbox = Some((*settings).clone());
                        Ok(true)
                    })?;
                }
            }
            Ok(())
        };
        if let Err(error) = save() {
            if let Some(runner) = &runner {
                runner.fail("Cannot save sandbox settings");
            }
            return Err(error);
        }
        // Restore ancestry before starting a child, without spawning unused
        // parent runners or letting later preset edits split the tree's policy.
        for ancestor in ancestors.iter().rev() {
            entries.insert(
                ancestor.id.clone(),
                Entry {
                    parent: ancestor.parent_id.clone(),
                    settings: settings.clone(),
                    policy: policy.clone(),
                    runner: None,
                },
            );
        }
        entries.insert(
            session.id.clone(),
            Entry {
                parent: session.parent_id.clone(),
                settings: settings.clone(),
                policy,
                runner,
            },
        );
        Ok(settings)
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
        self.request_retained(id, method, args, cancel, None, None)
            .await
    }
    pub async fn request_retained(
        &self,
        id: &str,
        method: &str,
        args: Value,
        cancel: CancellationToken,
        lease: Option<Box<dyn Send>>,
        directory: Option<&str>,
    ) -> Result<Value, OperationError> {
        let runner = self
            .entries
            .lock()
            .unwrap()
            .get(id)
            .and_then(|e| e.runner.clone())
            .ok_or("Runner is unavailable")?;
        let mut args = args;
        if let Some(object) = args.as_object_mut() {
            object.insert("_ocmini_session".into(), id.into());
        }
        runner
            .request(method, args, cancel, lease, directory.map(str::to_owned))
            .await
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
        // v2 accepts caller-supplied IDs. They must not become path traversal
        // when selecting the session's private scratch and spill directories.
        if session.id.is_empty()
            || session.id.contains(['/', '\\', '\0'])
            || !matches!(
                std::path::Path::new(&session.id).components().next(),
                Some(std::path::Component::Normal(_))
            )
        {
            return Err(OperationError::sandbox_refused(ocmini_sandbox::refusal(
                "session ID must be a single path component",
            )));
        }
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
        })
        .map_err(OperationError::sandbox_refused)?;
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
        Err(OperationError::sandbox_refused(ocmini_sandbox::refusal(
            "sandbox requires Linux",
        )))
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
        directory: Option<String>,
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
            if let Err(error) = wire::write_control(
                &mut *writer,
                &Control::Request {
                    id,
                    method,
                    args,
                    directory,
                },
            )
            .await
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
        tokio::select!{r=wire::serve(socket,spill,move|method,mut args,requested_directory,cancel|{let effects=effects.clone();let directory=requested_directory.unwrap_or_else(||directory.clone());async move{
            if method == "check_directory" {
                std::fs::read_dir(&directory).map_err(|e|format!("Sandbox cannot enter directory {directory}: {e}"))?;
                return Ok(Value::Null);
            }
            let session = args
                .as_object_mut()
                .and_then(|object| object.remove("_ocmini_session"))
                .and_then(|value| value.as_str().map(str::to_owned))
                .unwrap_or_else(|| "runner".into());
            effects.dispatch(&directory,&session,&method,args,cancel).await.and_then(ocmini_tools::effects::EffectResult::json).map_err(String::from)
        }})=>r.map_err(|e|e.to_string()),r=relay_end=>r.map_err(|e|e.to_string()),_=term.recv()=>Ok(())}
    }))
}

#[cfg(not(target_os = "linux"))]
struct Runner;
#[cfg(not(target_os = "linux"))]
impl Runner {
    fn start(_: &Session, _: Policy) -> Result<Arc<Self>, OperationError> {
        Err(OperationError::sandbox_refused(ocmini_sandbox::refusal(
            "sandbox requires Linux",
        )))
    }
    fn fail(&self, _: &str) {}
    async fn request(
        &self,
        _: &str,
        _: Value,
        _: CancellationToken,
        _: Option<Box<dyn Send>>,
        _: Option<String>,
    ) -> Result<Value, OperationError> {
        Err(OperationError::sandbox_refused(ocmini_sandbox::refusal(
            "sandbox requires Linux",
        )))
    }
}
