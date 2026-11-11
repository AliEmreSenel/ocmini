//! Stdio and HTTP transports own connection I/O, cancellation and process cleanup.
use super::{
    ConnectionState, EventScope, Local, Pending, Remote, Server, Status, Transport, MAX_MESSAGE,
    PROTOCOL,
};
use futures::StreamExt;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, ACCEPT, CONTENT_TYPE};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc, Arc, Mutex,
};
use std::time::Duration;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

pub(super) fn create_local(
    config: &Value,
    status: Arc<Mutex<Status>>,
    changed: Arc<AtomicBool>,
    events: EventScope,
    server_name: String,
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
    let stopped = CancellationToken::new();
    let reader_child = child.clone();
    let reader_stopped = stopped.clone();
    let reader_status = status.clone();
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
            let mut status = reader_status.lock().unwrap();
            if !reader_stopped.is_cancelled() {
                reader_stopped.cancel();
                *status = Status {
                    state: ConnectionState::Failed,
                    error: Some(error),
                };
                drop(status);
                events.emit(&server_name, ConnectionState::Failed);
            }
        }
        if let Some(mut child) = reader_child.lock().unwrap().take() {
            crate::process::kill_group(child.id());
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
        stopped,
        status,
        stderr: stderr_tail,
    })
}

pub(super) fn bounded_line(
    reader: &mut impl BufRead,
    limit: usize,
) -> std::io::Result<Option<Vec<u8>>> {
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

pub(super) fn fail_pending(pending: &Pending, error: &str) {
    for (_, sender) in pending.lock().unwrap().drain() {
        let _ = sender.send(Err(error.into()));
    }
}

pub(super) fn create_remote(config: &Value, changed: Arc<AtomicBool>) -> Result<Remote, String> {
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
        stopped: CancellationToken::new(),
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

pub(super) async fn rpc(
    server: &Server,
    method: &str,
    params: Value,
    cancel: Option<CancellationToken>,
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
    match &*transport {
        Transport::Local(local) => local.stopped.cancelled().await,
        Transport::Remote(remote) => remote.stopped.cancelled().await,
    }
}

pub(super) fn end_remote_session(remote: &Remote) {
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

async fn cancelled(cancel: Option<CancellationToken>) {
    match cancel {
        Some(cancel) => cancel.cancelled().await,
        None => std::future::pending().await,
    }
}

pub(super) async fn notify(server: &Server, method: &str, params: Value) -> Result<(), String> {
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
    if is_sse {
        let mut frames = crate::provider::sse::frames(response, None);
        while let Some(frame) = frames.next().await {
            let frame = frame.map_err(|error| error.replace("provider", "MCP"))?;
            if frame.data.trim().is_empty() {
                continue;
            }
            let event = serde_json::from_str(&frame.data)
                .map_err(|error| format!("MCP SSE data is not valid JSON: {error}"))?;
            if let Some(response) = handle_remote_event(remote, event, id).await? {
                return Ok(response);
            }
        }
        return if id.is_none() {
            Ok(Value::Null)
        } else {
            Err("MCP event stream ended before the RPC response".into())
        };
    }
    let body = crate::http::read_body(response, None, MAX_MESSAGE).await?;
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
