use super::*;

async fn session(server: &TestServer) -> String {
    server.post("/api/session",&json!({"title":"V2 test","location":{"directory":server.directory},"model":{"id":"test-model","providerID":"mock"},"permissions":[{"action":"*","resource":"*","effect":"allow"}]})).send().await.unwrap().error_for_status().unwrap().json::<Value>().await.unwrap()["data"]["id"].as_str().unwrap().into()
}
async fn post(server: &TestServer, path: &str, body: Value) -> Value {
    let r = server.post(path, &body).send().await.unwrap();
    let status = r.status();
    let text = r.text().await.unwrap();
    assert!(status.is_success(), "{path}: {status} {text}");
    if text.is_empty() {
        Value::Null
    } else {
        serde_json::from_str(&text).unwrap()
    }
}
async fn get(server: &TestServer, path: &str) -> Value {
    server
        .get(path)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap()
}

#[tokio::test]
async fn v2_streams_native_lifecycle_and_persists_selection_instructions_and_history() {
    let mut provider = Provider::start(vec![
        text_script("Hello ", "v2"),
        text_script("Resumed", ""),
    ])
    .await;
    let mut server = TestServer::start(&provider, json!({})).await;
    let sid = session(&server).await;
    let mut events = server.events_at("/api/event").await;
    let response = server
        .client
        .put(format!(
            "{}/api/experimental/session/{sid}/instructions/entries/t3-code",
            server.url
        ))
        .json(&json!({"value":"V2_SYSTEM_SENTINEL"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 204);
    let receipt = post(
        &server,
        &format!("/api/session/{sid}/prompt"),
        json!({"id":"msg_t3_turn_ses:attempt:1","text":"Hello"}),
    )
    .await;
    assert_eq!(receipt["data"]["type"], "user");
    let observed = events.through("session.execution.succeeded").await;
    let types: Vec<_> = observed.iter().filter_map(|v| v["type"].as_str()).collect();
    for t in [
        "session.inbox.enqueued",
        "session.execution.started",
        "session.inbox.delivered",
        "session.step.started",
        "session.text.started",
        "session.text.delta",
        "session.text.ended",
        "session.step.ended",
    ] {
        assert!(types.contains(&t), "missing {t}: {types:?}");
    }
    let mut sequence = 0;
    for e in &observed {
        if let Some(seq) = e["durable"]["seq"].as_i64() {
            assert!(seq > sequence);
            sequence = seq;
            assert_eq!(e["durable"]["aggregateID"], sid);
        }
    }
    let request = provider.request().await;
    assert!(request["messages"][0]["content"]
        .as_str()
        .unwrap()
        .contains("V2_SYSTEM_SENTINEL"));
    assert!(request["tools"]
        .as_array()
        .unwrap()
        .iter()
        .any(|t| t["function"]["name"] == "subagent"));
    let history = get(&server, &format!("/api/session/{sid}/message")).await;
    assert_eq!(history["data"][0]["id"], "msg_t3_turn_ses:attempt:1");
    assert!(history["data"]
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m["type"] == "assistant" && m["content"][0]["text"] == "Hello v2"));
    assert_eq!(
        get(&server, &format!("/api/session/{sid}")).await["data"]["outcome"],
        "succeeded"
    );
    server.restart().await;
    let mut events = server.events_at("/api/event").await;
    post(
        &server,
        &format!("/api/session/{sid}/prompt"),
        json!({"text":"Continue"}),
    )
    .await;
    events.through("session.execution.succeeded").await;
    assert!(provider.request().await["messages"][0]["content"]
        .as_str()
        .unwrap()
        .contains("V2_SYSTEM_SENTINEL"));
}

#[tokio::test]
async fn v2_deferred_inputs_are_hidden_cancelled_and_delivered_without_duplicate_execution() {
    let mut provider = Provider::start(vec![text_script("Done", "")]).await;
    let server = TestServer::start(&provider, json!({})).await;
    let sid = session(&server).await;
    post(
        &server,
        &format!("/api/session/{sid}/prompt"),
        json!({"id":"msg_pending","text":"Keep pending","resume":false}),
    )
    .await;
    assert_eq!(
        get(&server, &format!("/api/session/{sid}/message")).await["data"],
        json!([])
    );
    assert_eq!(
        get(&server, &format!("/api/session/{sid}/inbox")).await["data"][0]["id"],
        "msg_pending"
    );
    let r = server
        .client
        .delete(format!(
            "{}/api/session/{sid}/inbox/msg_pending",
            server.url
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);
    let mut events = server.events_at("/api/event").await;
    post(
        &server,
        &format!("/api/session/{sid}/prompt"),
        json!({"id":"msg_once","text":"Run"}),
    )
    .await;
    events.through("session.execution.succeeded").await;
    post(
        &server,
        &format!("/api/session/{sid}/prompt"),
        json!({"id":"msg_once","text":"Different retry"}),
    )
    .await;
    provider.request().await;
    assert!(
        tokio::time::timeout(Duration::from_millis(100), provider.requests.recv())
            .await
            .is_err()
    );
    let history = get(&server, &format!("/api/session/{sid}/message")).await;
    assert!(!history["data"]
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m["id"] == "msg_pending"));
}

#[tokio::test]
async fn v2_history_revert_fork_move_and_pagination_preserve_files_and_instructions() {
    let provider = Provider::start(vec![text_script("A", ""), text_script("B", "")]).await;
    let server = TestServer::start(&provider, json!({})).await;
    let sid = session(&server).await;
    let file = server.directory.join("keep.txt");
    std::fs::write(&file, "unchanged").unwrap();
    let mut events = server.events_at("/api/event").await;
    for (id, text) in [("msg_a", "First"), ("msg_b", "Second")] {
        post(
            &server,
            &format!("/api/session/{sid}/prompt"),
            json!({"id":id,"text":text}),
        )
        .await;
        events.through("session.execution.succeeded").await;
    }
    let page = get(&server, &format!("/api/session/{sid}/message?limit=2")).await;
    assert_eq!(page["data"].as_array().unwrap().len(), 2);
    let cursor = page["cursor"]["next"].as_str().unwrap();
    assert!(!get(
        &server,
        &format!("/api/session/{sid}/message?cursor={cursor}")
    )
    .await["data"]
        .as_array()
        .unwrap()
        .is_empty());
    post(
        &server,
        &format!("/api/session/{sid}/revert/stage"),
        json!({"messageID":"msg_b","files":false}),
    )
    .await;
    post(
        &server,
        &format!("/api/session/{sid}/revert/commit"),
        json!({}),
    )
    .await;
    assert!(
        !get(&server, &format!("/api/session/{sid}/message")).await["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["id"] == "msg_b")
    );
    assert_eq!(std::fs::read_to_string(file).unwrap(), "unchanged");
    let fork = post(&server, &format!("/api/session/{sid}/fork"), json!({})).await;
    let child = fork["data"]["id"].as_str().unwrap();
    assert_eq!(fork["data"]["cost"], 0.0);
    assert_eq!(fork["data"]["fork"]["sessionID"], sid);
    assert!(fork["data"].get("parentID").is_none());
    let destination = server.root.path().join("other");
    std::fs::create_dir(&destination).unwrap();
    post(
        &server,
        &format!("/api/session/{child}/move"),
        json!({"directory":destination}),
    )
    .await;
    events.through("session.moved").await;
    assert_eq!(
        get(&server, &format!("/api/session/{child}")).await["data"]["location"]["directory"],
        destination.to_string_lossy().as_ref()
    );
}

#[tokio::test]
async fn v2_subagent_tools_publish_child_linkage_and_native_question_forms() {
    let mut provider=Provider::start(vec![tool_script("subagent",json!({"description":"Child","prompt":"Answer","subagent_type":"general"}),"call_child"),text_script("Child result",""),tool_script("question",json!({"questions":[{"header":"Pick","question":"Which?","options":[{"label":"A","description":"Option A"}]}]}),"call_question"),text_script("Parent result","")]).await;
    let server = TestServer::start(&provider, json!({})).await;
    let sid = session(&server).await;
    let mut events = server.events_at("/api/event").await;
    post(
        &server,
        &format!("/api/session/{sid}/prompt"),
        json!({"text":"Delegate and ask"}),
    )
    .await;
    let observed = events.through("form.created").await;
    assert!(observed.iter().any(|e| e["type"] == "session.tool.progress"
        && e["data"]["metadata"]["sessionID"]
            .as_str()
            .is_some_and(|id| id != sid)));
    let form = &observed.last().unwrap()["data"]["form"];
    assert!(form["id"].as_str().unwrap().starts_with("frm_"));
    post(
        &server,
        &format!(
            "/api/session/{sid}/form/{}/reply",
            form["id"].as_str().unwrap()
        ),
        json!({"answer":{"0":"A"}}),
    )
    .await;
    events.through("session.execution.succeeded").await;
    for _ in 0..4 {
        provider.request().await;
    }
}

#[tokio::test]
#[ignore = "development-only: bun install --cwd tests/v2-client; then run this test explicitly"]
async fn official_v2_client_and_schema_conformance() {
    let provider = Provider::start(vec![
        tool_script(
            "shell",
            json!({"command":"printf sdk-shell"}),
            "call_sdk_shell",
        ),
        tool_script(
            "question",
            json!({"questions":[{"header":"Pick","question":"Which?","options":[{"label":"A","description":"Option A"}]}]}),
            "call_sdk_form",
        ),
        text_script("Native SDK reply", ""),
    ])
    .await;
    let server = TestServer::start(&provider, json!({})).await;
    let output = Command::new("bun")
        .arg("tests/v2-client/check.ts")
        .env("OCMINI_TEST_URL", &server.url)
        .env("OCMINI_TEST_DIRECTORY", &server.directory)
        .env("OCMINI_TEST_PASSWORD", PASSWORD)
        .output()
        .await
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[tokio::test]
async fn v2_interrupt_preserves_queued_inputs_and_does_not_replay_cancelled_work() {
    let (_release, gate) = oneshot::channel();
    let mut stalled = text_script("Partial", "");
    stalled.gate = Some(gate);
    let mut provider = Provider::start(vec![
        stalled,
        text_script("Queued result", ""),
        text_script("Final result", ""),
    ])
    .await;
    let server = TestServer::start(&provider, json!({})).await;
    let sid = session(&server).await;
    let mut events = server.events_at("/api/event").await;
    post(
        &server,
        &format!("/api/session/{sid}/prompt"),
        json!({"text":"Stall"}),
    )
    .await;
    events.through("session.text.delta").await;
    post(
        &server,
        &format!("/api/session/{sid}/prompt"),
        json!({"id":"msg_queued_keep","text":"Keep queued","delivery":"queue","resume":false}),
    )
    .await;
    assert_eq!(
        post(&server, &format!("/api/session/{sid}/interrupt"), json!({})).await["interrupted"],
        true
    );
    let ended = events.through("session.execution.interrupted").await;
    assert_eq!(ended.last().unwrap()["data"]["reason"], "user");
    assert_eq!(
        get(&server, &format!("/api/session/{sid}/inbox")).await["data"][0]["id"],
        "msg_queued_keep"
    );
    post(
        &server,
        &format!("/api/session/{sid}/prompt"),
        json!({"text":"Continue"}),
    )
    .await;
    events.through("session.execution.succeeded").await;
    let history = get(&server, &format!("/api/session/{sid}/message")).await;
    assert!(history["data"]
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m["id"] == "msg_queued_keep"));
    for _ in 0..3 {
        provider.request().await;
    }
}

#[tokio::test]
async fn v2_compaction_uses_native_shapes_and_saved_instructions_survive() {
    let mut provider = Provider::start(vec![
        text_script("Remembered", ""),
        text_script("Continuation summary", ""),
        text_script("Still remembered", ""),
    ])
    .await;
    let server = TestServer::start(&provider, json!({})).await;
    let sid = session(&server).await;
    let mut events = server.events_at("/api/event").await;
    post(
        &server,
        &format!("/api/session/{sid}/prompt"),
        json!({"text":"Remember the fact"}),
    )
    .await;
    events.through("session.execution.succeeded").await;
    post(
        &server,
        &format!("/api/session/{sid}/compact"),
        json!({"id":"msg_compact"}),
    )
    .await;
    let compacted = events.through("session.execution.succeeded").await;
    assert!(compacted
        .iter()
        .any(|e| e["type"] == "session.compaction.started"));
    assert!(compacted
        .iter()
        .any(|e| e["type"] == "session.compaction.ended"));
    let history = get(&server, &format!("/api/session/{sid}/message")).await;
    let summary = history["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["type"] == "compaction")
        .unwrap();
    assert_eq!(summary["summary"], "Continuation summary");
    assert!(summary.get("recent").is_none());
    post(
        &server,
        &format!("/api/session/{sid}/prompt"),
        json!({"text":"Recall"}),
    )
    .await;
    events.through("session.execution.succeeded").await;
    provider.request().await;
    provider.request().await;
    assert!(provider.request().await["messages"]
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m["content"]
            .as_str()
            .is_some_and(|t| t.contains("Continuation summary"))));
}

#[tokio::test]
async fn v2_background_children_publish_running_metadata_and_synthetic_reports() {
    let (release, gate) = oneshot::channel();
    let mut child = text_script("Child background", "result");
    child.gate = Some(gate);
    let mut provider=Provider::start(vec![tool_script("subagent",json!({"agent":"general","description":"Background child","prompt":"Work","background":true}),"call_background"),child,text_script("Parent paused", ""),text_script("Report handled", "")]).await;
    let server = TestServer::start(&provider, json!({})).await;
    let sid = session(&server).await;
    let mut events = server.events_at("/api/event").await;
    post(
        &server,
        &format!("/api/session/{sid}/prompt"),
        json!({"text":"Run background"}),
    )
    .await;
    let initial = events.through("session.execution.succeeded").await;
    assert!(initial.iter().any(
        |e| e["type"] == "session.tool.success" && e["data"]["metadata"]["status"] == "running"
    ));
    release.send(()).unwrap();
    let mut report = None;
    for _ in 0..100 {
        let e = events.next().await;
        if e["type"] == "session.inbox.enqueued" && e["data"]["item"]["type"] == "synthetic" {
            report = Some(e);
            break;
        }
    }
    let report = report.expect("No background report");
    assert_eq!(
        report["data"]["item"]["payload"]["metadata"]["source"],
        "subagent"
    );
    assert!(report["data"]["item"]["payload"]["metadata"]["childID"]
        .as_str()
        .unwrap()
        .starts_with("ses_"));
    for _ in 0..4 {
        provider.request().await;
    }
}

#[tokio::test]
async fn v2_native_configuration_and_shell_permission_rules_reach_the_model() {
    let mut provider = Provider::start(vec![
        tool_script("shell", json!({"command":"echo v2-shell"}), "call_shell"),
        text_script("Allowed", ""),
    ])
    .await;
    let server=TestServer::start(&provider,json!({"model":{"providerID":"mock","model":"test-model"},"providers":{"mock":{"package":"@opencode/ai/providers/openai-compatible","settings":{"baseURL":provider.url},"models":{"test-model":{"variants":[{"id":"focused","settings":{"reasoningEffort":"high"}}]}}}}})).await;
    let sid = session(&server).await;
    let response = server
        .client
        .patch(format!("{}/api/session/{sid}", server.url))
        .json(&json!({"permissions":[{"action":"shell","resource":"*","effect":"ask"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 204);
    post(
        &server,
        &format!("/api/session/{sid}/model"),
        json!({"model":{"providerID":"mock","id":"test-model","variant":"focused"}}),
    )
    .await;
    let mut events = server.events_at("/api/event").await;
    post(
        &server,
        &format!("/api/session/{sid}/prompt"),
        json!({"text":"Run shell"}),
    )
    .await;
    let asked = events.through("permission.asked").await;
    let request = &asked.last().unwrap()["data"];
    assert_eq!(request["action"], "shell");
    assert_eq!(
        get(&server, &format!("/api/session/{sid}/permission")).await["data"][0]["id"],
        request["id"]
    );
    post(
        &server,
        &format!(
            "/api/session/{sid}/permission/{}/reply",
            request["id"].as_str().unwrap()
        ),
        json!({"decision":"once"}),
    )
    .await;
    events.through("session.execution.succeeded").await;
    assert_eq!(provider.request().await["reasoning_effort"], "high");
    provider.request().await;
}

#[tokio::test]
#[cfg(unix)]
async fn v2_mcp_registration_reuses_the_connection_and_removal_stops_tools() {
    let mut provider = Provider::start(vec![
        tool_script("runtime_echo_value", json!({}), "call_mcp"),
        text_script("MCP complete", ""),
    ])
    .await;
    let server = TestServer::start(&provider, json!({})).await;
    let config = super::mcp::local_mcp()["mcp"]["probe"].clone();
    for _ in 0..2 {
        let response = server
            .client
            .put(format!("{}/api/experimental/mcp/runtime", server.url))
            .json(&json!({"config":config}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 204, "{}", response.text().await.unwrap());
    }
    assert_eq!(super::mcp::mcp_spawns(&server).len(), 1);
    let sid = session(&server).await;
    let mut events = server.events_at("/api/event").await;
    post(
        &server,
        &format!("/api/session/{sid}/prompt"),
        json!({"text":"Use MCP"}),
    )
    .await;
    let observed = events.through("session.execution.succeeded").await;
    assert!(observed.iter().any(
        |e| e["type"] == "session.tool.success" && e["data"]["content"][0]["text"] == "global"
    ));
    provider.request().await;
    provider.request().await;
    let response = server
        .client
        .delete(format!("{}/api/experimental/mcp/runtime", server.url))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 204);
    assert_eq!(get(&server, "/mcp").await, json!({}));
    let response = server
        .client
        .put(format!("{}/api/experimental/mcp/broken", server.url))
        .json(&json!({"config":{"type":"local","command":["/no-such-ocmini-test-command"]}}))
        .send()
        .await
        .unwrap();
    assert!(!response.status().is_success());
}

#[tokio::test]
async fn v2_permission_denial_feedback_reaches_the_model_without_executing_the_tool() {
    let mut provider = Provider::start(vec![
        tool_script(
            "shell",
            json!({"command":"touch denied-file"}),
            "call_denied",
        ),
        text_script("Understood", ""),
    ])
    .await;
    let server = TestServer::start(&provider, json!({})).await;
    let sid = session(&server).await;
    server
        .client
        .patch(format!("{}/api/session/{sid}", server.url))
        .json(&json!({"permissions":[{"action":"shell","resource":"*","effect":"ask"}]}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let mut events = server.events_at("/api/event").await;
    post(
        &server,
        &format!("/api/session/{sid}/prompt"),
        json!({"text":"Create file"}),
    )
    .await;
    let observed = events.through("permission.asked").await;
    let id = observed.last().unwrap()["data"]["id"].as_str().unwrap();
    post(
        &server,
        &format!("/api/session/{sid}/permission/{id}/reply"),
        json!({"decision":"reject","message":"Explain the change first"}),
    )
    .await;
    events.through("session.execution.succeeded").await;
    provider.request().await;
    let next = provider.request().await;
    assert!(next["messages"]
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m["role"] == "tool"
            && m["content"]
                .as_str()
                .is_some_and(|text| text.contains("Explain the change first"))));
    assert!(!server.directory.join("denied-file").exists());
}

#[tokio::test]
async fn v2_failed_compaction_settles_with_native_failure_and_history() {
    let provider = Provider::start(vec![
        text_script("Initial context", ""),
        Script::http_status_body(400, "Cannot summarize"),
    ])
    .await;
    let server = TestServer::start(&provider, json!({})).await;
    let sid = session(&server).await;
    let mut events = server.events_at("/api/event").await;
    post(
        &server,
        &format!("/api/session/{sid}/prompt"),
        json!({"text":"Start"}),
    )
    .await;
    events.through("session.execution.succeeded").await;
    post(&server, &format!("/api/session/{sid}/compact"), json!({})).await;
    let observed = events.through("session.execution.failed").await;
    let failed = observed
        .iter()
        .find(|e| e["type"] == "session.compaction.failed")
        .expect("native compaction failure");
    assert_eq!(failed["data"]["reason"], "manual");
    assert!(!observed
        .iter()
        .any(|e| e["type"] == "session.compaction.ended"));
    let history = get(&server, &format!("/api/session/{sid}/message")).await;
    assert!(history["data"]
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m["type"] == "compaction" && m["status"] == "failed" && m["reason"] == "manual"));
}

#[tokio::test]
async fn v2_cascade_deletion_preserves_hook_vetoes_before_removing_children() {
    let provider = Provider::start(vec![]).await;
    let config = json!({"ocmini":{"hooks":{"version":1,"handlers":[{"id":"veto-parent-delete","on":"session.delete.before","command":["python3","-c","import json,sys; r=json.load(sys.stdin); print(json.dumps({'deny':'Keep this parent'} if not r['input']['info'].get('parentID') else {}))"]}]}}});
    let server = TestServer::start(&provider, config).await;
    let sid = session(&server).await;
    let child = post(&server, "/api/session", json!({"parentID":sid})).await["data"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let response = server
        .client
        .delete(format!("{}/api/session/{sid}", server.url))
        .send()
        .await
        .unwrap();
    assert!(!response.status().is_success());
    assert_eq!(
        get(&server, &format!("/api/session/{sid}")).await["data"]["id"],
        sid
    );
    assert_eq!(
        get(&server, &format!("/api/session/{child}")).await["data"]["parentID"],
        sid
    );
}

#[tokio::test]
async fn v2_adopts_legacy_history_and_revert_clear_wakes_real_pending_work() {
    let mut provider = Provider::start(vec![
        text_script("Legacy answer", ""),
        text_script("Native continuation", ""),
    ])
    .await;
    let server = TestServer::start(&provider, json!({})).await;
    let sid = server.session(allow()).await;
    let old = server
        .post(
            &format!("/session/{sid}/message"),
            &prompt("msg_legacy_seed", "Keep my context"),
        )
        .send()
        .await
        .unwrap();
    assert!(old.status().is_success());
    let history = get(&server, &format!("/api/session/{sid}/message")).await;
    assert!(history["data"]
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m["type"] == "assistant" && m["content"][0]["text"] == "Legacy answer"));
    post(
        &server,
        &format!("/api/session/{sid}/prompt"),
        json!({"id":"msg_native_pending","text":"Continue my context","resume":false}),
    )
    .await;
    let mut events = server.events_at("/api/event").await;
    let response = server
        .client
        .delete(format!("{}/api/session/{sid}/revert", server.url))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 204);
    let observed = events.through("session.execution.succeeded").await;
    assert!(observed
        .iter()
        .any(|e| e["type"] == "session.text.ended" && e["data"]["text"] == "Native continuation"));
    assert!(
        get(&server, &format!("/api/session/{sid}/inbox")).await["data"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    provider.request().await;
    let next = provider.request().await;
    assert!(next["messages"]
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m["role"] == "assistant" && m["content"] == "Legacy answer"));
}

#[tokio::test]
async fn v2_session_pages_keep_filters_and_can_page_backwards_with_tied_timestamps() {
    let provider = Provider::start(vec![]).await;
    let server = TestServer::start(&provider, json!({})).await;
    let mut ids = Vec::new();
    for _ in 0..7 {
        ids.push(session(&server).await);
    }
    let path = server.root.path().join(".local/share/ocmini/ocmini.db");
    let db = rusqlite::Connection::open(path).unwrap();
    db.execute(
        "UPDATE session SET time_updated=100,json=json_set(json,'$.time.updated',100)",
        [],
    )
    .unwrap();
    ids.sort();
    let first = get(
        &server,
        "/api/session?limit=2&order=asc&parentID=null&search=V2",
    )
    .await;
    assert_eq!(first["data"][0]["id"], ids[0]);
    assert_eq!(first["data"][1]["id"], ids[1]);
    assert!(first["cursor"].get("previous").is_none());
    let next = first["cursor"]["next"].as_str().unwrap();
    let second = get(&server, &format!("/api/session?limit=2&cursor={next}")).await;
    assert_eq!(second["data"][0]["id"], ids[2]);
    let previous = second["cursor"]["previous"].as_str().unwrap();
    let back = get(&server, &format!("/api/session?limit=2&cursor={previous}")).await;
    assert_eq!(back["data"], first["data"]);
    let mut collected = first["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["id"].as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    let mut page = first;
    while let Some(next) = page["cursor"]["next"].as_str() {
        page = get(&server, &format!("/api/session?limit=2&cursor={next}")).await;
        collected.extend(
            page["data"]
                .as_array()
                .unwrap()
                .iter()
                .map(|s| s["id"].as_str().unwrap().to_owned()),
        );
    }
    assert_eq!(collected, ids);
    assert!(
        get(&server, "/api/session?search=no-such-title").await["data"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    for query in [
        "limit=0",
        "order=wrong",
        "archived=maybe",
        "cursor=invalid",
        "project=global&subpath=child",
    ] {
        assert_eq!(
            server
                .get(&format!("/api/session?{query}"))
                .send()
                .await
                .unwrap()
                .status(),
            400
        );
    }
}

#[tokio::test]
async fn v2_archive_filters_and_native_update_events_survive_restart() {
    let provider = Provider::start(vec![]).await;
    let mut server = TestServer::start(&provider, json!({})).await;
    let sid = session(&server).await;
    let mut events=server.events_at("/api/event?types=session.archived&types=session.unarchived&types=session.metadata.updated&types=session.renamed").await;
    let patch = |body: Value| {
        server
            .client
            .patch(format!("{}/api/session/{sid}", server.url))
            .json(&body)
            .send()
    };
    patch(json!({"archived":true,"metadata":{"source":"archive-test"}}))
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let observed = events.through("session.archived").await;
    assert!(observed
        .iter()
        .any(|e| e["type"] == "session.metadata.updated"));
    assert!(!observed.iter().any(|e| e["type"] == "session.renamed"));
    let archived =
        get(&server, &format!("/api/session/{sid}")).await["data"]["time"]["archived"].clone();
    assert!(archived.is_number());
    let before_time =
        get(&server, &format!("/api/session/{sid}")).await["data"]["time"]["updated"].clone();
    patch(json!({"archived":true}))
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    assert_eq!(
        get(&server, &format!("/api/session/{sid}")).await["data"]["time"]["updated"],
        before_time,
        "repeating archive state is a no-op"
    );
    assert_eq!(
        get(&server, &format!("/api/session/{sid}")).await["data"]["time"]["archived"],
        archived
    );
    assert_eq!(
        get(&server, "/api/session?archived=true").await["data"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert!(get(&server, "/api/session?archived=false").await["data"]
        .as_array()
        .unwrap()
        .is_empty());
    assert_eq!(
        patch(json!({"archived":"yes"})).await.unwrap().status(),
        400
    );
    drop(events);
    server.restart().await;
    assert_eq!(
        get(&server, &format!("/api/session/{sid}")).await["data"]["time"]["archived"],
        archived
    );
    let mut events = server
        .events_at("/api/event?types=session.archived&types=session.unarchived")
        .await;
    server
        .client
        .patch(format!("{}/api/session/{sid}", server.url))
        .json(&json!({"archived":false}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    assert_eq!(events.next().await["type"], "session.unarchived");
    assert!(
        get(&server, &format!("/api/session/{sid}")).await["data"]["time"]
            .get("archived")
            .is_none()
    );
    assert!(get(&server, "/api/session?archived=true").await["data"]
        .as_array()
        .unwrap()
        .is_empty());
}
