use crate::events as event;
use crate::types::stable_hash;
use tokio_util::sync::CancellationToken;
mod transport;
use futures::StreamExt;
use reqwest::header::HeaderMap;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::process::Child;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;
use tokio::sync::oneshot;
#[cfg(test)]
use transport::bounded_line;
use transport::{create_local, create_remote, end_remote_session, fail_pending, notify, rpc};

const PROTOCOL: &str = "2025-03-26";
const MAX_MESSAGE: usize = 16 * 1024 * 1024;
type Reply = Result<Value, String>;
type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Reply>>>>;

#[derive(Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ConnectionState {
    #[default]
    Disconnected,
    Connecting,
    Connected,
    Disabled,
    Failed,
    NeedsAuth,
}

#[derive(Default)]
struct Status {
    state: ConnectionState,
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
    stopped: CancellationToken,
    status: Arc<Mutex<Status>>,
    stderr: Arc<Mutex<String>>,
}

impl Local {
    fn stop(&self) {
        {
            let _status = self.status.lock().unwrap();
            self.stopped.cancel();
        }
        if let Some(mut child) = self.child.lock().unwrap().take() {
            crate::process::kill_group(child.id());
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
    stopped: CancellationToken,
}

enum Transport {
    Local(Local),
    Remote(Remote),
}

#[derive(Clone)]
struct ScopedHooks {
    hooks: std::sync::Weak<crate::hooks::Hooks>,
    directory: String,
}

#[derive(Clone, Default)]
struct EventScope {
    target: Arc<Mutex<Option<ScopedHooks>>>,
    catalog_version: Arc<AtomicU64>,
}
impl EventScope {
    fn emit(&self, name: &str, status: ConnectionState) {
        self.catalog_version.fetch_add(1, Ordering::AcqRel);
        let target = self.target.lock().unwrap().clone();
        if let Some(target) = target {
            let context = json!({"directory":target.directory});
            if let Some(hooks) = target.hooks.upgrade() {
                hooks.emit(
                    event::Mcp::Status {
                        server: name,
                        status,
                    },
                    &context,
                );
            } else {
                crate::engine::bus().emit(
                    event::Mcp::Status {
                        server: name,
                        status,
                    },
                    json!({"version":1,"directory":target.directory,"time":crate::types::now_ms()}),
                );
            }
        }
    }
}

#[derive(Default)]
struct Refresh {
    attempted: Option<std::time::Instant>,
    error: Option<String>,
}
impl Refresh {
    async fn run(&mut self, server: &Server) -> Result<(), String> {
        self.attempted = Some(std::time::Instant::now());
        let result = refresh_tools(server).await;
        self.error = result.as_ref().err().cloned();
        if result.is_err() {
            server.changed.store(true, Ordering::Release);
        }
        result
    }
}

struct Server {
    name: String,
    events: EventScope,
    config: Value,
    status: Arc<Mutex<Status>>,
    tools: Mutex<Vec<Value>>,
    transport: Mutex<Option<Arc<Transport>>>,
    changed: Arc<AtomicBool>,
    supports_tools: AtomicBool,
    next_id: AtomicU64,
    generation: AtomicU64,
    connect_lock: tokio::sync::Mutex<()>,
    refresh: tokio::sync::Mutex<Refresh>,
}

impl Server {
    fn set_status(&self, state: ConnectionState, error: Option<String>) {
        self.set_status_for(self.generation.load(Ordering::Acquire), state, error);
    }

    fn set_status_for(&self, generation: u64, state: ConnectionState, error: Option<String>) {
        let changed = {
            let mut status = self.status.lock().unwrap();
            if self.generation.load(Ordering::Acquire) != generation {
                return;
            }
            let changed = status.state != state || status.error != error;
            *status = Status { state, error };
            changed
        };
        if changed {
            self.events.emit(&self.name, state);
        }
    }

    fn timeout(&self) -> Duration {
        Duration::from_millis(self.config["timeout"].as_u64().unwrap_or(30_000).max(1))
    }

    fn stop(&self) {
        let transport = self.transport.lock().unwrap().take();
        if let Some(transport) = transport {
            match &*transport {
                Transport::Local(local) => local.stop(),
                Transport::Remote(remote) => {
                    remote.stopped.cancel();
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
    events: EventScope,
    catalog: Arc<Mutex<Catalog>>,
}

#[derive(Default)]
struct Catalog {
    version: u64,
    routes: HashMap<String, (String, String)>,
    definitions: Vec<Value>,
}

impl McpHub {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_hooks(&self, hooks: Arc<crate::hooks::Hooks>, directory: &str) {
        *self.events.target.lock().unwrap() = Some(ScopedHooks {
            hooks: Arc::downgrade(&hooks),
            directory: directory.into(),
        });
    }

    pub async fn add_async(
        &self,
        name: &str,
        config: Value,
    ) -> Result<Value, crate::types::OperationError> {
        validate_config(name, &config).map_err(crate::types::OperationError::invalid)?;
        let server = Arc::new(Server {
            name: name.into(),
            events: self.events.clone(),
            config,
            status: Arc::new(Mutex::new(Status {
                state: ConnectionState::Disconnected,
                error: None,
            })),
            tools: Mutex::new(Vec::new()),
            transport: Mutex::new(None),
            changed: Arc::new(AtomicBool::new(false)),
            supports_tools: AtomicBool::new(false),
            next_id: AtomicU64::new(1),
            generation: AtomicU64::new(0),
            connect_lock: tokio::sync::Mutex::new(()),
            refresh: tokio::sync::Mutex::new(Refresh::default()),
        });
        let old = self
            .servers
            .lock()
            .unwrap()
            .insert(name.into(), server.clone());
        if let Some(old) = old {
            old.generation.fetch_add(1, Ordering::AcqRel);
            old.set_status(ConnectionState::Disabled, None);
            old.stop();
        }
        if server.config["enabled"] == false {
            server.set_status(ConnectionState::Disabled, None);
        } else {
            connect(&server).await;
        }
        Ok(named_status(name, &server))
    }

    pub async fn connect_async(&self, name: &str) -> Result<Value, crate::types::OperationError> {
        let server = self.server(name)?;
        connect(&server).await;
        Ok(named_status(name, &server))
    }

    pub fn disconnect(&self, name: &str) -> Result<Value, crate::types::OperationError> {
        let server = self.server(name)?;
        server.generation.fetch_add(1, Ordering::AcqRel);
        server.stop();
        server.set_status(ConnectionState::Disabled, None);
        Ok(named_status(name, &server))
    }

    fn server(&self, name: &str) -> Result<Arc<Server>, crate::types::OperationError> {
        self.servers
            .lock()
            .unwrap()
            .get(name)
            .cloned()
            .ok_or_else(|| {
                crate::types::OperationError::not_found(format!(
                    "MCP server {name:?} is not registered"
                ))
            })
    }

    pub async fn call_result_async(
        &self,
        name: &str,
        tool: &str,
        args: Value,
        cancel: CancellationToken,
    ) -> Result<Value, String> {
        let server = self.server(name)?;
        let generation = server.generation.load(Ordering::Acquire);
        if server.status.lock().unwrap().state != ConnectionState::Connected {
            return Err(format!("MCP server {name:?} is not connected"));
        }
        if let Err(error) = refresh_if_needed(&server).await {
            eprintln!(
                "ocmini: MCP catalog {name} refresh failed; using the previous catalog: {error}"
            );
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
                    server.set_status_for(
                        generation,
                        ConnectionState::NeedsAuth,
                        Some(error.clone()),
                    );
                } else if error.starts_with("MCP HTTP ")
                    || error.starts_with("MCP event stream ended")
                {
                    server.set_status_for(generation, ConnectionState::Failed, Some(error.clone()));
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
        futures::stream::iter(servers)
            .for_each_concurrent(8, |server| async move {
                if let Err(error) = refresh_if_needed(&server).await {
                    eprintln!(
                        "ocmini: MCP catalog {} refresh failed: {error}",
                        server.name
                    );
                }
            })
            .await;
        self.tool_defs()
    }

    pub fn tool_defs(&self) -> Vec<Value> {
        self.with_catalog(|catalog| catalog.definitions.clone())
    }

    fn with_catalog<T>(&self, read: impl FnOnce(&Catalog) -> T) -> T {
        let version = self.events.catalog_version.load(Ordering::Acquire);
        let mut catalog = self.catalog.lock().unwrap();
        if catalog.version != version {
            *catalog = self.build_catalog(version);
        }
        read(&catalog)
    }

    fn build_catalog(&self, version: u64) -> Catalog {
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
        let mut catalog = Catalog {
            version,
            ..Default::default()
        };
        for (server_name, server) in self.servers.lock().unwrap().iter() {
            if server.status.lock().unwrap().state != ConnectionState::Connected {
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
                    let hash =
                        stable_hash(&format!("{}:{server_name}{tool_name}", server_name.len()));
                    name.truncate(47);
                    name.push_str(&format!("_{hash:016x}"));
                    let base = name.clone();
                    let mut collision = 0;
                    while !used.insert(name.clone()) {
                        collision += 1;
                        name = format!("{}_{collision}", &base[..base.len().min(54)]);
                    }
                }
                catalog.definitions.push(json!({"type":"function","function":{
                    "name":name,"description":tool["description"].as_str().unwrap_or("MCP tool"),
                    "parameters":tool.get("inputSchema").cloned().unwrap_or_else(|| json!({"type":"object","properties":{}}))
                }}));
                catalog
                    .routes
                    .insert(name, (server_name.clone(), tool_name.into()));
            }
        }
        catalog
    }

    pub fn route(&self, name: &str) -> Option<(String, String)> {
        self.with_catalog(|catalog| {
            catalog
                .routes
                .get(name)
                .or_else(|| {
                    name.strip_prefix("mcp_")
                        .and_then(|alias| catalog.routes.get(alias))
                })
                .cloned()
        })
    }
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
        if timeout.as_u64().is_none_or(|value| value == 0) {
            return Err("MCP timeout must be a positive integer in milliseconds".into());
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
                ConnectionState::Failed,
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
    server.set_status(ConnectionState::Connecting, None);
    let transport = if transport_kind(&server.config) == "remote" {
        create_remote(&server.config, server.changed.clone()).map(Transport::Remote)
    } else {
        create_local(
            &server.config,
            server.status.clone(),
            server.changed.clone(),
            server.events.clone(),
            server.name.clone(),
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
                    Transport::Local(local) => !local.stopped.is_cancelled(),
                    Transport::Remote(remote) => !remote.stopped.is_cancelled(),
                });
            if live && server.generation.load(Ordering::Acquire) == generation {
                *status = Status {
                    state: ConnectionState::Connected,
                    error: None,
                };
                drop(status);
                server.events.emit(&server.name, ConnectionState::Connected);
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
                ConnectionState::NeedsAuth
            } else {
                ConnectionState::Failed
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
        server.refresh.lock().await.run(server).await?;
    }
    Ok(())
}

async fn refresh_if_needed(server: &Server) -> Result<(), String> {
    let mut refresh = server.refresh.lock().await;
    if server.status.lock().unwrap().state != ConnectionState::Connected
        || !server.supports_tools.load(Ordering::Acquire)
    {
        return Ok(());
    }
    let recent = refresh
        .attempted
        .is_some_and(|at| at.elapsed() < Duration::from_secs(3));
    if recent {
        if let Some(error) = &refresh.error {
            return Err(error.clone());
        }
    }
    let remote_due = transport_kind(&server.config) == "remote" && !recent;
    if server.changed.swap(false, Ordering::AcqRel) || remote_due {
        refresh.run(server).await?;
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
                server.events.catalog_version.fetch_add(1, Ordering::AcqRel);
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

#[cfg(test)]
mod tests;
