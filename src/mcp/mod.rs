use futures::StreamExt;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, ACCEPT, CONTENT_TYPE};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;
use tokio::sync::oneshot;

const PROTOCOL: &str = "2025-03-26";
const MAX_MESSAGE: usize = 16 * 1024 * 1024;
type Reply = Result<Value, String>;
type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Reply>>>>;

#[derive(Default)]
struct Status {
    state: String,
    error: Option<String>,
}

impl Status {
    fn json(&self) -> Value {
        match &self.error {
            Some(error) => json!({"status": self.state, "error": error}),
            None => json!({"status": self.state}),
        }
    }
}

struct Local {
    sender: mpsc::Sender<String>,
    pending: Pending,
    child: Arc<Mutex<Option<Child>>>,
    active: Arc<AtomicBool>,
    stderr: Arc<Mutex<String>>,
}

impl Local {
    fn stop(&self) {
        self.active.store(false, Ordering::Release);
        if let Some(mut child) = self.child.lock().unwrap().take() {
            #[cfg(unix)]
            unsafe {
                libc::kill(-(child.id() as i32), libc::SIGKILL);
            }
            let _ = child.kill();
            let _ = child.wait();
        }
        fail_pending(&self.pending, "MCP server disconnected");
    }
}

impl Drop for Local {
    fn drop(&mut self) {
        self.stop();
    }
}

struct Remote {
    client: reqwest::Client,
    url: String,
    headers: HeaderMap,
    session: Mutex<Option<String>>,
    protocol: Mutex<String>,
    changed: Arc<AtomicBool>,
    active: AtomicBool,
}

enum Transport {
    Local(Local),
    Remote(Remote),
}

struct Server {
    config: Value,
    status: Arc<Mutex<Status>>,
    tools: Mutex<Vec<Value>>,
    transport: Mutex<Option<Arc<Transport>>>,
    changed: Arc<AtomicBool>,
    supports_tools: AtomicBool,
    next_id: AtomicU64,
    generation: AtomicU64,
    connect_lock: tokio::sync::Mutex<()>,
}

impl Server {
    fn set_status(&self, state: &str, error: Option<String>) {
        *self.status.lock().unwrap() = Status {
            state: state.into(),
            error,
        };
    }

    fn set_status_for(&self, generation: u64, state: &str, error: Option<String>) {
        let mut status = self.status.lock().unwrap();
        if self.generation.load(Ordering::Acquire) == generation {
            *status = Status {
                state: state.into(),
                error,
            };
        }
    }

    fn timeout(&self) -> Duration {
        Duration::from_millis(
            self.config["timeout"]
                .as_u64()
                .unwrap_or(30_000)
                .clamp(1, 600_000),
        )
    }

    fn stop(&self) {
        let transport = self.transport.lock().unwrap().take();
        if let Some(transport) = transport {
            match &*transport {
                Transport::Local(local) => local.stop(),
                Transport::Remote(remote) => {
                    remote.active.store(false, Ordering::Release);
                    end_remote_session(remote);
                }
            }
        }
        self.tools.lock().unwrap().clear();
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop();
    }
}

#[derive(Clone, Default)]
pub struct McpHub {
    servers: Arc<Mutex<BTreeMap<String, Arc<Server>>>>,
}

impl McpHub {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add(&self, name: &str, config: Value) -> Result<Value, String> {
        let hub = self.clone();
        let name = name.to_owned();
        blocking(async move { hub.add_async(&name, config).await })
    }

    pub async fn add_async(&self, name: &str, config: Value) -> Result<Value, String> {
        validate_config(name, &config)?;
        let server = Arc::new(Server {
            config,
            status: Arc::new(Mutex::new(Status {
                state: "disconnected".into(),
                error: None,
            })),
            tools: Mutex::new(Vec::new()),
            transport: Mutex::new(None),
            changed: Arc::new(AtomicBool::new(false)),
            supports_tools: AtomicBool::new(false),
            next_id: AtomicU64::new(1),
            generation: AtomicU64::new(0),
            connect_lock: tokio::sync::Mutex::new(()),
        });
        let old = self
            .servers
            .lock()
            .unwrap()
            .insert(name.into(), server.clone());
        if let Some(old) = old {
            old.generation.fetch_add(1, Ordering::AcqRel);
            old.set_status("disabled", None);
            old.stop();
        }
        if server.config["enabled"] == false {
            server.set_status("disabled", None);
        } else {
            connect(&server).await;
        }
        Ok(named_status(name, &server))
    }

    pub async fn connect_async(&self, name: &str) -> Result<Value, String> {
        let server = self.server(name)?;
        connect(&server).await;
        Ok(named_status(name, &server))
    }

    pub fn disconnect(&self, name: &str) -> Result<Value, String> {
        let server = self.server(name)?;
        server.generation.fetch_add(1, Ordering::AcqRel);
        server.stop();
        server.set_status("disabled", None);
        Ok(named_status(name, &server))
    }

    fn server(&self, name: &str) -> Result<Arc<Server>, String> {
        self.servers
            .lock()
            .unwrap()
            .get(name)
            .cloned()
            .ok_or_else(|| format!("MCP server {name:?} is not registered"))
    }

    pub async fn call_result_async(
        &self,
        name: &str,
        tool: &str,
        args: Value,
        cancel: Arc<AtomicBool>,
    ) -> Result<Value, String> {
        let server = self.server(name)?;
        let generation = server.generation.load(Ordering::Acquire);
        if server.status.lock().unwrap().state != "connected" {
            return Err(format!("MCP server {name:?} is not connected"));
        }
        if server.changed.swap(false, Ordering::AcqRel) {
            refresh_tools(&server).await?;
        }
        if !server
            .tools
            .lock()
            .unwrap()
            .iter()
            .any(|t| t["name"].as_str() == Some(tool))
        {
            return Err(format!("MCP server {name:?} has no tool {tool:?}"));
        }
        let result = match rpc(
            &server,
            "tools/call",
            json!({"name": tool, "arguments": args}),
            Some(cancel),
        )
        .await
        {
            Ok(result) => result,
            Err(error) => {
                if error.starts_with("MCP HTTP 401") || error.starts_with("MCP HTTP 403") {
                    server.set_status_for(generation, "needs_auth", Some(error.clone()));
                } else if error.starts_with("MCP HTTP ")
                    || error.starts_with("MCP event stream ended")
                {
                    server.set_status_for(generation, "failed", Some(error.clone()));
                }
                return Err(error);
            }
        };
        if result["isError"] == true {
            let text = render_result(&result);
            return Err(if text.is_empty() {
                format!("MCP tool {name}/{tool} failed")
            } else {
                text
            });
        }
        Ok(result)
    }

    pub fn status(&self) -> Value {
        let mut result = serde_json::Map::new();
        for (name, server) in self.servers.lock().unwrap().iter() {
            result.insert(name.clone(), server.status.lock().unwrap().json());
        }
        Value::Object(result)
    }

    pub async fn tool_defs_async(&self) -> Vec<Value> {
        let servers: Vec<_> = self.servers.lock().unwrap().values().cloned().collect();
        for server in servers {
            let generation = server.generation.load(Ordering::Acquire);
            let remote = transport_kind(&server.config) == "remote"
                && server.supports_tools.load(Ordering::Acquire);
            if (server.changed.swap(false, Ordering::AcqRel) || remote)
                && server.status.lock().unwrap().state == "connected"
            {
                if let Err(error) = refresh_tools(&server).await {
                    server.set_status_for(generation, "failed", Some(error));
                }
            }
        }
        self.tool_defs()
    }

    pub fn tool_defs(&self) -> Vec<Value> {
        self.routes().into_iter().map(|(name, _, _, tool)| {
            json!({"type": "function", "function": {
                "name": name,
                "description": tool["description"].as_str().unwrap_or("MCP tool"),
                "parameters": tool.get("inputSchema").cloned().unwrap_or_else(|| json!({"type":"object","properties":{}}))
            }})
        }).collect()
    }

    fn routes(&self) -> Vec<(String, String, String, Value)> {
        let mut used: HashSet<String> = [
            "bash",
            "read",
            "write",
            "edit",
            "apply_patch",
            "patch",
            "glob",
            "grep",
            "webfetch",
            "todowrite",
            "question",
            "task",
            "skill",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        let mut routes = Vec::new();
        for (server_name, server) in self.servers.lock().unwrap().iter() {
            if server.status.lock().unwrap().state != "connected" {
                continue;
            }
            for tool in server.tools.lock().unwrap().iter() {
                let Some(tool_name) = tool["name"].as_str() else {
                    continue;
                };
                let raw = format!("{server_name}_{tool_name}");
                let mut name: String = raw
                    .chars()
                    .map(|ch| {
                        if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
                            ch
                        } else {
                            '_'
                        }
                    })
                    .take(64)
                    .collect();
                if !used.insert(name.clone()) || name != raw {
                    let hash = stable_hash(&raw);
                    name.truncate(47);
                    name.push_str(&format!("_{hash:016x}"));
                    used.insert(name.clone());
                }
                routes.push((name, server_name.clone(), tool_name.into(), tool.clone()));
            }
        }
        routes
    }

    pub fn route(&self, name: &str) -> Option<(String, String)> {
        self.routes()
            .into_iter()
            .find_map(|(candidate, server, tool, _)| {
                if candidate == name || format!("mcp_{candidate}") == name {
                    Some((server, tool))
                } else {
                    None
                }
            })
    }
}

fn stable_hash(value: &str) -> u64 {
    value.bytes().fold(0xcbf29ce484222325, |hash, byte| {
        (hash ^ byte as u64).wrapping_mul(0x100000001b3)
    })
}

fn named_status(name: &str, server: &Server) -> Value {
    let mut status = server.status.lock().unwrap().json();
    status["name"] = Value::String(name.into());
    status
}

fn transport_kind(config: &Value) -> &str {
    config["type"]
        .as_str()
        .filter(|kind| !kind.is_empty())
        .unwrap_or_else(|| {
            if config["url"].as_str().is_some_and(|url| !url.is_empty()) {
                "remote"
            } else {
                "local"
            }
        })
}

fn validate_config(name: &str, config: &Value) -> Result<(), String> {
    if name.trim().is_empty() || !config.is_object() {
        return Err("MCP requires a nonempty name and object config".into());
    }

    if config["enabled"] == false {
        return Ok(());
    }
    match transport_kind(config) {
        "local" => {
            let command = config["command"]
                .as_array()
                .ok_or("MCP local command must be a nonempty array of strings")?;
            if command.is_empty()
                || command.iter().any(|v| v.as_str().is_none())
                || command[0].as_str().unwrap().is_empty()
            {
                return Err("MCP local command must be a nonempty array of strings".into());
            }
            if let Some(env) = config.get("environment").filter(|v| !v.is_null()) {
                if env
                    .as_object()
                    .is_none_or(|env| env.values().any(|v| !v.is_string()))
                {
                    return Err("MCP environment must map names to strings".into());
                }
            }
        }
        "remote" => {
            let url =
                reqwest::Url::parse(config["url"].as_str().ok_or("MCP remote URL is required")?)
                    .map_err(|e| format!("Invalid MCP URL: {e}"))?;
            if !matches!(url.scheme(), "http" | "https")
                || url.host_str().is_none()
                || !url.username().is_empty()
                || url.password().is_some()
            {
                return Err(
                    "MCP URL must be HTTP(S), with credentials supplied through headers".into(),
                );
            }
        }
        other => return Err(format!("Unsupported MCP transport {other:?}")),
    }
    if let Some(timeout) = config.get("timeout").filter(|v| !v.is_null()) {
        if !timeout.as_u64().is_some_and(|v| v > 0 && v <= 600_000) {
            return Err("MCP timeout must be 1..600000 milliseconds".into());
        }
    }
    Ok(())
}

struct ConnectionGuard {
    server: Arc<Server>,
    generation: u64,
    armed: bool,
}
impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        if self.armed && self.server.generation.load(Ordering::Acquire) == self.generation {
            self.server.stop();
            self.server.set_status_for(
                self.generation,
                "failed",
                Some("MCP connection cancelled".into()),
            );
        }
    }
}

async fn connect(server: &Arc<Server>) {
    let _lock = server.connect_lock.lock().await;
    let generation = server.generation.fetch_add(1, Ordering::AcqRel) + 1;
    let mut guard = ConnectionGuard {
        server: server.clone(),
        generation,
        armed: true,
    };
    server.stop();
    server.set_status("disconnected", None);
    let transport = if transport_kind(&server.config) == "remote" {
        create_remote(&server.config, server.changed.clone()).map(Transport::Remote)
    } else {
        create_local(
            &server.config,
            server.status.clone(),
            server.changed.clone(),
        )
        .map(Transport::Local)
    };
    let result = match transport {
        Ok(transport) => {
            {
                let mut slot = server.transport.lock().unwrap();
                if server.generation.load(Ordering::Acquire) != generation {
                    return;
                }
                *slot = Some(Arc::new(transport));
            }
            initialize(server).await
        }
        Err(error) => Err(error),
    };
    if server.generation.load(Ordering::Acquire) != generation {
        return;
    }
    match result {
        Ok(()) => {
            let transport = server.transport.lock().unwrap().clone();
            let mut status = server.status.lock().unwrap();
            let live = transport
                .as_ref()
                .is_some_and(|transport| match &**transport {
                    Transport::Local(local) => local.active.load(Ordering::Acquire),
                    Transport::Remote(remote) => remote.active.load(Ordering::Acquire),
                });
            if live && server.generation.load(Ordering::Acquire) == generation {
                *status = Status {
                    state: "connected".into(),
                    error: None,
                };
            }
        }
        Err(mut error) => {
            let transport = server.transport.lock().unwrap().clone();
            if let Some(transport) = transport {
                if let Transport::Local(local) = &*transport {
                    let tail = local.stderr.lock().unwrap().clone();
                    if !tail.trim().is_empty() {
                        error.push_str(&format!("\nServer stderr: {}", tail.trim()));
                    }
                }
            }
            server.stop();
            let state = if error.contains("HTTP 401") || error.contains("HTTP 403") {
                "needs_auth"
            } else {
                "failed"
            };
            server.set_status_for(generation, state, Some(error));
        }
    }
    guard.armed = false;
}

async fn initialize(server: &Server) -> Result<(), String> {
    let result = rpc(
        server,
        "initialize",
        json!({
            "protocolVersion": PROTOCOL,
            "capabilities": {},
            "clientInfo": {"name": "ocmini", "version": env!("CARGO_PKG_VERSION")}
        }),
        None,
    )
    .await?;
    let protocol = result["protocolVersion"]
        .as_str()
        .ok_or("MCP initialize omitted protocolVersion")?;
    if !["2024-11-05", "2025-03-26", "2025-06-18", "2025-11-25"].contains(&protocol) {
        return Err(format!(
            "MCP server selected unsupported protocol version {protocol:?}"
        ));
    }
    if !result["capabilities"].is_object() || !result["serverInfo"].is_object() {
        return Err("MCP initialize omitted capabilities or serverInfo".into());
    }
    if let Some(transport) = server.transport.lock().unwrap().as_ref() {
        if let Transport::Remote(remote) = &**transport {
            *remote.protocol.lock().unwrap() = protocol.into();
        }
    }
    notify(server, "notifications/initialized", json!({})).await?;
    let supports_tools = result["capabilities"]["tools"].is_object();
    server
        .supports_tools
        .store(supports_tools, Ordering::Release);
    if supports_tools {
        refresh_tools(server).await?;
    }
    Ok(())
}

async fn refresh_tools(server: &Server) -> Result<(), String> {
    let generation = server.generation.load(Ordering::Acquire);
    let mut tools = Vec::new();
    let mut cursor = None;
    let mut seen = HashSet::new();
    let mut names = HashSet::new();
    for _ in 0..1000 {
        let params = cursor
            .as_ref()
            .map_or_else(|| json!({}), |cursor| json!({"cursor": cursor}));
        let result = rpc(server, "tools/list", params, None).await?;
        for tool in result["tools"]
            .as_array()
            .ok_or("MCP tools/list omitted tools array")?
        {
            let name = tool["name"]
                .as_str()
                .filter(|n| !n.is_empty())
                .ok_or("MCP tool has no name")?;
            if !tool["inputSchema"].is_object() {
                return Err(format!("MCP tool {name:?} has invalid inputSchema"));
            }
            if !names.insert(name.to_owned()) {
                return Err(format!("MCP tools/list returned duplicate tool {name:?}"));
            }
            tools.push(tool.clone());
        }
        cursor = result["nextCursor"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
        match cursor.as_ref() {
            None => {
                let mut current = server.tools.lock().unwrap();
                if server.generation.load(Ordering::Acquire) != generation {
                    return Err("MCP connection changed while refreshing tools".into());
                }
                *current = tools;
                return Ok(());
            }
            Some(cursor) if !seen.insert(cursor.clone()) => {
                return Err("MCP tools/list repeated a pagination cursor".into())
            }
            _ => {}
        }
    }
    Err("MCP tools/list exceeded 1000 pages".into())
}

fn create_local(
    config: &Value,
    status: Arc<Mutex<Status>>,
    changed: Arc<AtomicBool>,
) -> Result<Local, String> {
    let args = config["command"]
        .as_array()
        .ok_or("MCP local command is required")?;
    let program = args
        .first()
        .and_then(Value::as_str)
        .ok_or("MCP local command is empty")?;
    let mut command = Command::new(program);
    command
        .args(args[1..].iter().filter_map(Value::as_str))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(directory) = config["cwd"]
        .as_str()
        .or_else(|| config["directory"].as_str())
    {
        command.current_dir(directory);
    }
    if let Some(environment) = config["environment"].as_object() {
        for (key, value) in environment {
            command.env(
                key,
                value
                    .as_str()
                    .ok_or("MCP environment values must be strings")?,
            );
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command
        .spawn()
        .map_err(|e| format!("Could not start MCP server {program:?}: {e}"))?;
    let mut stdin = child.stdin.take().ok_or("MCP stdin unavailable")?;
    let stdout = child.stdout.take().ok_or("MCP stdout unavailable")?;
    let stderr = child.stderr.take().ok_or("MCP stderr unavailable")?;
    let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
    let (sender, receiver) = mpsc::channel::<String>();
    let writer_pending = pending.clone();
    std::thread::spawn(move || {
        while let Ok(line) = receiver.recv() {
            if writeln!(stdin, "{line}")
                .and_then(|_| stdin.flush())
                .is_err()
            {
                fail_pending(&writer_pending, "MCP stdin closed");
                break;
            }
        }
    });
    let child = Arc::new(Mutex::new(Some(child)));
    let active = Arc::new(AtomicBool::new(true));
    let reader_child = child.clone();
    let reader_active = active.clone();
    let reader_pending = pending.clone();
    let responder = sender.clone();
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let error = loop {
            let line = match bounded_line(&mut reader, MAX_MESSAGE) {
                Ok(Some(line)) => line,
                Ok(None) => break "MCP server closed stdout".to_owned(),
                Err(error) => break format!("MCP stdout read failed: {error}"),
            };
            if line.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            let value = match serde_json::from_slice::<Value>(&line) {
                Ok(value) => value,
                Err(error) => break format!("MCP server emitted invalid JSON: {error}"),
            };
            if value["jsonrpc"] != "2.0" {
                break "MCP server emitted a message without jsonrpc 2.0".into();
            }
            if let Some(method) = value["method"].as_str() {
                if let Some(id) = value.get("id") {
                    let response = if method == "ping" {
                        json!({"jsonrpc":"2.0","id":id,"result":{}})
                    } else {
                        json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":format!("Client method {method} is not supported")}})
                    };
                    let _ = responder.send(response.to_string());
                } else if method == "notifications/tools/list_changed" {
                    changed.store(true, Ordering::Release);
                }
            } else if let Some(id) = value["id"].as_u64() {
                if let Some(reply) = reader_pending.lock().unwrap().remove(&id) {
                    let _ = reply.send(Ok(value));
                }
            }
        };
        fail_pending(&reader_pending, &error);
        {
            let mut status = status.lock().unwrap();
            if reader_active.swap(false, Ordering::AcqRel) {
                *status = Status {
                    state: "failed".into(),
                    error: Some(error),
                };
            }
        }
        if let Some(mut child) = reader_child.lock().unwrap().take() {
            #[cfg(unix)]
            unsafe {
                libc::kill(-(child.id() as i32), libc::SIGKILL);
            }
            let _ = child.kill();
            let _ = child.wait();
        };
    });
    let stderr_tail = Arc::new(Mutex::new(String::new()));
    let tail = stderr_tail.clone();
    std::thread::spawn(move || {
        use std::io::Read;
        let mut stderr = stderr;
        let mut chunk = [0u8; 4096];
        while let Ok(count) = stderr.read(&mut chunk) {
            if count == 0 {
                break;
            }
            let mut text = tail.lock().unwrap();
            text.push_str(&String::from_utf8_lossy(&chunk[..count]));
            if text.len() > 8192 {
                let mut cut = text.len() - 8192;
                while !text.is_char_boundary(cut) {
                    cut += 1;
                }
                text.drain(..cut);
            }
        }
    });
    Ok(Local {
        sender,
        pending,
        child,
        active,
        stderr: stderr_tail,
    })
}

fn bounded_line(reader: &mut impl BufRead, limit: usize) -> std::io::Result<Option<Vec<u8>>> {
    let mut out = Vec::new();
    loop {
        let chunk = reader.fill_buf()?;
        if chunk.is_empty() {
            return Ok(if out.is_empty() { None } else { Some(out) });
        }
        let length = chunk
            .iter()
            .position(|b| *b == b'\n')
            .map_or(chunk.len(), |i| i + 1);
        if out.len() + length > limit {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "message exceeds size limit",
            ));
        }
        out.extend_from_slice(&chunk[..length]);
        let newline = chunk[length - 1] == b'\n';
        reader.consume(length);
        if newline {
            return Ok(Some(out));
        }
    }
}

fn fail_pending(pending: &Pending, error: &str) {
    for (_, sender) in pending.lock().unwrap().drain() {
        let _ = sender.send(Err(error.into()));
    }
}

fn create_remote(config: &Value, changed: Arc<AtomicBool>) -> Result<Remote, String> {
    let mut headers = HeaderMap::new();
    if let Some(values) = config.get("headers").filter(|v| !v.is_null()) {
        for (key, value) in values.as_object().ok_or("MCP headers must be an object")? {
            let name = HeaderName::from_bytes(key.as_bytes())
                .map_err(|e| format!("Invalid MCP header name: {e}"))?;
            let value = HeaderValue::from_str(
                value
                    .as_str()
                    .ok_or("MCP headers must contain string values")?,
            )
            .map_err(|e| format!("Invalid MCP header value: {e}"))?;
            headers.insert(name, value);
        }
    }
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(10))
        .build()
        .map_err(|e| format!("MCP HTTP client failed: {e}"))?;
    Ok(Remote {
        client,
        url: config["url"].as_str().unwrap_or_default().into(),
        headers,
        session: Mutex::new(None),
        protocol: Mutex::new(PROTOCOL.into()),
        changed,
        active: AtomicBool::new(true),
    })
}

struct RpcGuard {
    transport: Arc<Transport>,
    id: u64,
    armed: bool,
}

impl Drop for RpcGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let message = json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":self.id,"reason":"Request cancelled or timed out"}});
        match &*self.transport {
            Transport::Local(local) => {
                local.pending.lock().unwrap().remove(&self.id);
                let _ = local.sender.send(message.to_string());
            }
            Transport::Remote(_) => {
                if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                    let transport = self.transport.clone();
                    runtime.spawn(async move {
                        if let Transport::Remote(remote) = &*transport {
                            let _ = tokio::time::timeout(
                                Duration::from_secs(5),
                                remote_send(remote, message, None),
                            )
                            .await;
                        }
                    });
                }
            }
        }
    }
}

async fn rpc(
    server: &Server,
    method: &str,
    params: Value,
    cancel: Option<Arc<AtomicBool>>,
) -> Result<Value, String> {
    let transport = server
        .transport
        .lock()
        .unwrap()
        .clone()
        .ok_or("MCP server is disconnected")?;
    let id = server.next_id.fetch_add(1, Ordering::Relaxed);
    let message = json!({"jsonrpc":"2.0","id":id,"method":method,"params":params});
    let mut guard = RpcGuard {
        transport: transport.clone(),
        id,
        armed: true,
    };
    let future = async {
        match &*transport {
            Transport::Local(local) => {
                let (tx, rx) = oneshot::channel();
                local.pending.lock().unwrap().insert(id, tx);
                local
                    .sender
                    .send(message.to_string())
                    .map_err(|_| "MCP writer stopped".to_owned())?;
                rx.await
                    .map_err(|_| "MCP response channel closed".to_owned())?
            }
            Transport::Remote(remote) => remote_send(remote, message, Some(id)).await,
        }
    };
    let response = tokio::select! {
        result = tokio::time::timeout(server.timeout(), future) => {
            result.map_err(|_| format!("MCP {method} timed out after {}ms", server.timeout().as_millis()))??
        }
        _ = cancelled(cancel) => return Err(format!("MCP {method} cancelled")),
        _ = transport_stopped(transport.clone()) => return Err(format!("MCP {method} cancelled because the server disconnected")),
    };
    guard.armed = false;
    if response["jsonrpc"] != "2.0" || response["id"].as_u64() != Some(id) {
        return Err("MCP response has invalid JSON-RPC version or request id".into());
    }
    if let Some(error) = response.get("error") {
        return Err(format!(
            "MCP {method} error {}: {}",
            error["code"],
            error["message"].as_str().unwrap_or("Unknown RPC error")
        ));
    }
    response
        .get("result")
        .cloned()
        .ok_or_else(|| format!("MCP {method} response omitted result"))
}

async fn transport_stopped(transport: Arc<Transport>) {
    loop {
        let active = match &*transport {
            Transport::Local(local) => local.active.load(Ordering::Acquire),
            Transport::Remote(remote) => remote.active.load(Ordering::Acquire),
        };
        if !active {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

fn end_remote_session(remote: &Remote) {
    let Some(session) = remote.session.lock().unwrap().take() else {
        return;
    };
    let client = remote.client.clone();
    let url = remote.url.clone();
    let headers = remote.headers.clone();
    let protocol = remote.protocol.lock().unwrap().clone();
    let cleanup = async move {
        let _ = client
            .delete(url)
            .headers(headers)
            .header("Mcp-Session-Id", session)
            .header("MCP-Protocol-Version", protocol)
            .timeout(Duration::from_secs(5))
            .send()
            .await;
    };
    if let Ok(runtime) = tokio::runtime::Handle::try_current() {
        runtime.spawn(cleanup);
    } else {
        std::thread::spawn(move || {
            if let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                runtime.block_on(cleanup);
            }
        });
    }
}

async fn cancelled(cancel: Option<Arc<AtomicBool>>) {
    let Some(cancel) = cancel else {
        std::future::pending::<()>().await;
        return;
    };
    while !cancel.load(Ordering::Acquire) {
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

async fn notify(server: &Server, method: &str, params: Value) -> Result<(), String> {
    let transport = server
        .transport
        .lock()
        .unwrap()
        .clone()
        .ok_or("MCP server is disconnected")?;
    let message = json!({"jsonrpc":"2.0","method":method,"params":params});
    match &*transport {
        Transport::Local(local) => local
            .sender
            .send(message.to_string())
            .map_err(|_| "MCP writer stopped".into()),
        Transport::Remote(remote) => {
            tokio::time::timeout(server.timeout(), remote_send(remote, message, None))
                .await
                .map_err(|_| format!("MCP {method} notification timed out"))??;
            Ok(())
        }
    }
}

async fn remote_send(remote: &Remote, message: Value, id: Option<u64>) -> Result<Value, String> {
    let response = remote_request(remote, &message)
        .send()
        .await
        .map_err(|e| format!("MCP HTTP request failed: {}", e.without_url()))?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!(
            "MCP HTTP {} {}",
            status.as_u16(),
            status.canonical_reason().unwrap_or("request failed")
        ));
    }
    if let Some(session) = response.headers().get("Mcp-Session-Id") {
        *remote.session.lock().unwrap() = Some(
            session
                .to_str()
                .map_err(|_| "Invalid MCP session header")?
                .into(),
        );
    }
    if id.is_none() && (status.as_u16() == 202 || status.as_u16() == 204) {
        return Ok(Value::Null);
    }
    if id.is_some() && (status.as_u16() == 202 || status.as_u16() == 204) {
        return Err("MCP server acknowledged a request without returning its result".into());
    }
    let is_sse = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|s| {
            s.split(';')
                .next()
                .unwrap_or("")
                .trim()
                .eq_ignore_ascii_case("text/event-stream")
        });
    let mut stream = response.bytes_stream();
    let mut body = Vec::new();
    let mut decoder = crate::provider::sse::Decoder::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| format!("MCP HTTP body failed: {}", e.without_url()))?;
        if is_sse {
            for frame in decoder
                .feed(&chunk)
                .map_err(|error| error.replace("provider SSE", "MCP SSE"))?
            {
                let mut data = frame.data;
                if let Some(event) = sse_event(&mut data)? {
                    if let Some(response) = handle_remote_event(remote, event, id).await? {
                        return Ok(response);
                    }
                }
            }
        } else {
            if body.len() + chunk.len() > MAX_MESSAGE {
                return Err("MCP response exceeds 16 MiB".into());
            }
            body.extend_from_slice(&chunk);
        }
    }
    if is_sse {
        for frame in decoder
            .finish()
            .map_err(|error| error.replace("provider SSE", "MCP SSE"))?
        {
            let mut data = frame.data;
            if let Some(event) = sse_event(&mut data)? {
                if let Some(response) = handle_remote_event(remote, event, id).await? {
                    return Ok(response);
                }
            }
        }
        if id.is_none() {
            return Ok(Value::Null);
        }
        return Err("MCP event stream ended before the RPC response".into());
    }
    if body.is_empty() && id.is_none() {
        return Ok(Value::Null);
    }
    serde_json::from_slice(&body).map_err(|e| format!("MCP response is not valid JSON: {e}"))
}

fn remote_request(remote: &Remote, message: &Value) -> reqwest::RequestBuilder {
    let mut request = remote
        .client
        .post(&remote.url)
        .headers(remote.headers.clone())
        .header(ACCEPT, "application/json, text/event-stream")
        .header(CONTENT_TYPE, "application/json")
        .header(
            "MCP-Protocol-Version",
            remote.protocol.lock().unwrap().clone(),
        )
        .json(message);
    if let Some(session) = remote.session.lock().unwrap().clone() {
        request = request.header("Mcp-Session-Id", session);
    }
    request
}

fn sse_event(data: &mut String) -> Result<Option<Value>, String> {
    if data.is_empty() {
        return Ok(None);
    }
    let value: Value =
        serde_json::from_str(data).map_err(|e| format!("MCP SSE data is not valid JSON: {e}"))?;
    data.clear();
    Ok(Some(value))
}

async fn handle_remote_event(
    remote: &Remote,
    value: Value,
    id: Option<u64>,
) -> Result<Option<Value>, String> {
    if value["jsonrpc"] != "2.0" {
        return Err("MCP SSE message omitted jsonrpc 2.0".into());
    }
    if let Some(method) = value["method"].as_str() {
        if let Some(request_id) = value.get("id") {
            let response = if method == "ping" {
                json!({"jsonrpc":"2.0","id":request_id,"result":{}})
            } else {
                json!({"jsonrpc":"2.0","id":request_id,"error":{"code":-32601,"message":format!("Client method {method} is not supported")}})
            };
            let result = remote_request(remote, &response)
                .send()
                .await
                .map_err(|e| format!("MCP client response failed: {}", e.without_url()))?;
            if !result.status().is_success() {
                return Err(format!(
                    "MCP client response failed with HTTP {}",
                    result.status().as_u16()
                ));
            }
        } else if method == "notifications/tools/list_changed" {
            remote.changed.store(true, Ordering::Release);
        }
        return Ok(None);
    }
    Ok(if id.is_some() && value["id"].as_u64() == id {
        Some(value)
    } else {
        None
    })
}

pub fn render_result(result: &Value) -> String {
    let mut output = Vec::new();
    if let Some(content) = result["content"].as_array() {
        for item in content {
            if item["type"] == "text" {
                output.push(item["text"].as_str().unwrap_or_default().to_owned());
            } else if item["type"] == "resource" && item["resource"]["text"].is_string() {
                output.push(format!(
                    "{}\n{}",
                    item["resource"]["uri"].as_str().unwrap_or("Resource"),
                    item["resource"]["text"].as_str().unwrap()
                ));
            } else if matches!(item["type"].as_str(), Some("image" | "audio")) {
                output.push(format!(
                    "[{} attachment: {}]",
                    item["type"].as_str().unwrap(),
                    item["mimeType"].as_str().unwrap_or("unknown media type")
                ));
            } else if item["type"] == "resource" && item["resource"]["blob"].is_string() {
                output.push(format!(
                    "[Resource attachment: {} ({})]",
                    item["resource"]["uri"].as_str().unwrap_or("unnamed"),
                    item["resource"]["mimeType"]
                        .as_str()
                        .unwrap_or("application/octet-stream")
                ));
            } else {
                output.push(item.to_string());
            }
        }
    }
    if let Some(structured) = result.get("structuredContent") {
        output.push(structured.to_string());
    }
    if output.is_empty() {
        result.to_string()
    } else {
        output.join("\n")
    }
}

fn blocking<F, T>(future: F) -> Result<T, String>
where
    F: std::future::Future<Output = Result<T, String>> + Send + 'static,
    T: Send + 'static,
{
    std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| format!("MCP runtime failed: {e}"))?
            .block_on(future)
    })
    .join()
    .map_err(|_| "MCP worker panicked".to_owned())?
}

#[cfg(test)]
mod tests;
