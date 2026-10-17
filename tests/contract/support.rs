use super::*;

pub(super) const PASSWORD: &str = "t3-test-pässword";

pub(super) struct Script {
    pub(super) chunks: Vec<Value>,
    pub(super) gate: Option<oneshot::Receiver<()>>,
    pub(super) status: Option<u16>,
    pub(super) retry_after: Option<String>,
    pub(super) terminate: bool,
    pub(super) body: Option<String>,
}

impl Script {
    pub(super) fn ok(chunks: Vec<Value>) -> Self {
        Self {
            chunks,
            gate: None,
            status: None,
            retry_after: None,
            terminate: true,
            body: None,
        }
    }
    pub(super) fn http_status(status: u16, retry_after: Option<&str>) -> Self {
        Self {
            chunks: vec![],
            gate: None,
            status: Some(status),
            retry_after: retry_after.map(str::to_owned),
            terminate: true,
            body: None,
        }
    }
    pub(super) fn http_status_body(status: u16, message: &str) -> Self {
        Self {
            chunks: vec![],
            gate: None,
            status: Some(status),
            retry_after: None,
            terminate: true,
            body: Some(message.to_owned()),
        }
    }
    pub(super) fn mid_stream_error(message: &str) -> Self {
        Self::ok(vec![
            json!({"choices": [{"index": 0, "delta": {"content": "partial"}, "finish_reason": null}]}),
            json!({"error": {"message": message}}),
        ])
    }
    pub(super) fn truncated() -> Self {
        Self {
            chunks: vec![
                json!({"choices": [{"index": 0, "delta": {"content": "Incomplete response"}, "finish_reason": null}]}),
            ],
            gate: None,
            status: None,
            retry_after: None,
            terminate: false,
            body: None,
        }
    }
}

pub(super) fn usage() -> Value {
    json!({"prompt_tokens": 17, "completion_tokens": 9,
        "prompt_tokens_details": {"cached_tokens": 3},
        "completion_tokens_details": {"reasoning_tokens": 2}})
}

pub(super) fn text_script(first: &str, second: &str) -> Script {
    Script::ok(vec![
        json!({"id": "completion-test", "choices": [{"index": 0, "delta": {"role": "assistant", "content": first}, "finish_reason": null}]}),
        json!({"id": "completion-test", "choices": [{"index": 0, "delta": {"content": second}, "finish_reason": null}]}),
        json!({"id": "completion-test", "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}], "usage": usage()}),
    ])
}

pub(super) fn tool_script(name: &str, arguments: Value, id: &str) -> Script {
    Script::ok(vec![
        json!({"choices": [{"index": 0, "delta": {"tool_calls": [{"index": 0, "id": id, "type": "function", "function": {"name": name, "arguments": arguments.to_string()}}]}, "finish_reason": null}]}),
        json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}], "usage": usage()}),
    ])
}

#[derive(Clone)]
pub(super) struct ProviderState {
    pub(super) scripts: Arc<Mutex<VecDeque<Script>>>,
    pub(super) requests: mpsc::UnboundedSender<Value>,
}

pub(super) async fn provider_reply(
    State(state): State<ProviderState>,
    Json(request): Json<Value>,
) -> Response {
    state.requests.send(request).unwrap();
    let Some(script) = state.scripts.lock().await.pop_front() else {
        return (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": {"message": "Unexpected extra model request"}})),
        )
            .into_response();
    };
    if let Some(status) = script.status {
        let status = axum::http::StatusCode::from_u16(status).expect("test status");
        let message = script
            .body
            .unwrap_or_else(|| "provider is having a bad day".to_owned());
        let mut response = (status, Json(json!({"error": {"message": message}}))).into_response();
        if let Some(value) = script.retry_after {
            if let Ok(value) = axum::http::HeaderValue::from_str(&value) {
                response.headers_mut().insert("retry-after", value);
            }
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

pub(super) struct Provider {
    pub(super) url: String,
    pub(super) requests: mpsc::UnboundedReceiver<Value>,
    pub(super) task: tokio::task::JoinHandle<()>,
}

impl Provider {
    pub(super) async fn start(scripts: Vec<Script>) -> Self {
        let (requests, receiver) = mpsc::unbounded_channel();
        let state = ProviderState {
            scripts: Arc::new(Mutex::new(scripts.into())),
            requests,
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        let router = Router::new()
            .route("/v1/chat/completions", post(provider_reply))
            .with_state(state);
        let task = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        Self {
            url,
            requests: receiver,
            task,
        }
    }

    pub(super) async fn request(&mut self) -> Value {
        tokio::time::timeout(Duration::from_secs(10), self.requests.recv())
            .await
            .expect("model request timed out")
            .expect("model server closed")
    }
}

impl Drop for Provider {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub(super) fn encode_directory(path: &Path) -> String {
    path.to_string_lossy()
        .as_bytes()
        .iter()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || b"-_.~".contains(byte) {
                (*byte as char).to_string()
            } else {
                format!("%{byte:02X}")
            }
        })
        .collect()
}

pub(super) fn configured_command(root: &Path, directory: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_ocmini"));
    command
        .env_clear()
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

pub(super) async fn launch(root: &Path, directory: &Path) -> (Child, String) {
    let mut child = configured_command(root, directory)
        .args(["serve", "--hostname=127.0.0.1", "--port=0"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let mut lines = BufReader::new(stdout).lines();
    let url = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let line = lines
                .next_line()
                .await
                .unwrap()
                .expect("server exited before its banner");
            if let Some(url) = line.strip_prefix("opencode server listening on ") {
                break url.to_owned();
            }
        }
    })
    .await
    .expect("server startup timed out");
    (child, url)
}

pub(super) struct TestServer {
    pub(super) child: Child,
    pub(super) url: String,
    pub(super) root: tempfile::TempDir,
    pub(super) directory: PathBuf,
    pub(super) client: reqwest::Client,
}

impl TestServer {
    pub(super) async fn start(provider: &Provider, extra: Value) -> Self {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("project café %");
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::create_dir_all(root.path().join(".config/opencode")).unwrap();
        std::fs::create_dir_all(root.path().join(".local/share")).unwrap();
        let mut config = json!({
            "model": "mock/test-model",
            "provider": {"mock": {"npm": "@ai-sdk/openai-compatible", "name": "Local test model",
                "options": {"baseURL": provider.url},
                "models": {"test-model": {"name": "Test model", "limit": {"context": 32000, "output": 2048}, "variants": {"high": {"reasoningEffort": "high"}}}}
            }},
            "command": {"review": {"description": "Review arguments", "template": "First $1. Rest $2."}},
            "agent": {"reviewer": {"description": "Review code", "mode": "primary", "prompt": "REVIEWER_SYSTEM"}}
        });
        for (key, value) in extra.as_object().unwrap() {
            config[key] = value.clone();
        }
        std::fs::write(
            root.path().join(".config/opencode/opencode.json"),
            config.to_string(),
        )
        .unwrap();
        let (child, url) = launch(root.path(), &directory).await;
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::AUTHORIZATION,
            format!(
                "Basic {}",
                base64::engine::general_purpose::STANDARD.encode(format!("opencode:{PASSWORD}"))
            )
            .parse()
            .unwrap(),
        );
        headers.insert(
            "x-opencode-directory",
            encode_directory(&directory).parse().unwrap(),
        );
        let client = reqwest::Client::builder()
            .default_headers(headers)
            .timeout(Duration::from_secs(20))
            .build()
            .unwrap();
        Self {
            child,
            url,
            root,
            directory,
            client,
        }
    }

    pub(super) fn get(&self, path: &str) -> reqwest::RequestBuilder {
        self.client.get(format!("{}{path}", self.url))
    }
    pub(super) fn post(&self, path: &str, body: &Value) -> reqwest::RequestBuilder {
        self.client.post(format!("{}{path}", self.url)).json(body)
    }

    pub(super) async fn session(&self, permission: Value) -> String {
        let response = self
            .post("/session", &json!({"permission": permission}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        response.json::<Value>().await.unwrap()["id"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    pub(super) async fn messages(&self, id: &str) -> Vec<Value> {
        self.get(&format!("/session/{id}/message"))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap()
    }

    pub(super) async fn restart(&mut self) {
        self.child.kill().await.unwrap();
        self.child.wait().await.unwrap();
        let (child, url) = launch(self.root.path(), &self.directory).await;
        self.child = child;
        self.url = url;
    }

    pub(super) async fn events(&self) -> Events {
        let response = self
            .get("/event")
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
        assert!(response.headers()["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/event-stream"));
        let (send, receive) = mpsc::unbounded_channel();
        let task = tokio::spawn(async move {
            let mut stream = response.bytes_stream();
            let mut bytes = Vec::new();
            while let Some(chunk) = stream.next().await {
                let Ok(chunk) = chunk else {
                    break;
                };
                bytes.extend_from_slice(&chunk);
                while let Some(end) = bytes.windows(2).position(|pair| pair == b"\n\n") {
                    let frame: Vec<_> = bytes.drain(..end + 2).collect();
                    let frame = String::from_utf8(frame).unwrap();
                    let data = frame
                        .lines()
                        .filter_map(|line| line.strip_prefix("data:").map(str::trim_start))
                        .collect::<Vec<_>>()
                        .join("\n");
                    if !data.is_empty() && send.send(serde_json::from_str(&data).unwrap()).is_err()
                    {
                        return;
                    }
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
    }
}

pub(super) struct Events {
    pub(super) receive: mpsc::UnboundedReceiver<Value>,
    pub(super) task: tokio::task::JoinHandle<()>,
}

impl Events {
    pub(super) async fn next(&mut self) -> Value {
        tokio::time::timeout(Duration::from_secs(10), self.receive.recv())
            .await
            .expect("SSE event timed out")
            .expect("SSE stream ended unexpectedly")
    }

    pub(super) async fn through(&mut self, event_type: &str) -> Vec<Value> {
        let mut result = Vec::new();
        loop {
            let event = self.next().await;
            let matched = event["type"] == event_type;
            result.push(event);
            if matched {
                return result;
            }
            assert!(
                result.len() < 10000,
                "event stream never reached {event_type}"
            );
        }
    }
}

impl Drop for Events {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub(super) fn allow() -> Value {
    json!([{"permission": "*", "pattern": "*", "action": "allow"}])
}

pub(super) fn prompt(id: &str, text: &str) -> Value {
    json!({"messageID": id, "model": {"providerID": "mock", "modelID": "test-model"}, "parts": [{"type": "text", "text": text}]})
}
