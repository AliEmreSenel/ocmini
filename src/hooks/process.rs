use super::config::{ResolvedRegistration, Settings};
use super::payload::{self, Files, PayloadMode};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::VecDeque;
use std::fmt;
use std::io::{self, Write};
use std::ops::Deref;
use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore};
use tokio::time::{Duration, Instant};

static SHUTDOWN: AtomicBool = AtomicBool::new(false);
static SHUTDOWN_NOTIFY: Notify = Notify::const_new();
static ACTIVE_PROCESSES: AtomicUsize = AtomicUsize::new(0);

pub async fn wait_for_shutdown() {
    let deadline = Instant::now() + Duration::from_secs(2);
    while ACTIVE_PROCESSES.load(Ordering::Acquire) > 0 && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

struct ProcessCount;
impl ProcessCount {
    fn new() -> Self {
        ACTIVE_PROCESSES.fetch_add(1, Ordering::AcqRel);
        Self
    }
}
impl Drop for ProcessCount {
    fn drop(&mut self) {
        ACTIVE_PROCESSES.fetch_sub(1, Ordering::AcqRel);
    }
}

pub fn shutdown() {
    SHUTDOWN.store(true, Ordering::Release);
    SHUTDOWN_NOTIFY.notify_waiters();
}
pub fn is_shutdown() -> bool {
    SHUTDOWN.load(Ordering::Acquire)
}

pub async fn cancelled(cancel: Option<&Arc<AtomicBool>>) {
    let stopping = SHUTDOWN_NOTIFY.notified();
    tokio::pin!(stopping);
    stopping.as_mut().enable();
    loop {
        if is_shutdown() || cancel.is_some_and(|c| c.load(Ordering::Acquire)) {
            return;
        }
        if cancel.is_none() {
            stopping.await;
            return;
        }
        tokio::select! {
            _ = &mut stopping => return,
            _ = tokio::time::sleep(Duration::from_millis(10)) => {},
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailureKind {
    Cancelled,
    QueueTimeout,
    QueueFull,
    Deadline,
    Spawn,
    Io,
    Exit,
    OutputLimit,
    Payload,
}

#[derive(Debug)]
pub struct ProcessError {
    pub kind: FailureKind,
    pub summary: String,
    pub stderr_tail: Vec<u8>,
    pub stderr_bytes: u64,
    pub queue_ms: u64,
    pub execution_ms: u64,
    pub exit_code: Option<i32>,
}

impl ProcessError {
    pub fn cancelled() -> Self {
        Self {
            kind: FailureKind::Cancelled,
            summary: "hook cancelled".into(),
            stderr_tail: Vec::new(),
            stderr_bytes: 0,
            queue_ms: 0,
            execution_ms: 0,
            exit_code: None,
        }
    }

    pub fn queue_timeout(msg: impl Into<String>, queue_ms: u64) -> Self {
        Self {
            kind: FailureKind::QueueTimeout,
            summary: msg.into(),
            stderr_tail: Vec::new(),
            stderr_bytes: 0,
            queue_ms,
            execution_ms: 0,
            exit_code: None,
        }
    }

    pub fn deadline(msg: impl Into<String>, queue_ms: u64, execution_ms: u64) -> Self {
        Self {
            kind: FailureKind::Deadline,
            summary: msg.into(),
            stderr_tail: Vec::new(),
            stderr_bytes: 0,
            queue_ms,
            execution_ms,
            exit_code: None,
        }
    }

    pub fn spawn(msg: impl Into<String>, queue_ms: u64) -> Self {
        Self {
            kind: FailureKind::Spawn,
            summary: msg.into(),
            stderr_tail: Vec::new(),
            stderr_bytes: 0,
            queue_ms,
            execution_ms: 0,
            exit_code: None,
        }
    }

    pub fn io(msg: impl Into<String>) -> Self {
        Self {
            kind: FailureKind::Io,
            summary: msg.into(),
            stderr_tail: Vec::new(),
            stderr_bytes: 0,
            queue_ms: 0,
            execution_ms: 0,
            exit_code: None,
        }
    }

    pub fn exit(
        code_or_signal: &str,
        exit_code: Option<i32>,
        stderr_tail: Vec<u8>,
        stderr_bytes: u64,
        queue_ms: u64,
        execution_ms: u64,
    ) -> Self {
        Self {
            kind: FailureKind::Exit,
            summary: format!("hook exited unsuccessfully ({code_or_signal})"),
            stderr_tail,
            stderr_bytes,
            queue_ms,
            execution_ms,
            exit_code,
        }
    }

    pub fn output_limit(msg: impl Into<String>, queue_ms: u64, execution_ms: u64) -> Self {
        Self {
            kind: FailureKind::OutputLimit,
            summary: msg.into(),
            stderr_tail: Vec::new(),
            stderr_bytes: 0,
            queue_ms,
            execution_ms,
            exit_code: None,
        }
    }

    pub fn payload(msg: impl Into<String>) -> Self {
        Self {
            kind: FailureKind::Payload,
            summary: msg.into(),
            stderr_tail: Vec::new(),
            stderr_bytes: 0,
            queue_ms: 0,
            execution_ms: 0,
            exit_code: None,
        }
    }

    pub fn with_timings(mut self, queue_ms: u64, execution_ms: u64) -> Self {
        self.queue_ms = queue_ms;
        self.execution_ms = execution_ms;
        self
    }

    pub fn with_stderr(mut self, stderr_tail: Vec<u8>, stderr_bytes: u64) -> Self {
        self.stderr_tail = stderr_tail;
        self.stderr_bytes = stderr_bytes;
        self
    }

    pub fn summary(&self) -> &str {
        &self.summary
    }
}

impl fmt::Display for ProcessError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.summary)
    }
}

impl Deref for ProcessError {
    type Target = str;
    fn deref(&self) -> &Self::Target {
        &self.summary
    }
}

impl std::error::Error for ProcessError {}

#[derive(Debug)]
pub struct ProcessOutcome {
    pub stdout: Vec<u8>,
    pub stderr_tail: Vec<u8>,
    pub stderr_bytes: u64,
    pub queue_ms: u64,
    pub execution_ms: u64,
    pub payload: Option<Files>,
    // Keep capacity through control parsing, including large referenced values.
    _permits: Option<(PoolPermit, OwnedSemaphorePermit)>,
    deadline: Instant,
}

impl ProcessOutcome {
    pub fn response_value(&self, max_payload_bytes: usize) -> Result<Option<Value>, ProcessError> {
        if Instant::now() >= self.deadline {
            return Err(ProcessError::deadline(
                "hook deadline exceeded before response validation",
                self.queue_ms,
                self.execution_ms,
            ));
        }
        if self.stdout.iter().all(u8::is_ascii_whitespace) {
            return Ok(None);
        }
        let stdout_val: Value = serde_json::from_slice(&self.stdout).map_err(|e| {
            ProcessError::payload(format!(
                "hook stdout must be exactly one valid JSON object: {e}"
            ))
        })?;

        if let Some(obj) = stdout_val.as_object() {
            if obj.get("version").and_then(|v| v.as_u64()) == Some(2) {
                if obj.len() != 2 || !obj.contains_key("responseFile") {
                    return Err(ProcessError::payload(
                        "v2 response requires only version and responseFile",
                    ));
                }
                if let Some(resp_file_val) = obj.get("responseFile") {
                    let resp_file_str = resp_file_val
                        .as_str()
                        .ok_or_else(|| ProcessError::payload("responseFile must be a string"))?;
                    let result = crate::blocking::run(|| {
                        self.read_response_file(resp_file_str, max_payload_bytes)
                    });
                    if Instant::now() >= self.deadline {
                        return Err(ProcessError::deadline(
                            "hook deadline exceeded during response validation",
                            self.queue_ms,
                            self.execution_ms,
                        ));
                    }
                    return result;
                }
            }
        }

        Ok(Some(stdout_val))
    }

    fn read_response_file(
        &self,
        resp_path_str: &str,
        max_payload_bytes: usize,
    ) -> Result<Option<Value>, ProcessError> {
        let files = self.payload.as_ref().ok_or_else(|| {
            ProcessError::payload("responseFile received but payload_mode was not file")
        })?;

        let expected_path = files.response_path();
        let given_path = Path::new(resp_path_str);

        if given_path != expected_path {
            return Err(ProcessError::payload(format!(
                "invalid responseFile path: expected predetermined path {:?}, got {:?}",
                expected_path, given_path
            )));
        }

        #[cfg(unix)]
        let file = {
            use std::os::fd::{AsRawFd, FromRawFd};
            let fd = unsafe {
                libc::openat(
                    files.directory_file.as_raw_fd(),
                    c"response.json".as_ptr(),
                    libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
                )
            };
            if fd < 0 {
                let e = io::Error::last_os_error();
                return Err(if e.kind() == io::ErrorKind::NotFound {
                    ProcessError::payload(format!(
                        "response file does not exist: {:?}",
                        expected_path
                    ))
                } else if e.raw_os_error() == Some(libc::ELOOP) {
                    ProcessError::payload(
                        "response file must be a regular file, symlinks are rejected",
                    )
                } else {
                    ProcessError::io(format!("cannot open response file: {e}"))
                });
            }
            unsafe { std::fs::File::from_raw_fd(fd) }
        };

        #[cfg(not(unix))]
        let file = std::fs::File::open(expected_path).map_err(|e| {
            if e.kind() == io::ErrorKind::NotFound {
                ProcessError::payload(format!("response file does not exist: {:?}", expected_path))
            } else {
                ProcessError::io(format!("cannot open response file: {e}"))
            }
        })?;

        let metadata = file.metadata().map_err(|e| {
            ProcessError::io(format!("cannot read open response file metadata: {e}"))
        })?;

        if !metadata.is_file() {
            return Err(ProcessError::payload(
                "response file must be a regular file",
            ));
        }

        if metadata.len() > max_payload_bytes as u64 {
            return Err(ProcessError::payload(format!(
                "response file exceeds maxPayloadBytes limit ({} > {})",
                metadata.len(),
                max_payload_bytes
            )));
        }

        use std::io::Read;
        let mut reader = std::io::BufReader::new(file.take(max_payload_bytes as u64 + 1));
        let mut decoder = serde_json::Deserializer::from_reader(&mut reader);
        let val = Value::deserialize(&mut decoder).map_err(|e| {
            ProcessError::payload(format!(
                "response file contains invalid or truncated JSON: {e}"
            ))
        })?;
        decoder.end().map_err(|e| {
            ProcessError::payload(format!(
                "response file contains invalid or truncated JSON: {e}"
            ))
        })?;
        if reader.get_ref().limit() == 0 {
            return Err(ProcessError::payload(
                "response file exceeds maxPayloadBytes limit",
            ));
        }

        Ok(Some(val))
    }
}

pub struct ResizablePool {
    semaphore: Arc<Semaphore>,
    balance: Arc<Mutex<PoolBalance>>,
}
#[derive(Debug)]
struct PoolBalance {
    capacity: usize,
    debt: usize,
}

impl ResizablePool {
    pub fn new(initial_capacity: usize) -> Self {
        Self {
            semaphore: Arc::new(Semaphore::new(initial_capacity)),
            balance: Arc::new(Mutex::new(PoolBalance {
                capacity: initial_capacity,
                debt: 0,
            })),
        }
    }

    pub fn capacity(&self) -> usize {
        self.balance.lock().unwrap().capacity
    }

    pub fn available(&self) -> usize {
        self.semaphore.available_permits()
    }

    pub fn resize(&self, new_capacity: usize) {
        let mut balance = self.balance.lock().unwrap();
        let old = balance.capacity;
        balance.capacity = new_capacity;
        if new_capacity > old {
            let cancel_debt = balance.debt.min(new_capacity - old);
            balance.debt -= cancel_debt;
            let remaining = new_capacity - old - cancel_debt;
            if remaining > 0 {
                self.semaphore.add_permits(remaining);
            }
        } else if new_capacity < old {
            let diff = old - new_capacity;
            let removed = self.semaphore.forget_permits(diff);
            balance.debt += diff - removed;
        }
    }

    pub async fn acquire_permit(&self) -> Result<PoolPermit, ProcessError> {
        let permit = self
            .semaphore
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| ProcessError::io("pool capacity closed"))?;

        Ok(PoolPermit {
            permit: Some(permit),
            balance: self.balance.clone(),
        })
    }
    fn try_acquire_permit(&self) -> Result<PoolPermit, ProcessError> {
        let permit = self.semaphore.clone().try_acquire_owned().map_err(|_| {
            let mut error = ProcessError::queue_timeout("hook pending queue is full", 0);
            error.kind = FailureKind::QueueFull;
            error
        })?;
        Ok(PoolPermit {
            permit: Some(permit),
            balance: self.balance.clone(),
        })
    }
}

#[derive(Debug)]
pub struct PoolPermit {
    permit: Option<OwnedSemaphorePermit>,
    balance: Arc<Mutex<PoolBalance>>,
}

impl Drop for PoolPermit {
    fn drop(&mut self) {
        if let Some(permit) = self.permit.take() {
            let mut balance = self.balance.lock().unwrap();
            if balance.debt > 0 {
                balance.debt -= 1;
                permit.forget();
            } else {
                drop(permit);
            }
        }
    }
}

struct GlobalPools {
    processes: ResizablePool,
    observers: ResizablePool,
    pending: ResizablePool,
    pending_observers: ResizablePool,
    max_payload_bytes: AtomicUsize,
    default_queue_timeout_ms: AtomicU64,
}

fn global_pools() -> &'static GlobalPools {
    static POOLS: OnceLock<GlobalPools> = OnceLock::new();
    POOLS.get_or_init(|| GlobalPools {
        processes: ResizablePool::new(16),
        observers: ResizablePool::new(4),
        pending: ResizablePool::new(128),
        pending_observers: ResizablePool::new(128),
        max_payload_bytes: AtomicUsize::new(64 * 1024 * 1024),
        default_queue_timeout_ms: AtomicU64::new(30_000),
    })
}

pub fn configure_limits(settings: &Settings) {
    let pools = global_pools();
    pools.processes.resize(settings.max_global_processes);
    pools.observers.resize(settings.max_global_observers);
    pools.pending.resize(settings.max_pending_hooks);
    pools.pending_observers.resize(settings.max_pending_hooks);
    pools
        .max_payload_bytes
        .store(settings.max_payload_bytes, Ordering::Release);
    pools
        .default_queue_timeout_ms
        .store(settings.queue_timeout_ms, Ordering::Release);
}

pub fn limits_debug() -> Value {
    let pools = global_pools();
    serde_json::json!({
        "maxGlobalProcesses": pools.processes.capacity(),
        "availableProcesses": pools.processes.available(),
        "maxGlobalObservers": pools.observers.capacity(),
        "availableObservers": pools.observers.available(),
        "maxPendingHooks": pools.pending.capacity(),
        "availablePending": pools.pending.available(),
        "availablePendingObservers": pools.pending_observers.available(),
        "defaultQueueTimeoutMs": pools.default_queue_timeout_ms.load(Ordering::Acquire),
        "maxPayloadBytes": pools.max_payload_bytes.load(Ordering::Acquire),
    })
}

struct BoundedWriter {
    bytes: Vec<u8>,
    limit: usize,
}
impl Write for BoundedWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.limit.saturating_sub(self.bytes.len()) {
            return Err(io::Error::other("hook stdin exceeds byte budget"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub fn encode<T: Serialize>(value: &T, limit: usize) -> Result<Vec<u8>, String> {
    let mut writer = BoundedWriter {
        bytes: Vec::new(),
        limit,
    };
    serde_json::to_writer(&mut writer, value)
        .map_err(|e| format!("cannot encode hook input: {e}"))?;
    Ok(writer.bytes)
}

async fn capture_stdout(
    mut stream: impl AsyncRead + Unpin,
    limit: usize,
) -> Result<Vec<u8>, ProcessError> {
    let mut bytes = Vec::with_capacity(limit.min(8192));
    let mut buffer = [0u8; 8192];
    loop {
        let n = stream
            .read(&mut buffer)
            .await
            .map_err(|e| ProcessError::io(format!("cannot read hook stdout: {e}")))?;
        if n == 0 {
            return Ok(bytes);
        }
        if n > limit.saturating_sub(bytes.len()) {
            return Err(ProcessError::output_limit(
                "hook stdout exceeds byte budget",
                0,
                0,
            ));
        }
        bytes.extend_from_slice(&buffer[..n]);
    }
}

#[derive(Default)]
struct StderrTail {
    bytes: VecDeque<u8>,
    total: u64,
}
async fn capture_stderr_tail(
    mut stream: impl AsyncRead + Unpin,
    limit: usize,
    capture: &Mutex<StderrTail>,
) -> Result<(), ProcessError> {
    let mut buffer = [0u8; 8192];
    loop {
        let n = stream
            .read(&mut buffer)
            .await
            .map_err(|e| ProcessError::io(format!("cannot read hook stderr: {e}")))?;
        if n == 0 {
            return Ok(());
        }
        let mut captured = capture.lock().unwrap();
        captured.total = captured.total.saturating_add(n as u64);
        let tail = &mut captured.bytes;
        if limit > 0 {
            let chunk = &buffer[..n];
            if chunk.len() >= limit {
                tail.clear();
                tail.extend(&chunk[chunk.len() - limit..]);
            } else {
                let excess = (tail.len() + chunk.len()).saturating_sub(limit);
                if excess > 0 {
                    tail.drain(..excess);
                }
                tail.extend(chunk);
            }
        }
    }
}

/// Owns the child even if the caller drops or aborts the invocation future.
struct ChildGuard {
    child: Option<tokio::process::Child>,
    id: u32,
    count: Option<ProcessCount>,
    reaped: Arc<AtomicBool>,
}

impl ChildGuard {
    fn kill_group(&self) {
        #[cfg(unix)]
        if self.id > 1 && self.id <= i32::MAX as u32 && !self.reaped.load(Ordering::Acquire) {
            unsafe {
                libc::kill(-(self.id as i32), libc::SIGKILL);
            }
        }
    }

    async fn stop(&mut self) {
        self.kill_group();
        if let Some(child) = &mut self.child {
            let _ = child.start_kill();
            let _ = child.wait().await;
        }
        self.child.take();
        self.count.take();
    }
}

async fn wait_child_status(
    child: &mut tokio::process::Child,
    id: u32,
    reaped: &AtomicBool,
) -> Result<std::process::ExitStatus, ProcessError> {
    #[cfg(unix)]
    {
        let mut exits = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::child())
            .map_err(|e| ProcessError::io(format!("cannot watch hook exit: {e}")))?;
        loop {
            let exited = {
                let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
                // WNOWAIT keeps the leader's PID reserved until descendants have
                // been stopped; never signal a process-group ID after reaping it.
                let result = unsafe {
                    libc::waitid(
                        libc::P_PID,
                        id,
                        &mut info,
                        libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                    )
                };
                if result < 0 {
                    let error = io::Error::last_os_error();
                    if error.kind() == io::ErrorKind::Interrupted {
                        continue;
                    }
                    if error.raw_os_error() == Some(libc::ECHILD) {
                        reaped.store(true, Ordering::Release);
                    }
                    return Err(ProcessError::io(format!(
                        "cannot inspect hook exit: {error}"
                    )));
                }
                unsafe { info.si_pid() == id as i32 }
            };
            if exited {
                if id > 1 && id <= i32::MAX as u32 {
                    unsafe {
                        libc::kill(-(id as i32), libc::SIGKILL);
                    }
                }
                break;
            }
            if exits.recv().await.is_none() {
                return Err(ProcessError::io("hook exit watcher closed"));
            }
        }
    }
    #[cfg(not(unix))]
    let _ = id;
    let status = child
        .wait()
        .await
        .map_err(|e| ProcessError::io(format!("cannot reap hook child: {e}")))?;
    reaped.store(true, Ordering::Release);
    Ok(status)
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        self.kill_group();
        if let Some(mut child) = self.child.take() {
            let _ = child.start_kill();
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                let count = self.count.take();
                runtime.spawn(async move {
                    let _ = child.wait().await;
                    drop(count);
                });
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn run<T: Serialize>(
    registration: &ResolvedRegistration,
    settings: &Settings,
    directory: &str,
    envelope: &T,
    cancel: Option<&Arc<AtomicBool>>,
    deadline: Option<Instant>,
    capacity: Arc<Semaphore>,
    boundary: &str,
    invocation: &str,
    session: &str,
) -> Result<ProcessOutcome, ProcessError> {
    let observer = boundary == "event";
    if is_shutdown() || cancel.is_some_and(|v| v.load(Ordering::Acquire)) {
        return Err(ProcessError::cancelled());
    }

    if deadline.is_some_and(|d| Instant::now() >= d) {
        return Err(ProcessError::deadline("hook deadline exceeded", 0, 0));
    }

    let r = &registration.registration;
    let queue_start = Instant::now();
    let queue_timeout_ms = r.queue_timeout_ms.unwrap_or(settings.queue_timeout_ms);
    let queue_timeout_duration = Duration::from_millis(queue_timeout_ms);
    let queue_deadline = queue_start + queue_timeout_duration;
    let effective_queue_deadline = match deadline {
        Some(d) => queue_deadline.min(d),
        None => queue_deadline,
    };

    let pending_pool = if observer {
        &global_pools().pending_observers
    } else {
        &global_pools().pending
    };
    let pending_permit = pending_pool.try_acquire_permit()?;
    let acquire_capacity = async {
        let local = capacity
            .acquire_owned()
            .await
            .map_err(|_| ProcessError::io("scope capacity closed"))?;
        let global = if observer {
            global_pools().observers.acquire_permit().await?
        } else {
            global_pools().processes.acquire_permit().await?
        };
        Ok::<_, ProcessError>((global, local))
    };

    let (_global_permit, _local_permit) = tokio::select! {
        biased;
        _ = cancelled(cancel) => return Err(ProcessError::cancelled().with_timings(queue_start.elapsed().as_millis() as u64, 0)),
        _ = tokio::time::sleep_until(effective_queue_deadline) => {
            let elapsed_queue = queue_start.elapsed().as_millis() as u64;
            if deadline.is_some_and(|d| d <= queue_deadline && Instant::now() >= d) {
                return Err(ProcessError::deadline("hook deadline exceeded while waiting for capacity", elapsed_queue, 0));
            } else {
                return Err(ProcessError::queue_timeout("hook queue timeout exceeded while waiting for capacity", elapsed_queue));
            }
        }
        res = acquire_capacity => res?,
    };

    drop(pending_permit);
    let queue_ms = queue_start.elapsed().as_millis() as u64;

    let exec_start = Instant::now();
    let exec_duration = Duration::from_millis(registration.hook_timeout_ms(boundary));
    let exec_deadline = exec_start + exec_duration;
    let effective_exec_deadline = match deadline {
        Some(d) => exec_deadline.min(d),
        None => exec_deadline,
    };

    let payload_mode = r.payload_mode;
    let (stdin_bytes, files) = crate::blocking::run(|| match payload_mode {
        PayloadMode::Inline => {
            let limit = r.max_input_bytes.unwrap_or(settings.max_input_bytes);
            let bytes = encode(envelope, limit).map_err(ProcessError::payload)?;
            Ok::<_, ProcessError>((bytes, None))
        }
        PayloadMode::File => {
            let max_payload_bytes = settings.max_payload_bytes;
            let files = payload::spool(envelope, max_payload_bytes, invocation)?;
            let stdin_bytes = files.v2_stdin_bytes()?;
            Ok((stdin_bytes, Some(files)))
        }
    })?;
    if is_shutdown() || cancel.is_some_and(|c| c.load(Ordering::Acquire)) {
        return Err(ProcessError::cancelled()
            .with_timings(queue_ms, exec_start.elapsed().as_millis() as u64));
    }
    if Instant::now() >= effective_exec_deadline {
        return Err(ProcessError::deadline(
            "hook deadline exceeded during payload preparation",
            queue_ms,
            exec_start.elapsed().as_millis() as u64,
        ));
    }

    let mut command = tokio::process::Command::new(
        r.command
            .first()
            .ok_or_else(|| ProcessError::spawn("empty hook command", queue_ms))?,
    );
    command
        .args(&r.command[1..])
        .current_dir(directory)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .env("OCMINI_HOOK_INVOCATION_ID", invocation)
        .env("OCMINI_HOOK_SESSION_ID", session);

    if let Some(f) = &files {
        command.env("OCMINI_HOOK_PAYLOAD_PATH", f.payload_path());
        command.env("OCMINI_HOOK_RESPONSE_PATH", f.response_path());
        command.env("OCMINI_HOOK_PAYLOAD_DIR", f.dir_path());
    }

    #[cfg(unix)]
    command.process_group(0);

    let child = command.spawn().map_err(|e| {
        ProcessError::spawn(
            format!("cannot start hook executable {}: {e}", r.command[0]),
            queue_ms,
        )
    })?;
    let id = child
        .id()
        .ok_or_else(|| ProcessError::spawn("hook child has no process id", queue_ms))?;
    let mut guard = ChildGuard {
        child: Some(child),
        id,
        count: Some(ProcessCount::new()),
        reaped: Arc::new(AtomicBool::new(false)),
    };
    let reaped = guard.reaped.clone();
    let child = guard.child.as_mut().unwrap();
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| ProcessError::io("hook stdin unavailable"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| ProcessError::io("hook stdout unavailable"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| ProcessError::io("hook stderr unavailable"))?;

    let max_output = r.max_output_bytes.unwrap_or(settings.max_output_bytes);
    let max_stderr = r.max_stderr_bytes.unwrap_or(settings.max_stderr_bytes);
    let stderr_capture = Mutex::new(StderrTail::default());

    let io = async {
        let write_stdin = async {
            match stdin.write_all(&stdin_bytes).await {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::BrokenPipe => {}
                Err(e) => return Err(ProcessError::io(format!("cannot write hook stdin: {e}"))),
            }
            drop(stdin);
            Ok::<_, ProcessError>(())
        };

        let wait_child = async { wait_child_status(child, id, &reaped).await };

        let ((), stdout, (), status) = tokio::try_join!(
            write_stdin,
            capture_stdout(stdout, max_output),
            capture_stderr_tail(stderr, max_stderr, &stderr_capture),
            wait_child,
        )?;

        let execution_ms = exec_start.elapsed().as_millis() as u64;

        if !status.success() {
            let code_str = status
                .code()
                .map_or_else(|| "signal".into(), |n| n.to_string());
            return Err(ProcessError::exit(
                &code_str,
                status.code(),
                Vec::new(),
                0,
                queue_ms,
                execution_ms,
            ));
        }

        Ok::<_, ProcessError>(stdout)
    };

    let result = tokio::select! {
        biased;
        _ = cancelled(cancel) => {
            let execution_ms = exec_start.elapsed().as_millis() as u64;
            Err(ProcessError::cancelled().with_timings(queue_ms, execution_ms))
        }
        _ = tokio::time::sleep_until(effective_exec_deadline) => {
            let execution_ms = exec_start.elapsed().as_millis() as u64;
            Err(ProcessError::deadline("hook deadline exceeded", queue_ms, execution_ms))
        }
        res = io => res,
    };

    guard.stop().await;

    let captured = stderr_capture.into_inner().unwrap();
    let stderr_tail: Vec<u8> = captured.bytes.into_iter().collect();
    let stderr_bytes = captured.total;
    let execution_ms = exec_start.elapsed().as_millis() as u64;
    let stdout = match result {
        Ok(stdout) => stdout,
        Err(error) => {
            return Err(error
                .with_timings(queue_ms, execution_ms)
                .with_stderr(stderr_tail, stderr_bytes))
        }
    };

    Ok(ProcessOutcome {
        stdout,
        stderr_tail,
        stderr_bytes,
        queue_ms,
        execution_ms,
        payload: files,
        _permits: Some((_global_permit, _local_permit)),
        deadline: effective_exec_deadline,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::config::Registration;
    use serde_json::json;
    use std::fs;

    fn test_settings() -> Settings {
        Settings {
            queue_timeout_ms: 2000,
            max_input_bytes: 1024 * 1024,
            max_output_bytes: 256 * 1024,
            max_stderr_bytes: 64 * 1024,
            max_payload_bytes: 4 * 1024 * 1024,
            max_processes: 8,
            max_global_processes: 16,
            max_global_observers: 4,
            max_pending_hooks: 128,
            ..Settings::default()
        }
    }

    fn test_resolved(command: Vec<&str>, payload_mode: PayloadMode) -> ResolvedRegistration {
        ResolvedRegistration {
            registration: serde_json::from_value::<Registration>(json!({"id":"test_hook","on":"turn.start.before","command":command,"timeoutMs":2000,"queueTimeoutMs":1000,"payloadMode":payload_mode})).unwrap(),
            source: "test".into(),
            trusted: true,
            blocked: None,
        }
    }

    #[tokio::test]
    async fn multiple_mib_stderr_plus_valid_stdout_succeeds_and_tail_bounded() {
        let settings = test_settings();
        let script = "head -c 3145728 /dev/zero | tr '\\0' 'x' >&2; echo '{\"action\":\"allow\"}'";
        let resolved = test_resolved(vec!["sh", "-c", script], PayloadMode::Inline);
        let sem = Arc::new(Semaphore::new(4));

        let outcome = run(
            &resolved,
            &settings,
            ".",
            &json!({"test": 123}),
            None,
            None,
            sem,
            "turn.start.before",
            "inv_1",
            "ses_1",
        )
        .await
        .expect("should succeed despite massive stderr");

        assert_eq!(outcome.stdout, b"{\"action\":\"allow\"}\n");
        assert!(outcome.stderr_bytes >= 3 * 1024 * 1024);
        assert_eq!(outcome.stderr_tail.len(), settings.max_stderr_bytes);
        assert!(outcome.stderr_tail.iter().all(|&b| b == b'x'));
    }

    #[tokio::test]
    async fn stdout_overflow_errors() {
        let mut settings = test_settings();
        settings.max_output_bytes = 1024;
        let script = "head -c 2048 /dev/zero | tr '\\0' 'o'; sleep 30";
        let resolved = test_resolved(vec!["sh", "-c", script], PayloadMode::Inline);
        let sem = Arc::new(Semaphore::new(4));

        let start = Instant::now();
        let err = run(
            &resolved,
            &settings,
            ".",
            &json!({"test": 123}),
            None,
            None,
            sem,
            "turn.start.before",
            "inv_2",
            "ses_1",
        )
        .await
        .unwrap_err();

        assert_eq!(err.kind, FailureKind::OutputLimit);
        assert!(start.elapsed() < Duration::from_secs(1));
        assert!(err.summary().contains("byte budget"));
    }

    #[tokio::test]
    async fn queue_wait_separate_from_command_time() {
        let settings = test_settings();
        let sem = Arc::new(Semaphore::new(1));
        let permit = sem.clone().acquire_owned().await.unwrap();

        let mut resolved = test_resolved(
            vec!["sh", "-c", "sleep 0.02; echo '{\"action\":\"allow\"}'"],
            PayloadMode::Inline,
        );
        resolved.registration.timeout_ms = Some(60);

        let sem_clone = sem.clone();
        let handle = tokio::spawn(async move {
            run(
                &resolved,
                &settings,
                ".",
                &json!({}),
                None,
                None,
                sem_clone,
                "turn.start.before",
                "inv_q",
                "ses_1",
            )
            .await
        });

        tokio::time::sleep(Duration::from_millis(80)).await;
        drop(permit);

        let outcome = handle.await.unwrap().expect("run should succeed");
        assert!(outcome.queue_ms >= 70, "queue_ms was {}", outcome.queue_ms);
        assert!(
            outcome.execution_ms >= 15,
            "execution_ms was {}",
            outcome.execution_ms
        );
    }

    #[tokio::test]
    async fn cancellation_during_queue_and_execution() {
        let settings = test_settings();
        let sem = Arc::new(Semaphore::new(0));
        let cancel = Arc::new(AtomicBool::new(false));

        let mut resolved = test_resolved(vec!["echo", "ok"], PayloadMode::Inline);
        resolved.registration.queue_timeout_ms = Some(2000);

        let cancel_clone = cancel.clone();
        let sem_clone = sem.clone();
        let handle = tokio::spawn(async move {
            run(
                &resolved,
                &settings,
                ".",
                &json!({}),
                Some(&cancel_clone),
                None,
                sem_clone,
                "turn.start.before",
                "inv_c1",
                "ses_1",
            )
            .await
        });

        tokio::time::sleep(Duration::from_millis(30)).await;
        cancel.store(true, Ordering::Release);

        let err = handle.await.unwrap().unwrap_err();
        assert_eq!(err.kind, FailureKind::Cancelled);
    }

    #[tokio::test]
    async fn file_input_exact_bytes_schema_and_large_response_read() {
        let settings = test_settings();
        let dir = tempfile::tempdir().unwrap();

        let script = r#"
            read -r STDIN_LINE
            echo "$STDIN_LINE" > stdin_captured.json
            test -f "$OCMINI_HOOK_PAYLOAD_PATH" || exit 1
            test -n "$OCMINI_HOOK_RESPONSE_PATH" || exit 2
            test -n "$OCMINI_HOOK_PAYLOAD_DIR" || exit 3

            echo -n '{"status":"ok","large":"' > "$OCMINI_HOOK_RESPONSE_PATH"
            head -c 200000 /dev/zero | tr '\0' 'a' >> "$OCMINI_HOOK_RESPONSE_PATH"
            echo '"}' >> "$OCMINI_HOOK_RESPONSE_PATH"

            echo "{\"version\":2,\"responseFile\":\"$OCMINI_HOOK_RESPONSE_PATH\"}"
        "#;

        let resolved = test_resolved(vec!["sh", "-c", script], PayloadMode::File);
        let sem = Arc::new(Semaphore::new(4));

        let envelope = json!({
            "version": 1,
            "hook": "test.hook",
            "invocationID": "inv_file_1",
            "operationID": "op_test_99",
            "context": {"directory": dir.path().to_str().unwrap()},
            "input": {"hello": "world"}
        });

        let outcome = run(
            &resolved,
            &settings,
            dir.path().to_str().unwrap(),
            &envelope,
            None,
            None,
            sem,
            "turn.start.before",
            "inv_file_1",
            "ses_file",
        )
        .await
        .expect("file mode hook should succeed");

        assert!(outcome.payload.is_some());
        let payload_files = outcome.payload.as_ref().unwrap();
        let actual_payload: Value =
            serde_json::from_reader(std::fs::File::open(payload_files.payload_path()).unwrap())
                .unwrap();
        assert_eq!(actual_payload, envelope);

        let captured_bytes = fs::read(dir.path().join("stdin_captured.json")).unwrap();
        let v2: Value = serde_json::from_slice(&captured_bytes).unwrap();
        assert_eq!(v2["version"], 2);
        assert_eq!(v2["invocationID"], "inv_file_1");
        assert_eq!(actual_payload["operationID"], "op_test_99");
        assert_eq!(v2["payload"]["contentType"], "application/json");
        assert_eq!(v2["payload"]["lifetime"], "invocation");
        assert_eq!(
            v2["payload"]["path"],
            payload_files.payload_path().to_str().unwrap()
        );

        let val = outcome
            .response_value(4 * 1024 * 1024)
            .expect("should resolve responseFile")
            .expect("should have value");

        assert_eq!(val["status"], "ok");
        assert_eq!(val["large"].as_str().unwrap().len(), 200000);
    }

    #[tokio::test]
    async fn response_file_invalid_cases_symlink_size_missing_path() {
        let settings = test_settings();
        let dir = tempfile::tempdir().unwrap();

        let resolved = test_resolved(
            vec![
                "sh",
                "-c",
                "echo '{\"version\":2,\"responseFile\":\"/etc/passwd\"}'",
            ],
            PayloadMode::File,
        );
        let sem = Arc::new(Semaphore::new(4));
        let outcome = run(
            &resolved,
            &settings,
            dir.path().to_str().unwrap(),
            &json!({}),
            None,
            None,
            sem.clone(),
            "turn.start.before",
            "inv_bad1",
            "s",
        )
        .await
        .unwrap();

        let err = outcome.response_value(1024 * 1024).unwrap_err();
        assert_eq!(err.kind, FailureKind::Payload);
        assert!(err.summary().contains("invalid responseFile path"));

        let script = r#"
            touch "$OCMINI_HOOK_PAYLOAD_DIR/real.json"
            ln -s "$OCMINI_HOOK_PAYLOAD_DIR/real.json" "$OCMINI_HOOK_RESPONSE_PATH"
            echo "{\"version\":2,\"responseFile\":\"$OCMINI_HOOK_RESPONSE_PATH\"}"
        "#;
        let resolved = test_resolved(vec!["sh", "-c", script], PayloadMode::File);
        let outcome = run(
            &resolved,
            &settings,
            dir.path().to_str().unwrap(),
            &json!({}),
            None,
            None,
            sem.clone(),
            "turn.start.before",
            "inv_bad2",
            "s",
        )
        .await
        .unwrap();

        let err = outcome.response_value(1024 * 1024).unwrap_err();
        assert_eq!(err.kind, FailureKind::Payload);
        assert!(err.summary().contains("symlinks are rejected"));

        let script = r#"
            head -c 2048 /dev/zero | tr '\0' 'a' > "$OCMINI_HOOK_RESPONSE_PATH"
            echo "{\"version\":2,\"responseFile\":\"$OCMINI_HOOK_RESPONSE_PATH\"}"
        "#;
        let resolved = test_resolved(vec!["sh", "-c", script], PayloadMode::File);
        let outcome = run(
            &resolved,
            &settings,
            dir.path().to_str().unwrap(),
            &json!({}),
            None,
            None,
            sem.clone(),
            "turn.start.before",
            "inv_bad3",
            "s",
        )
        .await
        .unwrap();

        let err = outcome.response_value(512).unwrap_err();
        assert_eq!(err.kind, FailureKind::Payload);
        assert!(err.summary().contains("exceeds maxPayloadBytes"));

        let script = r#"
            echo -n '{"unclosed":' > "$OCMINI_HOOK_RESPONSE_PATH"
            echo "{\"version\":2,\"responseFile\":\"$OCMINI_HOOK_RESPONSE_PATH\"}"
        "#;
        let resolved = test_resolved(vec!["sh", "-c", script], PayloadMode::File);
        let outcome = run(
            &resolved,
            &settings,
            dir.path().to_str().unwrap(),
            &json!({}),
            None,
            None,
            sem,
            "turn.start.before",
            "inv_bad4",
            "s",
        )
        .await
        .unwrap();

        let err = outcome.response_value(1024 * 1024).unwrap_err();
        assert_eq!(err.kind, FailureKind::Payload);
        assert!(err.summary().contains("invalid or truncated JSON"));
    }

    #[tokio::test]
    async fn temp_cleanup_on_error_cancel_and_outcome_drop() {
        let settings = test_settings();
        let dir = tempfile::tempdir().unwrap();

        let script = r#"
            echo "$OCMINI_HOOK_PAYLOAD_DIR" > dir.txt
            echo '{"action":"allow"}'
        "#;
        let resolved = test_resolved(vec!["sh", "-c", script], PayloadMode::File);
        let sem = Arc::new(Semaphore::new(4));

        let outcome = run(
            &resolved,
            &settings,
            dir.path().to_str().unwrap(),
            &json!({}),
            None,
            None,
            sem,
            "turn.start.before",
            "inv_clean",
            "s",
        )
        .await
        .unwrap();

        let temp_dir_path = fs::read_to_string(dir.path().join("dir.txt")).unwrap();
        let temp_dir_path = Path::new(temp_dir_path.trim());

        assert!(temp_dir_path.exists());
        drop(outcome);
        assert!(
            !temp_dir_path.exists(),
            "temp dir should be cleaned on drop"
        );
    }

    #[test]
    fn resizable_pool_fairness_and_running_holders() {
        // Test isolated local ResizablePool instance to avoid modifying static global state
        let pool = ResizablePool::new(4);
        assert_eq!(pool.capacity(), 4);
        assert_eq!(pool.available(), 4);

        pool.resize(2);
        assert_eq!(pool.capacity(), 2);
        assert_eq!(pool.available(), 2);

        pool.resize(6);
        assert_eq!(pool.capacity(), 6);
        assert_eq!(pool.available(), 6);
    }
    #[tokio::test]
    async fn resizing_and_waiter_limits_preserve_owned_capacity() {
        let pool = ResizablePool::new(2);
        let first = pool.acquire_permit().await.unwrap();
        let second = pool.acquire_permit().await.unwrap();
        pool.resize(1);
        assert!(pool.try_acquire_permit().is_err());
        drop(first);
        assert!(pool.try_acquire_permit().is_err());
        pool.resize(3);
        let third = pool.try_acquire_permit().unwrap();
        drop(second);
        drop(third);
        assert_eq!(pool.available(), 3);
    }
    #[tokio::test]
    async fn response_keeps_capacity_until_validation_finishes() {
        let settings = test_settings();
        let dir = tempfile::tempdir().unwrap();
        let local = Arc::new(Semaphore::new(1));
        let resolved = test_resolved(vec!["true"], PayloadMode::Inline);
        let outcome = run(
            &resolved,
            &settings,
            dir.path().to_str().unwrap(),
            &json!({}),
            None,
            None,
            local.clone(),
            "turn.start.before",
            "inv_capacity",
            "s",
        )
        .await
        .unwrap();
        assert_eq!(local.available_permits(), 0);
        drop(outcome);
        assert_eq!(local.available_permits(), 1);
    }
    #[tokio::test]
    async fn queued_invocations_do_not_serialize_their_payloads() {
        struct Input(Arc<AtomicUsize>);
        impl Serialize for Input {
            fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                self.0.fetch_add(1, Ordering::Relaxed);
                s.serialize_str("must not encode while queued")
            }
        }
        let settings = test_settings();
        let mut resolved = test_resolved(vec!["true"], PayloadMode::Inline);
        resolved.registration.queue_timeout_ms = Some(25);
        let encodes = Arc::new(AtomicUsize::new(0));
        let error = run(
            &resolved,
            &settings,
            ".",
            &Input(encodes.clone()),
            None,
            None,
            Arc::new(Semaphore::new(0)),
            "turn.start.before",
            "inv_queue",
            "s",
        )
        .await
        .unwrap_err();
        assert_eq!(error.kind, FailureKind::QueueTimeout);
        assert_eq!(encodes.load(Ordering::Relaxed), 0);
    }
    #[tokio::test]
    async fn normal_exit_stops_descendants_before_reaping_leader() {
        let settings = test_settings();
        let resolved = test_resolved(
            vec!["sh", "-c", "sleep 30 & echo '{}'"],
            PayloadMode::Inline,
        );
        let start = Instant::now();
        let out = run(
            &resolved,
            &settings,
            ".",
            &json!({}),
            None,
            None,
            Arc::new(Semaphore::new(1)),
            "turn.start.before",
            "inv_children",
            "s",
        )
        .await
        .unwrap();
        assert_eq!(out.stdout, b"{}\n");
        assert!(start.elapsed() < Duration::from_secs(1));
    }
}
