use super::*;

#[cfg(unix)]
fn stdio_config() -> Value {
    json!({"type":"local","command":["/bin/sh","-c",r#"
while IFS= read -r request; do
  id=$(printf '%s\n' "$request" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  case "$request" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"2025-03-26","capabilities":{"tools":{}},"serverInfo":{"name":"fixture","version":"1"}}}\n' "$id" ;;
    *'"method":"tools/list"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"echo_value","description":"Echo fixture","inputSchema":{"type":"object","properties":{}}}]}}\n' "$id" ;;
    *'"method":"tools/call"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"%s"}]}}\n' "$id" "$MCP_TEST_VALUE" ;;
  esac
done
"#],"environment":{"MCP_TEST_VALUE":"environment-is-forwarded"},"timeout":2000})
}

#[tokio::test]
#[cfg(unix)]
async fn stdio_connects_discovers_routes_and_calls_with_environment() {
    let hub = McpHub::new();
    let mut config = stdio_config();
    config["timeout"] = json!(3_600_000);
    let status = hub
        .add_async("server_with_underscores", config)
        .await
        .unwrap();
    assert_eq!(status["status"], "connected", "{status}");
    assert_eq!(
        hub.servers.lock().unwrap()["server_with_underscores"].timeout(),
        Duration::from_secs(3600)
    );
    let definitions = hub.tool_defs();
    let name = definitions[0]["function"]["name"].as_str().unwrap();
    assert_eq!(name, "server_with_underscores_echo_value");
    assert_eq!(
        hub.route(name),
        Some(("server_with_underscores".into(), "echo_value".into()))
    );
    let output = hub
        .call_result_async(
            "server_with_underscores",
            "echo_value",
            json!({}),
            Arc::new(AtomicBool::new(false)),
        )
        .await
        .unwrap();
    assert_eq!(render_result(&output), "environment-is-forwarded");
    hub.disconnect("server_with_underscores").unwrap();
    assert_eq!(
        hub.status()["server_with_underscores"]["status"],
        "disabled"
    );
    assert!(hub.tool_defs().is_empty());
    let status = hub.connect_async("server_with_underscores").await.unwrap();
    assert_eq!(status["status"], "connected");
    assert_eq!(hub.tool_defs().len(), 1);
}

#[tokio::test]
async fn disabled_servers_are_registered_without_launching() {
    let hub = McpHub::new();
    let status = hub
        .add_async(
            "disabled",
            json!({"type":"local","command":["does-not-exist"],"enabled":false}),
        )
        .await
        .unwrap();
    assert_eq!(status["status"], "disabled");
    assert_eq!(hub.status()["disabled"]["status"], "disabled");
    assert!(hub.tool_defs().is_empty());
}

#[tokio::test]
#[cfg(unix)]
async fn handshake_timeout_kills_process_and_preserves_failure() {
    let hub = McpHub::new();
    let pidfile = std::env::temp_dir().join(crate::types::hex_id("ocmini-mcp-pid-"));
    let script = format!("echo $$ > '{}'; sleep 60", pidfile.display());
    let status = hub
        .add_async(
            "slow",
            json!({"type":"local","command":["/bin/sh","-c",script],"timeout":100}),
        )
        .await
        .unwrap();
    assert_eq!(status["status"], "failed");
    assert!(status["error"].as_str().unwrap().contains("timed out"));
    let pid: i32 = std::fs::read_to_string(&pidfile)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let _ = std::fs::remove_file(pidfile);
    assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
    assert!(hub.tool_defs().is_empty());
}

#[tokio::test]
async fn remote_streamable_http_negotiates_session_and_reads_sse() {
    use axum::{
        extract::State,
        http::{HeaderMap, StatusCode},
        response::IntoResponse,
        routing::post,
        Json, Router,
    };
    type Requests = Arc<Mutex<Vec<Value>>>;
    async fn endpoint(
        State(requests): State<Requests>,
        headers: HeaderMap,
        Json(request): Json<Value>,
    ) -> axum::response::Response {
        assert_eq!(
            headers.get("authorization").unwrap(),
            "Bearer fixture-token"
        );
        assert_eq!(headers.get("mcp-protocol-version").unwrap(), PROTOCOL);
        if request["id"] == "server-ping" {
            assert_eq!(request["result"], json!({}));
            requests.lock().unwrap().push(request);
            return StatusCode::ACCEPTED.into_response();
        }
        let method = request["method"].as_str().unwrap();
        if method != "initialize" {
            assert_eq!(headers.get("mcp-session-id").unwrap(), "fixture-session");
        }
        requests.lock().unwrap().push(request.clone());
        let result = match method {
            "initialize" => {
                json!({"protocolVersion":PROTOCOL,"capabilities":{"tools":{}},"serverInfo":{"name":"http-fixture","version":"1"}})
            }
            "notifications/initialized" | "notifications/cancelled" => {
                return StatusCode::ACCEPTED.into_response()
            }
            "tools/list" if request["params"]["cursor"] == "next" => {
                json!({"tools":[{"name":"failure","inputSchema":{"type":"object"}}]})
            }
            "tools/list" => {
                json!({"tools":[{"name":"echo","description":"Echo","inputSchema":{"type":"object","properties":{}}}],"nextCursor":"next"})
            }
            "tools/call" if request["params"]["name"] == "failure" => {
                json!({"isError":true,"content":[{"type":"text","text":"fixture tool failed"}]})
            }
            "tools/call" => {
                json!({"content":[{"type":"text","text":"hello"},{"type":"resource_link","uri":"file:///fixture","name":"resource"}],"structuredContent":{"answer":42}})
            }
            _ => return StatusCode::BAD_REQUEST.into_response(),
        };
        let response = json!({"jsonrpc":"2.0","id":request["id"],"result":result});
        if method == "tools/call" {
            let ping = json!({"jsonrpc":"2.0","id":"server-ping","method":"ping"});
            let changed = json!({"jsonrpc":"2.0","method":"notifications/tools/list_changed"});
            return ([("content-type","text/event-stream")], format!(": keepalive\r\nevent: message\r\ndata: {ping}\r\n\r\ndata: {changed}\r\n\r\ndata: {response}\r\n\r\n")).into_response();
        }
        let mut response = Json(response).into_response();
        if method == "initialize" {
            response
                .headers_mut()
                .insert("mcp-session-id", "fixture-session".parse().unwrap());
        }
        response
    }
    let requests: Requests = Arc::new(Mutex::new(Vec::new()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let app = Router::new()
        .route("/mcp", post(endpoint))
        .with_state(requests.clone());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let hub = McpHub::new();
    let status = hub.add_async("remote", json!({"type":"remote","url":format!("http://{address}/mcp"),"headers":{"Authorization":"Bearer fixture-token"},"oauth":false,"timeout":2000})).await.unwrap();
    assert_eq!(status["status"], "connected", "{status}");
    assert_eq!(hub.tool_defs().len(), 2);
    let result = hub
        .call_result_async(
            "remote",
            "echo",
            json!({}),
            Arc::new(AtomicBool::new(false)),
        )
        .await
        .unwrap();
    let result = render_result(&result);
    assert!(result.contains("hello"));
    assert!(result.contains("resource_link"));
    assert!(result.contains("42"));
    let error = hub
        .call_result_async(
            "remote",
            "failure",
            json!({}),
            Arc::new(AtomicBool::new(false)),
        )
        .await
        .unwrap_err();
    assert_eq!(error, "fixture tool failed");
    assert_eq!(
        requests.lock().unwrap()[1]["method"],
        "notifications/initialized"
    );
    server.abort();
}

#[tokio::test]
async fn remote_auth_failure_is_never_reported_as_connected() {
    use axum::{http::StatusCode, routing::post, Router};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let app = Router::new().route("/", post(|| async { StatusCode::UNAUTHORIZED }));
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let hub = McpHub::new();
    let status = hub
        .add_async(
            "auth",
            json!({"type":"remote","url":format!("http://{address}/"),"timeout":1000}),
        )
        .await
        .unwrap();
    assert_eq!(status["status"], "needs_auth");
    assert!(hub.tool_defs().is_empty());
    server.abort();
}

#[tokio::test]
async fn disconnect_during_remote_refresh_keeps_disabled_status_and_empty_catalog() {
    use axum::{
        extract::State, http::StatusCode, response::IntoResponse, routing::post, Json, Router,
    };
    use std::sync::atomic::AtomicUsize;
    struct Fixture {
        lists: AtomicUsize,
        entered: tokio::sync::Notify,
        release: tokio::sync::Notify,
    }
    async fn endpoint(
        State(fixture): State<Arc<Fixture>>,
        Json(request): Json<Value>,
    ) -> axum::response::Response {
        let result = match request["method"].as_str().unwrap_or_default() {
            "initialize" => {
                json!({"protocolVersion":PROTOCOL,"capabilities":{"tools":{}},"serverInfo":{"name":"refresh-fixture","version":"1"}})
            }
            "notifications/initialized" | "notifications/cancelled" => {
                return StatusCode::ACCEPTED.into_response()
            }
            "tools/list" => {
                if fixture.lists.fetch_add(1, Ordering::AcqRel) > 0 {
                    fixture.entered.notify_one();
                    fixture.release.notified().await;
                }
                json!({"tools":[{"name":"fixture","inputSchema":{"type":"object"}}]})
            }
            _ => return StatusCode::BAD_REQUEST.into_response(),
        };
        Json(json!({"jsonrpc":"2.0","id":request["id"],"result":result})).into_response()
    }
    let fixture = Arc::new(Fixture {
        lists: AtomicUsize::new(0),
        entered: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let app = Router::new()
        .route("/", post(endpoint))
        .with_state(fixture.clone());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let hub = McpHub::new();
    assert_eq!(
        hub.add_async(
            "refresh",
            json!({"url":format!("http://{address}/"),"timeout":2000})
        )
        .await
        .unwrap()["status"],
        "connected"
    );
    let refreshing = hub.clone();
    let refresh = tokio::spawn(async move { refreshing.tool_defs_async().await });
    tokio::time::timeout(Duration::from_secs(2), fixture.entered.notified())
        .await
        .unwrap();
    hub.disconnect("refresh").unwrap();
    fixture.release.notify_one();
    assert!(tokio::time::timeout(Duration::from_secs(2), refresh)
        .await
        .unwrap()
        .unwrap()
        .is_empty());
    assert_eq!(hub.status()["refresh"]["status"], "disabled");
    assert!(hub.tool_defs().is_empty());
    server.abort();
}

#[tokio::test]
async fn remote_server_without_tool_capability_stays_connected() {
    use axum::{http::StatusCode, response::IntoResponse, routing::post, Json, Router};
    async fn endpoint(Json(request): Json<Value>) -> axum::response::Response {
        match request["method"].as_str().unwrap_or_default() {
            "initialize" => Json(json!({"jsonrpc":"2.0","id":request["id"],"result":{"protocolVersion":PROTOCOL,"capabilities":{},"serverInfo":{"name":"no-tools","version":"1"}}})).into_response(),
            "notifications/initialized" => StatusCode::ACCEPTED.into_response(),
            _ => StatusCode::BAD_REQUEST.into_response(),
        }
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let app = Router::new().route("/", post(endpoint));
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let hub = McpHub::new();
    assert_eq!(
        hub.add_async(
            "no-tools",
            json!({"url":format!("http://{address}/"),"timeout":1000})
        )
        .await
        .unwrap()["status"],
        "connected"
    );
    assert!(hub.tool_defs_async().await.is_empty());
    assert_eq!(hub.status()["no-tools"]["status"], "connected");
    server.abort();
}

#[test]
fn nontext_results_and_tool_failures_are_not_silently_discarded() {
    let result = render_result(
        &json!({"content":[{"type":"resource","resource":{"uri":"file:///x","text":"resource text"}},{"type":"image","data":"abc","mimeType":"image/png"}]}),
    );
    assert!(result.contains("resource text"));
    assert!(result.contains("image/png"));
    assert!(!result.contains("abc"));
}

#[test]
fn bounded_reader_rejects_unterminated_oversized_messages() {
    let bytes = vec![b'a'; 65];
    assert!(bounded_line(&mut BufReader::new(bytes.as_slice()), 64).is_err());
}
