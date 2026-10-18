use super::*;

#[cfg(unix)]
fn local_mcp() -> Value {
    json!({"mcp": {"probe": {"type": "local", "timeout": 2000,
        "environment": {"MCP_TEST_VALUE": "global"},
        "command": ["/bin/sh", "-c", r#"
printf '%s\t%s\t%s\n' "$$" "$PWD" "$MCP_TEST_VALUE" >> "$HOME/mcp-spawns"
while IFS= read -r request; do
  id=$(printf '%s\n' "$request" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  case "$request" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"2025-03-26","capabilities":{"tools":{}},"serverInfo":{"name":"probe","version":"1"}}}\n' "$id" ;;
    *'"method":"tools/list"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"echo_value","description":"Echo fixture","inputSchema":{"type":"object","properties":{}}}]}}\n' "$id" ;;
    *'"method":"tools/call"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"%s"}]}}\n' "$id" "$MCP_TEST_VALUE" ;;
  esac
done
"#]}}})
}

#[cfg(unix)]
fn mcp_spawns(server: &TestServer) -> Vec<Vec<String>> {
    std::fs::read_to_string(server.root.path().join("mcp-spawns"))
        .unwrap_or_default()
        .lines()
        .map(|line| line.split('\t').map(str::to_owned).collect())
        .collect()
}

#[cfg(unix)]
fn other_project(server: &TestServer) -> PathBuf {
    let directory = server.root.path().join("other project");
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(
        directory.join("opencode.json"),
        json!({"mcp": {"probe": {"environment": {"MCP_TEST_VALUE": "project-specific"}}}})
            .to_string(),
    )
    .unwrap();
    directory
}

#[tokio::test]
#[cfg(unix)]
async fn mcp_starts_on_use_once_per_directory_and_preserves_isolation() {
    let provider = Provider::start(vec![]).await;
    let server = TestServer::start(&provider, local_mcp()).await;
    let other = other_project(&server);
    for path in ["/global/health", "/provider", "/session"] {
        server
            .get(path)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
    }
    let id = server.session(allow()).await;
    let mut request = prompt("msg_seed", "Save context without invoking MCP");
    request["noReply"] = json!(true);
    server
        .post(&format!("/session/{id}/message"), &request)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    // Give an erroneous eager background startup time to reach the fixture.
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        mcp_spawns(&server).is_empty(),
        "metadata and noReply must not start MCP"
    );

    let responses = futures::future::join_all((0..12).map(|_| {
        server
            .get("/mcp")
            .header("x-opencode-directory", encode_directory(&other))
            .send()
    }))
    .await;
    for response in responses {
        let status: Value = response
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(status["probe"]["status"], "connected");
    }
    let spawns = mcp_spawns(&server);
    assert_eq!(spawns.len(), 1, "{spawns:?}");
    assert_eq!(spawns[0][1], other.to_string_lossy());
    assert_eq!(spawns[0][2], "project-specific");

    let status: Value = server
        .get("/mcp")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(status["probe"]["status"], "connected");
    let spawns = mcp_spawns(&server);
    assert_eq!(
        spawns.len(),
        2,
        "the launch directory must still work on demand"
    );
    assert_eq!(spawns[1][1], server.directory.to_string_lossy());
    assert_eq!(spawns[1][2], "global");
    server
        .post("/mcp/probe/disconnect", &json!({}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let status: Value = server
        .get("/mcp")
        .header(
            "x-opencode-directory",
            encode_directory(&other.join("../other project")),
        )
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        status["probe"]["status"], "connected",
        "disconnect must stay scoped"
    );
    assert_eq!(
        mcp_spawns(&server).len(),
        2,
        "aliases and repeated requests reuse the scope"
    );
}

#[tokio::test]
#[cfg(unix)]
async fn first_turn_initializes_mcp_without_a_prior_status_request() {
    let mut provider = Provider::start(vec![
        tool_script("probe_echo_value", json!({}), "mcp-call"),
        text_script("Used MCP", ""),
        text_script("Reused MCP", ""),
    ])
    .await;
    let server = TestServer::start(&provider, local_mcp()).await;
    let other = other_project(&server);
    let response: Value = server
        .post("/session", &json!({"permission": allow()}))
        .header("x-opencode-directory", encode_directory(&other))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = response["id"].as_str().unwrap();
    for message_id in ["msg_first", "msg_second"] {
        let reply: Value = server
            .post(
                &format!("/session/{id}/message"),
                &prompt(message_id, "Use the probe"),
            )
            .header("x-opencode-directory", encode_directory(&other))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(reply["info"].get("error").is_none(), "{reply}");
    }
    let request = provider.request().await;
    assert!(request["tools"]
        .as_array()
        .unwrap()
        .iter()
        .any(|tool| tool["function"]["name"] == "probe_echo_value"));
    let request = provider.request().await;
    assert!(request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .any(|message| message["role"] == "tool" && message["content"] == "project-specific"));
    let spawns = mcp_spawns(&server);
    assert_eq!(spawns.len(), 1, "{spawns:?}");
    assert_eq!(spawns[0][1], other.to_string_lossy());
    assert_eq!(spawns[0][2], "project-specific");
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
    assert!(!server
        .post("/mcp/unreachable/connect", &json!({}))
        .send()
        .await
        .unwrap()
        .status()
        .is_success());
    let other = server.root.path().join("other-mcp-project");
    std::fs::create_dir_all(&other).unwrap();
    let status: Value = server
        .get("/mcp")
        .header("x-opencode-directory", encode_directory(&other))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(status.get("unreachable").is_none());
}
