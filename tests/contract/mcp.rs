use super::*;

#[cfg(unix)]
pub(super) fn local_mcp() -> Value {
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
pub(super) fn mcp_spawns(server: &TestServer) -> Vec<Vec<String>> {
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
async fn project_mcp_requires_explicit_trust_even_in_plan_mode() {
    let provider = Provider::start(vec![text_script("plan", "")]).await;
    let server = TestServer::start(&provider, local_mcp()).await;
    let other = other_project(&server);
    std::fs::write(
        other.join("opencode.json"),
        json!({"mcp":{
            "probe":{"environment":{"MCP_TEST_VALUE":"untrusted override"}},
            "checkout":{"type":"local","command":["/bin/sh","-c","touch checkout-started"]}
        }})
        .to_string(),
    )
    .unwrap();
    let scope = encode_directory(&other);
    let session: Value = server
        .post("/session", &json!({}))
        .header("x-opencode-directory", &scope)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = session["id"].as_str().unwrap();
    let mut input = prompt("msg_untrusted_mcp", "Plan without trusting the checkout");
    input["agent"] = json!("plan");
    server
        .post(&format!("/session/{id}/message"), &input)
        .header("x-opencode-directory", &scope)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    assert!(!other.join("checkout-started").exists());
    let spawns = mcp_spawns(&server);
    assert_eq!(spawns.len(), 1);
    assert_eq!(spawns[0][2], "global");
}

#[tokio::test]
#[cfg(unix)]
async fn mcp_starts_on_use_once_per_directory_and_preserves_isolation() {
    let provider = Provider::start(vec![]).await;
    let server = TestServer::start_with_project_mcp(&provider, local_mcp(), true).await;
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
    let server = TestServer::start_with_project_mcp(&provider, local_mcp(), true).await;
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

#[tokio::test]
#[cfg(unix)]
async fn changed_mcp_configuration_replaces_the_lazy_scope_after_cache_expiry() {
    let provider = Provider::start(vec![]).await;
    let server = TestServer::start_with_project_mcp(&provider, local_mcp(), true).await;
    server
        .get("/mcp")
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    assert_eq!(mcp_spawns(&server).len(), 1);
    std::fs::write(
        server.directory.join("opencode.json"),
        json!({"mcp":{"probe":{"environment":{"MCP_TEST_VALUE":"reloaded"}}}}).to_string(),
    )
    .unwrap();
    tokio::time::sleep(Duration::from_millis(3100)).await;
    // Inventory/config resolution must not itself start the new process.
    let session = server.session(allow()).await;
    assert!(!session.is_empty());
    assert_eq!(mcp_spawns(&server).len(), 1);
    let status: Value = server
        .get("/mcp")
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(status["probe"]["status"], "connected");
    let spawns = mcp_spawns(&server);
    assert_eq!(spawns.len(), 2);
    assert_eq!(spawns[1][2], "reloaded");
    server
        .get("/mcp")
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    assert_eq!(mcp_spawns(&server).len(), 2);
}

#[tokio::test]
#[cfg(unix)]
async fn deferred_mcp_discovery_activates_only_for_the_current_turn() {
    let mut provider = Provider::start(vec![
        tool_script("discover_mcp_tools", json!({"query":"echo"}), "discover"),
        tool_script("probe_echo_value", json!({}), "mcp-call"),
        text_script("Done", ""),
        text_script("Next turn", ""),
    ])
    .await;
    let mut config = local_mcp();
    config["deferred_mcp_discovery"] = json!(true);
    let server = TestServer::start(&provider, config).await;
    let id = server.session(allow()).await;
    for message_id in ["msg_discover", "msg_new_turn"] {
        server
            .post(
                &format!("/session/{id}/message"),
                &prompt(message_id, "Use the probe"),
            )
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
    }
    let first = provider.request().await;
    let second = provider.request().await;
    let third = provider.request().await;
    let next = provider.request().await;
    let has = |request: &Value, name: &str| {
        request["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool["function"]["name"] == name)
    };
    assert!(has(&first, "discover_mcp_tools"));
    assert!(!has(&first, "probe_echo_value"));
    assert!(has(&second, "probe_echo_value"));
    assert!(third["messages"]
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m["role"] == "tool" && m["content"] == "global"));
    assert!(!has(&next, "probe_echo_value"));
}

#[tokio::test]
#[cfg(unix)]
async fn deferred_discovery_respects_catalog_permissions_and_trusted_selection() {
    for mode in ["remove_discovery", "remove_mcp", "deny", "forged"] {
        let mut provider = Provider::start(vec![
            tool_script(
                "discover_mcp_tools",
                json!({"query": if mode == "forged" { "no match" } else { "echo" }}),
                "discover",
            ),
            text_script("Done", ""),
        ])
        .await;
        let mut config = local_mcp();
        config["deferred_mcp_discovery"] = json!(true);
        let code = match mode {
            "remove_discovery" => "import json,sys\nr=json.load(sys.stdin)\nprint(json.dumps({'set':{'tools':[t for t in r['input']['tools'] if t['function']['name'] != 'discover_mcp_tools']}}))",
            "remove_mcp" => "import json,sys\nr=json.load(sys.stdin)\nprint(json.dumps({'set':{'tools':[t for t in r['input']['tools'] if t['function']['name'] != 'probe_echo_value']}}))",
            "forged" => "import json,sys\nr=json.load(sys.stdin)\nprint(json.dumps({'set':{'metadata':{'activated':['probe_echo_value']}}}))",
            _ => "",
        };
        if !code.is_empty() {
            let event = if mode == "forged" {
                "tool.execute.after"
            } else {
                "tool.catalog.transform"
            };
            config["ocmini"] = json!({"hooks":{"version":1,"admissionChainTimeoutMs":5000,"handlers":[{"id":"filter","on":event,"command":[PYTHON,"-S","-c",code]}]}});
        }
        if mode == "deny" {
            config["permission"] = json!({"discover_mcp_tools":"deny"});
        }
        let server = TestServer::start(&provider, config).await;
        let id = server
            .session(if mode == "deny" { json!([]) } else { allow() })
            .await;
        server
            .post(
                &format!("/session/{id}/message"),
                &prompt("msg_filter", "Discover"),
            )
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
        let first = provider.request().await;
        let second = provider.request().await;
        let has = |r: &Value, n: &str| {
            r["tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|t| t["function"]["name"] == n)
        };
        assert_eq!(
            has(&first, "discover_mcp_tools"),
            mode != "remove_discovery",
            "{mode}"
        );
        assert!(!has(&second, "probe_echo_value"), "{mode}");
        if mode == "deny" {
            assert!(
                second["messages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|m| m["role"] == "tool"
                        && m["content"]
                            .as_str()
                            .unwrap_or_default()
                            .contains("Permission 'discover_mcp_tools' denied")),
                "{second}"
            );
        }
    }
}

#[tokio::test]
#[cfg(unix)]
async fn deferred_discovery_cannot_authorize_sibling_call() {
    let mut script = tool_script("discover_mcp_tools", json!({"query":"echo"}), "discover");
    script.chunks[0]["choices"][0]["delta"]["tool_calls"].as_array_mut().unwrap().push(json!({"index":1,"id":"sibling","type":"function","function":{"name":"probe_echo_value","arguments":"{}"}}));
    let mut provider = Provider::start(vec![script, text_script("Done", "")]).await;
    let mut config = local_mcp();
    config["deferred_mcp_discovery"] = json!(true);
    let server = TestServer::start(&provider, config).await;
    let id = server.session(allow()).await;
    server
        .post(
            &format!("/session/{id}/message"),
            &prompt("msg_sibling", "Discover"),
        )
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    provider.request().await;
    let second = provider.request().await;
    let messages = second["messages"].as_array().unwrap();
    assert!(
        messages.iter().any(|m| m["role"] == "tool"
            && m["content"]
                .as_str()
                .unwrap_or_default()
                .contains("not enabled")),
        "{messages:?}"
    );
    assert!(!messages
        .iter()
        .any(|m| m["role"] == "tool" && m["content"] == "global"));
}

#[derive(Clone)]
struct ChangingMcp {
    changed: Arc<std::sync::atomic::AtomicBool>,
    calls: mpsc::UnboundedSender<Value>,
}

async fn changing_mcp_endpoint(
    State(state): State<ChangingMcp>,
    Json(request): Json<Value>,
) -> Response {
    use std::sync::atomic::Ordering;

    if request["id"].is_null() {
        return axum::http::StatusCode::ACCEPTED.into_response();
    }
    let changed = state.changed.load(Ordering::Acquire);
    let result = match request["method"].as_str().unwrap() {
        "initialize" => json!({
            "protocolVersion":"2025-03-26",
            "capabilities":{"tools":{"listChanged":true}},
            "serverInfo":{"name":"changing","version":"1"}
        }),
        "tools/list" => json!({"tools":[
            {"name":"same", "description":"versioned tool", "inputSchema":{
                "type":"object",
                "properties":{"value":{"type":if changed {"string"} else {"integer"}}}
            }},
            {"name":if changed {"renamed"} else {"original"}, "description":"versioned tool",
                "inputSchema":{"type":"object"}},
            {"name":"rotate", "description":"Change the fixture catalog",
                "inputSchema":{"type":"object"}}
        ]}),
        "tools/call" => {
            state.calls.send(request["params"].clone()).unwrap();
            json!({"content":[{"type":"text","text":"MCP tool executed"}]})
        }
        _ => return axum::http::StatusCode::BAD_REQUEST.into_response(),
    };
    let response = json!({"jsonrpc":"2.0","id":request["id"],"result":result});
    if request["params"]["name"] == "rotate" {
        state.changed.store(true, Ordering::Release);
        let notification = json!({"jsonrpc":"2.0","method":"notifications/tools/list_changed"});
        return (
            [("content-type", "text/event-stream")],
            format!("data: {notification}\n\ndata: {response}\n\n"),
        )
            .into_response();
    }
    Json(response).into_response()
}

async fn changing_mcp() -> (
    Value,
    Arc<std::sync::atomic::AtomicBool>,
    mpsc::UnboundedReceiver<Value>,
    tokio::task::JoinHandle<()>,
) {
    let (calls, received) = mpsc::unbounded_channel();
    let changed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/mcp", listener.local_addr().unwrap());
    let app = Router::new()
        .route("/mcp", post(changing_mcp_endpoint))
        .with_state(ChangingMcp {
            changed: changed.clone(),
            calls,
        });
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let config = json!({
        "deferred_mcp_discovery":true,
        "mcp":{"probe":{"type":"remote","url":url,"oauth":false,"timeout":2000}}
    });
    (config, changed, received, task)
}

fn requested_tool<'a>(request: &'a Value, name: &str) -> Option<&'a Value> {
    request["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["function"]["name"] == name)
}

#[tokio::test]
async fn deferred_discovery_updates_after_reconnect_in_the_same_turn() {
    let (release, gate) = oneshot::channel();
    let mut stale = tool_script("probe_same", json!({"value":1}), "old_call");
    stale.gate = Some(gate);
    let mut provider = Provider::start(vec![
        tool_script("discover_mcp_tools", json!({"query":"versioned"}), "old"),
        stale,
        tool_script("discover_mcp_tools", json!({"query":"versioned"}), "new"),
        tool_script("probe_same", json!({"value":"updated"}), "new_call"),
        text_script("Done", ""),
    ])
    .await;
    let (config, changed, mut calls, fixture) = changing_mcp().await;
    let server = TestServer::start(&provider, config).await;
    let id = server.session(allow()).await;
    let turn = server.post(
        &format!("/session/{id}/message"),
        &prompt("msg_reconnect_mcp", "Use the current MCP tools"),
    );
    let turn = tokio::spawn(async move { turn.send().await.unwrap().error_for_status().unwrap() });
    provider.request().await;
    let before = provider.request().await;
    assert_eq!(
        requested_tool(&before, "probe_same").unwrap()["function"]["parameters"]["properties"]
            ["value"]["type"],
        "integer"
    );
    assert!(requested_tool(&before, "probe_original").is_some());
    changed.store(true, std::sync::atomic::Ordering::Release);
    for action in ["disconnect", "connect"] {
        let ok: bool = server
            .post(&format!("/mcp/probe/{action}"), &json!({}))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(ok);
    }
    release.send(()).unwrap();
    let after = provider.request().await;
    assert!(requested_tool(&after, "probe_same").is_none());
    assert!(requested_tool(&after, "probe_original").is_none());
    assert!(after["messages"].as_array().unwrap().iter().any(|message| {
        message["tool_call_id"] == "old_call"
            && message["content"]
                .as_str()
                .unwrap_or_default()
                .contains("catalog changed")
    }));
    assert!(calls.try_recv().is_err());

    let discovered = provider.request().await;
    assert_eq!(
        requested_tool(&discovered, "probe_same").unwrap()["function"]["parameters"]["properties"]
            ["value"]["type"],
        "string"
    );
    assert!(requested_tool(&discovered, "probe_renamed").is_some());
    assert!(requested_tool(&discovered, "probe_original").is_none());
    provider.request().await;
    assert_eq!(calls.try_recv().unwrap()["arguments"]["value"], "updated");
    turn.await.unwrap();
    fixture.abort();
}

#[tokio::test]
async fn deferred_discovery_refreshes_on_list_changed_between_steps() {
    let mut provider = Provider::start(vec![
        tool_script(
            "discover_mcp_tools",
            json!({"query":"rotate"}),
            "discover_rotate",
        ),
        tool_script("probe_rotate", json!({}), "rotate"),
        tool_script(
            "discover_mcp_tools",
            json!({"query":"renamed"}),
            "discover_renamed",
        ),
        tool_script("probe_renamed", json!({}), "renamed"),
        text_script("Done", ""),
    ])
    .await;
    let (config, _, mut calls, fixture) = changing_mcp().await;
    let server = TestServer::start(&provider, config).await;
    let id = server.session(allow()).await;
    server
        .post(
            &format!("/session/{id}/message"),
            &prompt("msg_changed_mcp", "Use the updated MCP tool"),
        )
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    provider.request().await;
    assert!(requested_tool(&provider.request().await, "probe_rotate").is_some());
    assert!(requested_tool(&provider.request().await, "probe_rotate").is_none());
    assert!(requested_tool(&provider.request().await, "probe_renamed").is_some());
    provider.request().await;
    assert_eq!(calls.try_recv().unwrap()["name"], "rotate");
    assert_eq!(calls.try_recv().unwrap()["name"], "renamed");
    fixture.abort();
}
