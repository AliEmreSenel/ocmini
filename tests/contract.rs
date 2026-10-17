use axum::extract::State;
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use base64::Engine;
use futures::StreamExt;
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, oneshot, Mutex};

const PASSWORD: &str = "t3-test-pässword";

struct Script {
    chunks: Vec<Value>,
    gate: Option<oneshot::Receiver<()>>,
    status: Option<u16>,
    retry_after: Option<String>,
    terminate: bool,
}

impl Script {
    fn ok(chunks: Vec<Value>) -> Self { Self { chunks, gate: None, status: None, retry_after: None, terminate: true } }
    fn http_status(status: u16, retry_after: Option<&str>) -> Self {
        Self { chunks: vec![], gate: None, status: Some(status), retry_after: retry_after.map(str::to_owned), terminate: true }
    }
    fn mid_stream_error(message: &str) -> Self {
        Self::ok(vec![
            json!({"choices": [{"index": 0, "delta": {"content": "partial"}, "finish_reason": null}]}),
            json!({"error": {"message": message}}),
        ])
    }
    fn truncated() -> Self {
        Self { chunks: vec![json!({"choices": [{"index": 0, "delta": {"content": "Incomplete response"}, "finish_reason": null}]})], gate: None, status: None, retry_after: None, terminate: false }
    }
}

fn usage() -> Value {
    json!({"prompt_tokens": 17, "completion_tokens": 9,
        "prompt_tokens_details": {"cached_tokens": 3},
        "completion_tokens_details": {"reasoning_tokens": 2}})
}

fn text_script(first: &str, second: &str) -> Script {
    Script::ok(vec![
        json!({"id": "completion-test", "choices": [{"index": 0, "delta": {"role": "assistant", "content": first}, "finish_reason": null}]}),
        json!({"id": "completion-test", "choices": [{"index": 0, "delta": {"content": second}, "finish_reason": null}]}),
        json!({"id": "completion-test", "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}], "usage": usage()}),
    ])
}

fn tool_script(name: &str, arguments: Value, id: &str) -> Script {
    Script::ok(vec![
        json!({"choices": [{"index": 0, "delta": {"tool_calls": [{"index": 0, "id": id, "type": "function", "function": {"name": name, "arguments": arguments.to_string()}}]}, "finish_reason": null}]}),
        json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}], "usage": usage()}),
    ])
}

#[derive(Clone)]
struct ProviderState {
    scripts: Arc<Mutex<VecDeque<Script>>>,
    requests: mpsc::UnboundedSender<Value>,
}

async fn provider_reply(State(state): State<ProviderState>, Json(request): Json<Value>) -> Response {
    state.requests.send(request).unwrap();
    let Some(script) = state.scripts.lock().await.pop_front() else {
        return (axum::http::StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": {"message": "Unexpected extra model request"}}))).into_response();
    };
    if let Some(status) = script.status {
        let status = axum::http::StatusCode::from_u16(status).expect("test status");
        let mut response = (status, Json(json!({"error": {"message": "provider is having a bad day"}}))).into_response();
        if let Some(value) = script.retry_after {
            if let Ok(value) = axum::http::HeaderValue::from_str(&value) { response.headers_mut().insert("retry-after", value); }
        }
        return response;
    }
    let terminate = script.terminate;
    let stream = async_stream::stream! {
        let mut gate = script.gate;
        for (index, chunk) in script.chunks.into_iter().enumerate() {
            if index == 1 { if let Some(gate) = gate.take() { let _ = gate.await; } }
            yield Ok::<Event, std::convert::Infallible>(Event::default().data(chunk.to_string()));
        }
        if terminate { yield Ok(Event::default().data("[DONE]")); }
    };
    Sse::new(stream).into_response()
}

struct Provider {
    url: String,
    requests: mpsc::UnboundedReceiver<Value>,
    task: tokio::task::JoinHandle<()>,
}

impl Provider {
    async fn start(scripts: Vec<Script>) -> Self {
        let (requests, receiver) = mpsc::unbounded_channel();
        let state = ProviderState { scripts: Arc::new(Mutex::new(scripts.into())), requests };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        let router = Router::new().route("/v1/chat/completions", post(provider_reply)).with_state(state);
        let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap(); });
        Self { url, requests: receiver, task }
    }

    async fn request(&mut self) -> Value {
        tokio::time::timeout(Duration::from_secs(10), self.requests.recv()).await.expect("model request timed out").expect("model server closed")
    }
}

impl Drop for Provider {
    fn drop(&mut self) { self.task.abort(); }
}

fn encode_directory(path: &Path) -> String {
    path.to_string_lossy().as_bytes().iter().map(|byte| {
        if byte.is_ascii_alphanumeric() || b"-_.~".contains(byte) { (*byte as char).to_string() } else { format!("%{byte:02X}") }
    }).collect()
}

fn configured_command(root: &Path, directory: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_ocmini"));
    command.env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", root)
        .env("XDG_CONFIG_HOME", root.join(".config"))
        .env("XDG_DATA_HOME", root.join(".local/share"))
        .env("OPENCODE_CONFIG_CONTENT", "{}")
        .env("OPENCODE_AUTH_CONTENT", "{}")
        .env("OPENCODE_SERVER_PASSWORD", PASSWORD)
        .env("OCMINI_RETRY_BASE_MS", "50")
        .current_dir(directory);
    command
}

async fn launch(root: &Path, directory: &Path) -> (Child, String) {
    let mut child = configured_command(root, directory)
        .args(["serve", "--hostname=127.0.0.1", "--port=0"])
        .stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::inherit())
        .kill_on_drop(true).spawn().unwrap();
    let stdout = child.stdout.take().unwrap();
    let mut lines = BufReader::new(stdout).lines();
    let url = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let line = lines.next_line().await.unwrap().expect("server exited before its banner");
            if let Some(url) = line.strip_prefix("opencode server listening on ") { break url.to_owned(); }
        }
    }).await.expect("server startup timed out");
    (child, url)
}

struct TestServer {
    child: Child,
    url: String,
    root: PathBuf,
    directory: PathBuf,
    client: reqwest::Client,
}

impl TestServer {
    async fn start(provider: &Provider, extra: Value) -> Self {
        let root = std::env::temp_dir().join(format!("ocmini-contract-{}-{:016x}", std::process::id(), rand::random::<u64>()));
        let directory = root.join("project café %");
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::create_dir_all(root.join(".config/opencode")).unwrap();
        std::fs::create_dir_all(root.join(".local/share")).unwrap();
        let mut config = json!({
            "model": "mock/test-model",
            "provider": {"mock": {"npm": "@ai-sdk/openai-compatible", "name": "Local test model",
                "options": {"baseURL": provider.url},
                "models": {"test-model": {"name": "Test model", "limit": {"context": 32000, "output": 2048}, "variants": {"high": {"reasoningEffort": "high"}}}}
            }},
            "command": {"review": {"description": "Review arguments", "template": "First $1. Rest $2."}},
            "agent": {"reviewer": {"description": "Review code", "mode": "primary", "prompt": "REVIEWER_SYSTEM"}}
        });
        for (key, value) in extra.as_object().unwrap() { config[key] = value.clone(); }
        std::fs::write(root.join(".config/opencode/opencode.json"), config.to_string()).unwrap();
        let (child, url) = launch(&root, &directory).await;
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(reqwest::header::AUTHORIZATION, format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(format!("opencode:{PASSWORD}"))).parse().unwrap());
        headers.insert("x-opencode-directory", encode_directory(&directory).parse().unwrap());
        let client = reqwest::Client::builder().default_headers(headers).timeout(Duration::from_secs(20)).build().unwrap();
        Self { child, url, root, directory, client }
    }

    fn get(&self, path: &str) -> reqwest::RequestBuilder { self.client.get(format!("{}{path}", self.url)) }
    fn post(&self, path: &str, body: &Value) -> reqwest::RequestBuilder { self.client.post(format!("{}{path}", self.url)).json(body) }

    async fn session(&self, permission: Value) -> String {
        let response = self.post("/session", &json!({"permission": permission})).send().await.unwrap();
        assert_eq!(response.status(), 200);
        response.json::<Value>().await.unwrap()["id"].as_str().unwrap().to_owned()
    }

    async fn messages(&self, id: &str) -> Vec<Value> {
        self.get(&format!("/session/{id}/message")).send().await.unwrap().error_for_status().unwrap().json().await.unwrap()
    }

    async fn restart(&mut self) {
        self.child.kill().await.unwrap();
        self.child.wait().await.unwrap();
        let (child, url) = launch(&self.root, &self.directory).await;
        self.child = child;
        self.url = url;
    }

    async fn events(&self) -> Events {
        let response = self.get("/event").send().await.unwrap().error_for_status().unwrap();
        assert!(response.headers()["content-type"].to_str().unwrap().starts_with("text/event-stream"));
        let (send, receive) = mpsc::unbounded_channel();
        let task = tokio::spawn(async move {
            let mut stream = response.bytes_stream();
            let mut bytes = Vec::new();
            while let Some(chunk) = stream.next().await {
                let Ok(chunk) = chunk else { break; };
                bytes.extend_from_slice(&chunk);
                while let Some(end) = bytes.windows(2).position(|pair| pair == b"\n\n") {
                    let frame: Vec<_> = bytes.drain(..end + 2).collect();
                    let frame = String::from_utf8(frame).unwrap();
                    let data = frame.lines().filter_map(|line| line.strip_prefix("data:").map(str::trim_start)).collect::<Vec<_>>().join("\n");
                    if !data.is_empty() && send.send(serde_json::from_str(&data).unwrap()).is_err() { return; }
                }
            }
        });
        let mut events = Events { receive, task };
        assert_eq!(events.next().await["type"], "server.connected");
        events
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

struct Events {
    receive: mpsc::UnboundedReceiver<Value>,
    task: tokio::task::JoinHandle<()>,
}

impl Events {
    async fn next(&mut self) -> Value {
        tokio::time::timeout(Duration::from_secs(10), self.receive.recv()).await.expect("SSE event timed out").expect("SSE stream ended unexpectedly")
    }

    async fn through(&mut self, event_type: &str) -> Vec<Value> {
        let mut result = Vec::new();
        loop {
            let event = self.next().await;
            let matched = event["type"] == event_type;
            result.push(event);
            if matched { return result; }
            assert!(result.len() < 10000, "event stream never reached {event_type}");
        }
    }
}

impl Drop for Events { fn drop(&mut self) { self.task.abort(); } }

fn allow() -> Value { json!([{"permission": "*", "pattern": "*", "action": "allow"}]) }

fn prompt(id: &str, text: &str) -> Value {
    json!({"messageID": id, "model": {"providerID": "mock", "modelID": "test-model"}, "parts": [{"type": "text", "text": text}]})
}

#[tokio::test]
async fn startup_auth_and_cli_inventory_match_t3() {
    let provider = Provider::start(vec![]).await;
    let server = TestServer::start(&provider, json!({})).await;
    assert!(!server.url.ends_with(":0"));
    let health: Value = server.get("/global/health").send().await.unwrap().json().await.unwrap();
    assert_eq!(health["healthy"], true);
    assert_eq!(health["version"], "1.19.2-ocmini.0.2.0");
    assert_eq!(server.get("/api/info").send().await.unwrap().status(), 404);
    for path in ["/global/health", "/api/info", "/event", "/session"] {
        assert_eq!(reqwest::get(format!("{}{path}", server.url)).await.unwrap().status(), 401, "{path}");
    }
    let inventory: Value = server.get("/provider").send().await.unwrap().json().await.unwrap();
    assert_eq!(inventory["connected"], json!(["mock"]));
    assert_eq!(inventory["all"][0]["models"]["test-model"]["id"], "test-model");
    assert!(inventory["all"][0]["models"]["test-model"]["variants"]["high"].is_object());
    let output = configured_command(&server.root, &server.directory).args(["models", "--verbose"]).output().await.unwrap();
    assert!(output.status.success());
    let output = String::from_utf8(output.stdout).unwrap();
    let mut lines = output.lines();
    assert_eq!(lines.next(), Some("mock/test-model"));
    let model: Value = serde_json::from_str(lines.next().unwrap()).unwrap();
    assert_eq!(model["id"], "test-model");
    assert_eq!(model["limit"]["context"], 32000);
    let output = configured_command(&server.root, &server.directory).args(["agent", "list"]).output().await.unwrap();
    assert!(output.status.success());
    let output = String::from_utf8(output.stdout).unwrap();
    assert!(output.contains("reviewer (primary)"));
    for pair in output.lines().collect::<Vec<_>>().chunks(2) {
        assert!(pair[0].ends_with(')'));
        assert!(serde_json::from_str::<Value>(pair[1]).unwrap().is_array());
    }
    let output = configured_command(&server.root, &server.directory).args(["serve", "--port", "garbage"]).output().await.unwrap();
    assert!(!output.status.success());
}

#[tokio::test]
async fn session_scoping_and_validation_are_consistent() {
    let provider = Provider::start(vec![]).await;
    let server = TestServer::start(&provider, json!({})).await;
    let id = server.session(allow()).await;
    let session: Value = server.get(&format!("/session/{id}")).send().await.unwrap().json().await.unwrap();
    assert_eq!(session["directory"], server.directory.to_string_lossy().as_ref());
    let other = server.root.join("other-project");
    std::fs::create_dir_all(&other).unwrap();
    let response = server.get(&format!("/session/{id}")).header("x-opencode-directory", encode_directory(&other)).send().await.unwrap();
    assert_eq!(response.status(), 404);
    let all: Vec<Value> = server.get("/session").query(&[("directory", server.directory.to_string_lossy().as_ref())]).send().await.unwrap().json().await.unwrap();
    assert_eq!(all.len(), 1);
    assert_eq!(server.post("/session/missing/prompt_async", &prompt("msg_missing", "hello")).send().await.unwrap().status(), 404);
    assert_eq!(server.post(&format!("/session/{id}/prompt_async"), &json!({"parts": [{"type": "file", "url": "file:///tmp/x"}]})).send().await.unwrap().status(), 400);
    assert_eq!(server.post(&format!("/session/{id}/command"), &json!({"command": "missing", "arguments": ""})).send().await.unwrap().status(), 404);
    assert_eq!(server.post("/permission/missing/reply", &json!({"reply": "invalid"})).send().await.unwrap().status(), 400);
    assert_eq!(server.post("/permission/missing/reply", &json!({"reply": "once"})).send().await.unwrap().status(), 404);
}

#[tokio::test]
async fn streaming_admission_attachments_and_usage_reach_t3() {
    let (release, gate) = oneshot::channel();
    let mut script = text_script("Hello ", "world");
    script.gate = Some(gate);
    let mut provider = Provider::start(vec![script]).await;
    let server = TestServer::start(&provider, json!({})).await;
    let id = server.session(allow()).await;
    let attachment = server.directory.join("notes.txt");
    std::fs::write(&attachment, "ATTACHMENT_SENTINEL").unwrap();
    let mut request = prompt("msg_t3_admission_1", "Say hello");
    request["system"] = json!("T3_SYSTEM_SENTINEL");
    request["variant"] = json!("high");
    request["parts"].as_array_mut().unwrap().push(json!({"type": "file", "mime": "text/plain", "filename": "notes.txt", "url": format!("file://{}", encode_directory(&attachment).replace("%2F", "/"))}));
    let mut events = server.events().await;
    assert_eq!(server.post(&format!("/session/{id}/prompt_async"), &request).send().await.unwrap().status(), 204);
    let model_request = provider.request().await;
    assert!(model_request.to_string().contains("T3_SYSTEM_SENTINEL"));
    assert!(model_request.to_string().contains("ATTACHMENT_SENTINEL"));
    let first = events.through("message.part.delta").await;
    let delta = first.last().unwrap();
    let part_id = &delta["properties"]["partID"];
    assert!(first.iter().any(|event| event["type"] == "message.part.updated" && event["properties"]["part"]["id"] == *part_id && event["properties"]["part"]["text"] == ""));
    assert!(first.iter().any(|event| event["type"] == "message.updated" && event["properties"]["info"]["id"] == "msg_t3_admission_1"));
    let status: Value = server.get("/session/status").send().await.unwrap().json().await.unwrap();
    assert_eq!(status[&id]["type"], "busy");
    assert_eq!(server.post(&format!("/session/{id}/prompt_async"), &request).send().await.unwrap().status(), 204);
    assert!(provider.requests.try_recv().is_err(), "retry started a second provider request");
    release.send(()).unwrap();
    let final_events = events.through("session.idle").await;
    assert!(final_events.iter().any(|event| event["type"] == "message.part.updated" && event["properties"]["part"]["type"] == "step-finish" && event["properties"]["part"]["tokens"]["output"].as_i64().unwrap_or(0) > 0));
    let messages = server.messages(&id).await;
    assert_eq!(messages[0]["info"]["id"], "msg_t3_admission_1");
    let assistant = messages.iter().find(|message| message["info"]["role"] == "assistant").unwrap();
    assert_eq!(assistant["info"]["parentID"], "msg_t3_admission_1");
    assert!(assistant["info"]["time"]["completed"].is_number());
    assert!(assistant["parts"].as_array().unwrap().iter().any(|part| part["type"] == "text" && part["text"] == "Hello world"));
    let status: Value = server.get("/session/status").send().await.unwrap().json().await.unwrap();
    assert!(status.get(&id).is_none());
    let replay = server.post(&format!("/session/{id}/message"), &request).send().await.unwrap();
    assert_eq!(replay.status(), 200);
    assert_eq!(replay.json::<Value>().await.unwrap()["info"]["id"], assistant["info"]["id"]);
    assert_eq!(server.messages(&id).await.len(), messages.len());
    assert!(provider.requests.try_recv().is_err(), "completed retry generated again");
}

#[tokio::test]
async fn steering_is_admitted_while_busy_and_processed_before_idle() {
    let (release, gate) = oneshot::channel();
    let mut first = text_script("First ", "response");
    first.gate = Some(gate);
    let mut provider = Provider::start(vec![first, text_script("Steered ", "response")]).await;
    let server = TestServer::start(&provider, json!({})).await;
    let id = server.session(allow()).await;
    let mut events = server.events().await;
    assert_eq!(server.post(&format!("/session/{id}/prompt_async"), &prompt("msg_first", "first")).send().await.unwrap().status(), 204);
    provider.request().await;
    events.through("message.part.delta").await;
    assert_eq!(server.post(&format!("/session/{id}/prompt_async"), &prompt("msg_steer", "STEERING_SENTINEL")).send().await.unwrap().status(), 204);
    assert!(server.messages(&id).await.iter().any(|message| message["info"]["id"] == "msg_steer"));
    release.send(()).unwrap();
    let next = provider.request().await;
    assert!(next.to_string().contains("STEERING_SENTINEL"));
    let events = events.through("session.idle").await;
    assert!(events.iter().any(|event| event["type"] == "message.updated" && event["properties"]["info"]["parentID"] == "msg_steer" && event["properties"]["info"]["time"]["completed"].is_number()));
}

#[tokio::test]
async fn permissions_and_questions_resume_the_actual_tool_loop() {
    let provider = Provider::start(vec![
        tool_script("bash", json!({"command": "printf TOOL_SENTINEL", "description": "Print a test marker"}), "call_bash"),
        tool_script("question", json!({"questions": [
            {"question": "Choose a value", "header": "Choice", "options": [{"label": "A", "description": "First"}, {"label": "B", "description": "Second"}], "multiple": false},
            {"question": "Choose checks", "header": "Checks", "options": [{"label": "Unit", "description": "Unit tests"}, {"label": "Integration", "description": "Integration tests"}], "multiple": true}
        ]}), "call_question"),
        text_script("Finished", ""),
    ]).await;
    let server = TestServer::start(&provider, json!({})).await;
    let id = server.session(json!([{"permission": "*", "pattern": "*", "action": "allow"}, {"permission": "bash", "pattern": "*", "action": "ask"}])).await;
    let mut events = server.events().await;
    assert_eq!(server.post(&format!("/session/{id}/prompt_async"), &prompt("msg_tools", "Use tools")).send().await.unwrap().status(), 204);
    let asked = events.through("permission.asked").await.pop().unwrap();
    assert_eq!(asked["properties"]["tool"]["callID"], "call_bash");
    assert!(asked["properties"]["patterns"].is_array());
    let request_id = asked["properties"]["id"].as_str().unwrap();
    let response = server.post(&format!("/permission/{request_id}/reply"), &json!({"reply": "once"})).send().await.unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.json::<Value>().await.unwrap(), true);
    let asked = events.through("question.asked").await.pop().unwrap();
    assert!(asked["properties"]["questions"][0].is_object());
    assert_eq!(asked["properties"]["questions"][0]["header"], "Choice");
    assert_eq!(asked["properties"]["questions"].as_array().unwrap().len(), 2);
    let request_id = asked["properties"]["id"].as_str().unwrap();
    assert_eq!(server.post(&format!("/question/{request_id}/reply"), &json!({"answers": []})).send().await.unwrap().status(), 400);
    assert_eq!(server.post(&format!("/question/{request_id}/reply"), &json!({"answers": [["A", "B"], ["Unit"]]})).send().await.unwrap().status(), 400);
    assert_eq!(server.post(&format!("/question/{request_id}/reply"), &json!({"answers": [["A"], ["Unit", "Integration"]]})).send().await.unwrap().json::<Value>().await.unwrap(), true);
    events.through("session.idle").await;
    let messages = server.messages(&id).await;
    assert!(messages.iter().flat_map(|message| message["parts"].as_array().unwrap()).any(|part| part["type"] == "tool" && part["callID"] == "call_bash" && part["state"]["status"] == "completed" && part["state"]["output"].as_str().unwrap_or_default().contains("TOOL_SENTINEL")));
    assert_eq!(server.get("/permission").send().await.unwrap().json::<Value>().await.unwrap(), json!([]));
    assert_eq!(server.get("/question").send().await.unwrap().json::<Value>().await.unwrap(), json!([]));
}

#[tokio::test]
async fn task_children_execute_and_route_their_own_approval_requests() {
    let mut provider = Provider::start(vec![
        tool_script("task", json!({"description": "Check a child task", "prompt": "Run the child check", "subagent_type": "general"}), "call_parent_task"),
        tool_script("bash", json!({"command": "printf CHILD_TOOL_SENTINEL", "description": "Run a child check"}), "call_child_bash"),
        text_script("CHILD_RESULT_SENTINEL", ""),
        text_script("Parent completed", ""),
    ]).await;
    let server = TestServer::start(&provider, json!({})).await;
    let parent = server.session(json!([
        {"permission": "*", "pattern": "*", "action": "allow"},
        {"permission": "bash", "pattern": "*", "action": "ask"}
    ])).await;
    let mut events = server.events().await;
    assert_eq!(server.post(&format!("/session/{parent}/prompt_async"), &prompt("msg_parent_task", "Delegate the check")).send().await.unwrap().status(), 204);
    let prefix = events.through("permission.asked").await;
    let request = prefix.last().unwrap();
    let child = request["properties"]["sessionID"].as_str().unwrap().to_owned();
    assert_ne!(child, parent);
    assert_eq!(request["properties"]["tool"]["callID"], "call_child_bash");
    let created = prefix.iter().position(|event| event["type"] == "session.created" && event["properties"]["info"]["id"] == child).unwrap();
    assert_eq!(prefix[created]["properties"]["info"]["parentID"], parent);
    let first_child_message = prefix.iter().position(|event| event["type"] == "message.updated" && event["properties"]["sessionID"] == child).unwrap();
    assert!(created < first_child_message);
    assert!(prefix.iter().any(|event| event["type"] == "message.part.updated"
        && event["properties"]["part"]["callID"] == "call_parent_task"
        && event["properties"]["part"]["state"]["metadata"]["sessionId"] == child
        && event["properties"]["part"]["state"]["metadata"]["model"]["modelID"] == "test-model"));
    let request_id = request["properties"]["id"].as_str().unwrap();
    assert_eq!(server.post(&format!("/permission/{request_id}/reply"), &json!({"reply": "once"})).send().await.unwrap().status(), 200);
    loop {
        let event = events.next().await;
        if event["type"] == "session.idle" && event["properties"]["sessionID"] == parent { break; }
    }
    let children: Vec<Value> = server.get(&format!("/session/{parent}/children")).send().await.unwrap().json().await.unwrap();
    assert_eq!(children.len(), 1);
    assert_eq!(children[0]["id"], child);
    assert!(server.messages(&child).await.iter().any(|message| message.to_string().contains("CHILD_TOOL_SENTINEL")));
    let parent_messages = server.messages(&parent).await;
    let task = parent_messages.iter().flat_map(|message| message["parts"].as_array().unwrap()).find(|part| part["callID"] == "call_parent_task").unwrap();
    assert_eq!(task["state"]["status"], "completed");
    assert_eq!(task["state"]["metadata"]["sessionId"], child);
    assert!(task["state"]["output"].as_str().unwrap().contains("CHILD_RESULT_SENTINEL"));
    let mut requests = Vec::new();
    for _ in 0..4 { requests.push(provider.request().await); }
    assert!(requests.last().unwrap().to_string().contains("CHILD_RESULT_SENTINEL"));
}

#[tokio::test]
async fn abort_cancels_a_stalled_stream_without_waiting_for_another_token() {
    let (_release, gate) = oneshot::channel();
    let mut script = text_script("Started", "must not arrive");
    script.gate = Some(gate);
    let mut provider = Provider::start(vec![script]).await;
    let server = TestServer::start(&provider, json!({})).await;
    let id = server.session(allow()).await;
    let mut events = server.events().await;
    assert_eq!(server.post(&format!("/session/{id}/prompt_async"), &prompt("msg_abort", "hello")).send().await.unwrap().status(), 204);
    provider.request().await;
    events.through("message.part.delta").await;
    assert_eq!(server.post(&format!("/session/{id}/abort"), &json!({})).send().await.unwrap().json::<Value>().await.unwrap(), true);
    let terminal = tokio::time::timeout(Duration::from_secs(3), events.through("session.idle")).await.expect("abort waited for stalled provider");
    assert!(terminal.iter().any(|event| event["type"] == "session.error" && event["properties"]["error"]["name"] == "MessageAbortedError"));
    assert!(!server.messages(&id).await.to_vec().iter().any(|message| message.to_string().contains("must not arrive")));
}

#[tokio::test]
async fn fork_excludes_boundary_remaps_references_and_survives_restart() {
    let provider = Provider::start(vec![text_script("Answer one", ""), text_script("Answer two", "")]).await;
    let mut server = TestServer::start(&provider, json!({})).await;
    let id = server.session(allow()).await;
    let mut events = server.events().await;
    for (mid, text) in [("msg_turn_one", "one"), ("msg_turn_two", "two")] {
        assert_eq!(server.post(&format!("/session/{id}/message"), &prompt(mid, text)).send().await.unwrap().status(), 200);
        events.through("session.idle").await;
    }
    let original = server.messages(&id).await;
    let boundary_index = original.iter().position(|message| message["info"]["id"] == "msg_turn_two").unwrap();
    let fork: Value = server.post(&format!("/session/{id}/fork"), &json!({"messageID": "msg_turn_two"})).send().await.unwrap().error_for_status().unwrap().json().await.unwrap();
    let fork_id = fork["id"].as_str().unwrap().to_owned();
    assert!(fork.get("parentID").is_none());
    let messages = server.messages(&fork_id).await;
    assert_eq!(messages.len(), boundary_index);
    assert_eq!(messages[1]["info"]["parentID"], messages[0]["info"]["id"]);
    for (message, source) in messages.iter().zip(&original) {
        assert_eq!(message["info"]["sessionID"], fork_id);
        assert_ne!(message["info"]["id"], source["info"]["id"]);
        for part in message["parts"].as_array().unwrap() {
            assert_eq!(part["sessionID"], fork_id);
            assert_eq!(part["messageID"], message["info"]["id"]);
        }
    }
    assert_eq!(server.messages(&id).await, original);
    server.restart().await;
    assert_eq!(server.messages(&fork_id).await, messages);
    assert_eq!(server.messages(&id).await, original);
    let deleted = server.client.delete(format!("{}/session/{fork_id}", server.url)).send().await.unwrap();
    assert_eq!(deleted.json::<Value>().await.unwrap(), true);
    assert_eq!(server.get(&format!("/session/{fork_id}/message")).send().await.unwrap().status(), 404);
}

#[tokio::test]
async fn configured_commands_expand_arguments_and_preserve_admission_id() {
    let mut provider = Provider::start(vec![text_script("Review complete", "")]).await;
    let server = TestServer::start(&provider, json!({})).await;
    let id = server.session(allow()).await;
    let response = server.post(&format!("/session/{id}/command"), &json!({"command": "review", "arguments": "\"alpha beta\" gamma delta", "model": "mock/test-model", "messageID": "msg_command", "agent": "reviewer", "variant": "high", "parts": []})).send().await.unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.json::<Value>().await.unwrap()["info"]["role"], "assistant");
    let request = provider.request().await;
    assert!(request.to_string().contains("First alpha beta. Rest gamma delta."));
    assert!(request.to_string().contains("REVIEWER_SYSTEM"));
    assert!(server.messages(&id).await.iter().any(|message| message["info"]["id"] == "msg_command"));
    let commands: Value = server.get("/command").send().await.unwrap().json().await.unwrap();
    assert_eq!(commands[0]["name"], "review");
    assert_eq!(commands[0]["hints"], json!(["$1", "$2"]));
}

#[tokio::test]
async fn omitted_selections_use_the_configured_default_agent_and_its_model() {
    let mut provider = Provider::start(vec![text_script("Default agent answered", "")]).await;
    let server = TestServer::start(&provider, json!({
        "default_agent": "reviewer",
        "provider": {"mock": {"npm": "@ai-sdk/openai-compatible", "options": {"baseURL": provider.url},
            "models": {"test-model": {"name": "Global model"}, "agent-model": {"name": "Agent model"}}
        }},
        "agent": {"reviewer": {"mode": "primary", "model": "mock/agent-model", "prompt": "DEFAULT_AGENT_SENTINEL"}}
    })).await;
    let id = server.session(allow()).await;
    let response = server.post(&format!("/session/{id}/message"), &json!({
        "messageID": "msg_default_agent", "parts": [{"type": "text", "text": "Use my defaults"}]
    })).send().await.unwrap();
    assert_eq!(response.status(), 200);
    let request = provider.request().await;
    assert_eq!(request["model"], "agent-model");
    assert!(request.to_string().contains("DEFAULT_AGENT_SENTINEL"));
    let messages = server.messages(&id).await;
    assert_eq!(messages[0]["info"]["agent"], "reviewer");
    assert_eq!(messages[0]["info"]["model"]["modelID"], "agent-model");
}

#[tokio::test]
async fn compaction_calls_the_model_and_produces_a_real_summary() {
    let mut provider = Provider::start(vec![text_script("Prior answer", ""), text_script("SUMMARY_SENTINEL: user requested tests and the previous answer was recorded.", ""), text_script("Continued from summary", "")]).await;
    let server = TestServer::start(&provider, json!({})).await;
    let id = server.session(allow()).await;
    assert_eq!(server.post(&format!("/session/{id}/message"), &prompt("msg_before_compact", "Please inspect tests")).send().await.unwrap().status(), 200);
    provider.request().await;
    let mut events = server.events().await;
    let response = server.post(&format!("/session/{id}/summarize"), &json!({"providerID": "mock", "modelID": "test-model", "auto": false})).send().await.unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.json::<Value>().await.unwrap(), true);
    let request = provider.request().await;
    assert!(request.to_string().contains("Prior answer"));
    events.through("session.compacted").await;
    let messages = server.messages(&id).await;
    assert!(messages.iter().any(|message| message.to_string().contains("SUMMARY_SENTINEL")));
    assert!(messages.iter().flat_map(|message| message["parts"].as_array().unwrap()).any(|part| part["type"] == "compaction"));
    assert_eq!(server.post(&format!("/session/{id}/message"), &prompt("msg_after_compact", "Continue")).send().await.unwrap().status(), 200);
    let next = provider.request().await;
    assert!(next.to_string().contains("SUMMARY_SENTINEL"));
    assert!(!next.to_string().contains("Prior answer"), "compaction replayed the old conversation unchanged");
    assert!(!next.to_string().contains("Please inspect tests"), "compaction did not replace the old user context");
}

#[tokio::test]
async fn unreachable_mcp_is_never_reported_as_connected() {
    let provider = Provider::start(vec![]).await;
    let server = TestServer::start(&provider, json!({})).await;
    let response = server.post("/mcp", &json!({"name": "unreachable", "config": {"type": "remote", "url": "http://127.0.0.1:1/mcp", "oauth": false}})).send().await.unwrap();
    assert_eq!(response.status(), 200);
    let status = response.json::<Value>().await.unwrap();
    assert_ne!(status["unreachable"]["status"], "connected");
    assert!(status["unreachable"]["status"].is_string());
    assert!(!server.post("/mcp/unreachable/connect", &json!({})).send().await.unwrap().status().is_success());
    let other = server.root.join("other-mcp-project");
    std::fs::create_dir_all(&other).unwrap();
    let status: Value = server.get("/mcp").header("x-opencode-directory", encode_directory(&other)).send().await.unwrap().json().await.unwrap();
    assert!(status.get("unreachable").is_none());
}

#[tokio::test]
async fn directory_event_streams_exclude_other_projects() {
    let provider = Provider::start(vec![]).await;
    let server = TestServer::start(&provider, json!({})).await;
    let mut events = server.events().await;
    let first = server.session(allow()).await;
    let other = server.root.join("separate-project");
    std::fs::create_dir_all(&other).unwrap();
    let response = server.post("/session", &json!({"directory": other, "permission": allow()})).send().await.unwrap();
    assert_eq!(response.status(), 200);
    let last = server.session(allow()).await;
    let first_event = events.through("session.created").await.pop().unwrap();
    let last_event = events.through("session.created").await.pop().unwrap();
    assert_eq!(first_event["properties"]["info"]["id"], first);
    assert_eq!(last_event["properties"]["info"]["id"], last);
}

#[tokio::test]
async fn no_reply_and_duplicate_admission_persist_without_a_model_call() {
    let mut provider = Provider::start(vec![]).await;
    let mut server = TestServer::start(&provider, json!({})).await;
    let id = server.session(allow()).await;
    let mut request = prompt("msg_seed", "Retain this context");
    request["noReply"] = json!(true);
    for _ in 0..2 {
        let response = server.post(&format!("/session/{id}/message"), &request).send().await.unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(response.json::<Value>().await.unwrap()["info"]["role"], "user");
    }
    assert_eq!(server.messages(&id).await.len(), 1);
    server.restart().await;
    assert_eq!(server.messages(&id).await.len(), 1);
    assert!(provider.requests.try_recv().is_err());
}

#[tokio::test]
async fn truncated_provider_streams_fail_instead_of_reporting_success() {
    let script = Script::truncated();
    let provider = Provider::start(vec![script]).await;
    let server = TestServer::start(&provider, json!({})).await;
    let id = server.session(allow()).await;
    let mut events = server.events().await;
    let response = server.post(&format!("/session/{id}/message"), &prompt("msg_truncated", "hello")).send().await.unwrap();
    assert!(!response.status().is_success());
    let terminal = events.through("session.idle").await;
    assert!(terminal.iter().any(|event| event["type"] == "session.error"));
    assert!(server.messages(&id).await.iter().any(|message| message["info"]["role"] == "assistant" && message["info"]["error"].is_object()));
}

#[tokio::test]
async fn transient_provider_failures_are_retried_and_the_turn_completes() {
    // Open-phase 500 with Retry-After, mid-stream transport death and a
    // truncated stream must all be retried; the turn must still succeed.
    let provider = Provider::start(vec![
        Script::http_status(500, Some("0")),
        Script::mid_stream_error("provider stream failed: error decoding response body"),
        Script::truncated(),
        text_script("Recovery ", "works"),
    ]).await;
    let server = TestServer::start(&provider, json!({})).await;
    let id = server.session(allow()).await;
    let mut events = server.events().await;
    let response = server.post(&format!("/session/{id}/message"), &prompt("msg_retry_flaky", "hello")).send().await.unwrap();
    assert!(response.status().is_success(), "flaky provider turn must complete: {:?}", response.text().await.unwrap());
    let terminal = events.through("session.idle").await;
    let retries = terminal.iter().filter(|event| event["type"] == "session.status" && event["properties"]["status"]["type"] == "retry").count();
    assert!(retries >= 3, "retry status events must be visible, saw {retries}");
    let messages = server.messages(&id).await;
    let assistant = messages.iter().rev().find(|m| m["info"]["role"] == "assistant").unwrap();
    let text: String = assistant["parts"].as_array().unwrap().iter().filter(|p| p["type"] == "text").filter_map(|p| p["text"].as_str()).collect();
    assert_eq!(text, "Recovery works");
}

#[tokio::test]
async fn retry_exhaustion_reports_a_visible_failure() {
    let provider = Provider::start(vec![
        Script::http_status(500, Some("0")),
        Script::http_status(503, Some("0")),
        Script::http_status(429, Some("0")),
        Script::http_status(502, Some("0")),
    ]).await;
    let server = TestServer::start(&provider, json!({})).await;
    let id = server.session(allow()).await;
    let mut events = server.events().await;
    let response = server.post(&format!("/session/{id}/message"), &prompt("msg_retry_dead", "hello")).send().await.unwrap();
    assert!(!response.status().is_success());
    let terminal = events.through("session.idle").await;
    assert!(terminal.iter().any(|event| event["type"] == "session.error"));
    let retries = terminal.iter().filter(|event| event["type"] == "session.status" && event["properties"]["status"]["type"] == "retry").count();
    assert_eq!(retries, 3, "exactly three bounded retries per step");
}
