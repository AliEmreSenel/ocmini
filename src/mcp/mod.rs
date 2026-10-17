use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

struct Server {
    config: Value,
    status: String,
    tools: Vec<Value>,
    to_child: Option<Sender<String>>,
    pending: Arc<Mutex<HashMap<u64, Sender<Value>>>>,
    next_id: u64,
}

pub struct McpHub {
    servers: Mutex<HashMap<String, Server>>,
    children: Arc<Mutex<Vec<Child>>>,
}

impl McpHub {
    pub fn new() -> Self {
        Self {
            servers: Mutex::new(HashMap::new()),
            children: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn add(&self, name: &str, config: Value) -> Result<Value, String> {
        let kind = config["type"].as_str().unwrap_or("local");
        let mut server = Server {
            config: config.clone(),
            status: "disconnected".into(),
            tools: Vec::new(),
            to_child: None,
            pending: Arc::new(Mutex::new(HashMap::new())),
            next_id: 1,
        };
        if kind == "remote" {
            server.status = "connected".into();
            self.servers.lock().unwrap().insert(name.to_string(), server);
            return Ok(json!({"name": name, "status": "connected"}));
        }
        let cmd = config["command"]
            .as_array()
            .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect::<Vec<_>>())
            .unwrap_or_default();
        if cmd.is_empty() {
            return Err("mcp: empty command".into());
        }
        let mut c = Command::new(&cmd[0]);
        c.args(&cmd[1..]);
        c.stdin(Stdio::piped());
        c.stdout(Stdio::piped());
        c.stderr(Stdio::null());
        if let Some(env) = config["environment"].as_object() {
            for (k, v) in env {
                if let Some(v) = v.as_str() {
                    c.env(k, v);
                }
            }
        }
        let mut child = match c.spawn() {
            Ok(ch) => ch,
            Err(e) => {
                server.status = "failed".into();
                self.servers.lock().unwrap().insert(name.to_string(), server);
                return Ok(json!({"name": name, "status": "failed", "error": e.to_string()}));
            }
        };
        let mut stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, rx) = mpsc::channel::<String>();
        std::thread::spawn(move || {
            while let Ok(line) = rx.recv() {
                if writeln!(stdin, "{line}").and_then(|_| stdin.flush()).is_err() {
                    break;
                }
            }
        });
        let (reply_tx, reply_rx) = mpsc::channel::<String>();
        let pending = server.pending.clone();
        let _ = reply_tx.clone();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
                let t = line.trim().to_string();
                if t.is_empty() {
                    continue;
                }
                if let Ok(v) = serde_json::from_str::<Value>(&t) {
                    if let Some(id) = v.get("id").and_then(|i| i.as_u64()) {
                        if let Some(s) = pending.lock().unwrap().remove(&id) {
                            let _ = s.send(v);
                        }
                    }
                }
            }
            drop(reply_rx);
        });
        server.to_child = Some(tx);
        let rpc = |s: &mut Server, method: &str, params: Value, timeout: Duration| -> Result<Value, String> {
            let id = s.next_id;
            s.next_id += 1;
            let msg = json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}).to_string();
            let (rtx, rrx) = mpsc::channel();
            s.pending.lock().unwrap().insert(id, rtx);
            s.to_child.as_ref().ok_or("mcp: not running")?.send(msg).map_err(|e| e.to_string())?;
            let resp = match rrx.recv_timeout(timeout) {
                Ok(v) => v,
                Err(_) => {
                    s.pending.lock().unwrap().remove(&id);
                    return Err(format!("mcp: {method} timeout"));
                }
            };
            if let Some(e) = resp.get("error") {
                return Err(e["message"].as_str().unwrap_or("rpc error").to_string());
            }
            Ok(resp.get("result").cloned().unwrap_or(json!({})))
        };
        let init = rpc(
            &mut server,
            "initialize",
            json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "ocmini", "version": "0.1"}
            }),
            Duration::from_secs(5),
        );
        match init {
            Ok(_) => {
                if let Some(tx) = &server.to_child {
                    let _ = tx.send(json!({"jsonrpc":"2.0","method":"notifications/initialized"}).to_string());
                }
                if let Ok(res) = rpc(&mut server, "tools/list", json!({}), Duration::from_secs(10)) {
                    server.tools = res["tools"].as_array().cloned().unwrap_or_default();
                }
                server.status = "connected".into();
            }
            Err(_) => {
                server.status = "failed".into();
            }
        }
        self.children.lock().unwrap().push(child);
        let status = server.status.clone();
        self.servers.lock().unwrap().insert(name.to_string(), server);
        Ok(json!({"name": name, "status": status}))
    }

    pub fn call(&self, server: &str, tool: &str, args: Value) -> Result<String, String> {
        let (tx, id) = {
            let mut servers = self.servers.lock().unwrap();
            let s = servers.get_mut(server).ok_or("mcp: unknown server")?;
            let id = s.next_id;
            s.next_id += 1;
            let (rtx, rrx) = mpsc::channel();
            s.pending.lock().unwrap().insert(id, rtx);
            let msg = json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":tool,"arguments":args}});
            s.to_child
                .as_ref()
                .ok_or("mcp: server not running")?
                .send(msg.to_string())
                .map_err(|e| e.to_string())?;
            (rrx, id)
        };
        let resp = tx.recv_timeout(Duration::from_secs(120)).map_err(|_| "mcp: call timeout".to_string())?;
        if let Some(s) = self.servers.lock().unwrap().get(server) {
            s.pending.lock().unwrap().remove(&id);
        }
        if let Some(e) = resp.get("error") {
            return Err(e["message"].as_str().unwrap_or("rpc error").to_string());
        }
        let result = resp.get("result").cloned().unwrap_or(json!({}));
        Ok(result["content"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter(|c| c["type"] == "text")
                    .filter_map(|c| c["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_else(|| result.to_string()))
    }

    pub fn status(&self) -> Value {
        let map: serde_json::Map<String, Value> = self
            .servers
            .lock()
            .unwrap()
            .iter()
            .map(|(n, s)| (n.clone(), json!({"status": s.status})))
            .collect();
        Value::Object(map)
    }

    pub fn tool_defs(&self) -> Vec<Value> {
        let mut out = Vec::new();
        for (server, s) in self.servers.lock().unwrap().iter() {
            if s.status != "connected" {
                continue;
            }
            for t in &s.tools {
                out.push(json!({
                    "type": "function",
                    "function": {
                        "name": format!("mcp_{server}_{}", t["name"]),
                        "description": t["description"],
                        "parameters": t["inputSchema"],
                    }
                }));
            }
        }
        out
    }

    pub fn route(&self, name: &str) -> Option<(String, String)> {
        let rest = name.strip_prefix("mcp_")?;
        let (server, tool) = rest.split_once('_')?;
        Some((server.to_string(), tool.to_string()))
    }

    pub fn has(&self, name: &str) -> bool {
        self.servers.lock().unwrap().contains_key(name)
    }
}

impl Default for McpHub {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for McpHub {
    fn drop(&mut self) {
        for mut c in self.children.lock().unwrap().drain(..) {
            let _ = c.kill();
        }
    }
}

#[allow(dead_code)]
fn unused(_: TryRecvError, _: Instant) {}
