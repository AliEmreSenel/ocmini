use super::*;

#[tokio::test]
async fn child_provider_failures_reach_the_parent_tool_and_task_event() {
    let mut provider = Provider::start(vec![
        tool_script(
            "task",
            json!({"description":"Child failure check","prompt":"Child work","subagent_type":"general"}),
            "call_failed_child",
        ),
        Script::http_status_body(400, "CHILD_FAILURE_SENTINEL"),
        text_script("Parent handled the failure", ""),
    ])
    .await;
    let server = TestServer::start(&provider, json!({})).await;
    let sid = server.session(allow()).await;
    let mut events = server.events().await;
    server
        .post(
            &format!("/session/{sid}/message"),
            &prompt("msg_failed_child", "Delegate work"),
        )
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let ended = events.through("ocmini.task.ended").await;
    assert_eq!(ended.last().unwrap()["properties"]["status"], "failed");

    let messages = server.messages(&sid).await;
    let task = messages
        .iter()
        .flat_map(|message| message["parts"].as_array().unwrap())
        .find(|part| part["callID"] == "call_failed_child")
        .unwrap();
    assert_eq!(task["state"]["status"], "error");
    assert!(task["state"]["error"]
        .as_str()
        .unwrap()
        .contains("CHILD_FAILURE_SENTINEL"));
    provider.request().await;
    provider.request().await;
    let continuation = provider.request().await;
    assert!(continuation["messages"]
        .as_array()
        .unwrap()
        .iter()
        .any(|message| message["role"] == "tool"
            && message["content"]
                .as_str()
                .is_some_and(|text| text.contains("CHILD_FAILURE_SENTINEL"))));
}

#[tokio::test]
async fn an_always_approval_can_be_revoked_by_the_session_policy() {
    let command = json!({"command":"printf x >> approval-count"});
    let provider = Provider::start(vec![
        tool_script("bash", command.clone(), "call_allow"),
        text_script("Allowed", ""),
        tool_script("bash", command, "call_revoked"),
        text_script("Denied", ""),
    ])
    .await;
    let server = TestServer::start(&provider, json!({})).await;
    let sid = server
        .session(json!([{"permission":"*","action":"allow"},{"permission":"bash","action":"ask"}]))
        .await;
    let mut events = server.events().await;
    server
        .post(
            &format!("/session/{sid}/prompt_async"),
            &prompt("msg_allow", "Run once"),
        )
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let asked = events.through("permission.asked").await.pop().unwrap();
    let request = asked["properties"]["id"].as_str().unwrap();
    server
        .post(
            &format!("/permission/{request}/reply"),
            &json!({"reply":"always"}),
        )
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    events.through("session.idle").await;
    server.client.patch(format!("{}/session/{sid}", server.url)).json(&json!({"permission":[{"permission":"*","action":"allow"},{"permission":"bash","action":"deny"}]})).send().await.unwrap().error_for_status().unwrap();
    server
        .post(
            &format!("/session/{sid}/message"),
            &prompt("msg_revoked", "Run again"),
        )
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(server.directory.join("approval-count")).unwrap(),
        "x"
    );
    let messages = server.messages(&sid).await;
    let tool = messages
        .iter()
        .flat_map(|message| message["parts"].as_array().unwrap())
        .find(|part| part["callID"] == "call_revoked")
        .unwrap();
    assert_eq!(tool["state"]["status"], "error");
    assert!(tool["state"]["error"].as_str().unwrap().contains("denied"));
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
    assert_eq!(
        server
            .post(
                &format!("/session/{id}/prompt_async"),
                &prompt("msg_tools", "Use tools")
            )
            .send()
            .await
            .unwrap()
            .status(),
        204
    );
    let asked = events.through("permission.asked").await.pop().unwrap();
    assert_eq!(asked["properties"]["tool"]["callID"], "call_bash");
    assert!(asked["properties"]["patterns"].is_array());
    let request_id = asked["properties"]["id"].as_str().unwrap();
    let response = server
        .post(
            &format!("/permission/{request_id}/reply"),
            &json!({"reply": "once"}),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.json::<Value>().await.unwrap(), true);
    let asked = events.through("question.asked").await.pop().unwrap();
    assert!(asked["properties"]["questions"][0].is_object());
    assert_eq!(asked["properties"]["questions"][0]["header"], "Choice");
    assert_eq!(
        asked["properties"]["questions"].as_array().unwrap().len(),
        2
    );
    let request_id = asked["properties"]["id"].as_str().unwrap();
    assert_eq!(
        server
            .post(
                &format!("/question/{request_id}/reply"),
                &json!({"answers": []})
            )
            .send()
            .await
            .unwrap()
            .status(),
        400
    );
    assert_eq!(
        server
            .post(
                &format!("/question/{request_id}/reply"),
                &json!({"answers": [["A", "B"], ["Unit"]]})
            )
            .send()
            .await
            .unwrap()
            .status(),
        400
    );
    assert_eq!(
        server
            .post(
                &format!("/question/{request_id}/reply"),
                &json!({"answers": [["A"], ["Unit", "Integration"]]})
            )
            .send()
            .await
            .unwrap()
            .json::<Value>()
            .await
            .unwrap(),
        true
    );
    events.through("session.idle").await;
    let messages = server.messages(&id).await;
    assert!(messages
        .iter()
        .flat_map(|message| message["parts"].as_array().unwrap())
        .any(|part| part["type"] == "tool"
            && part["callID"] == "call_bash"
            && part["state"]["status"] == "completed"
            && part["state"]["output"]
                .as_str()
                .unwrap_or_default()
                .contains("TOOL_SENTINEL")));
    assert_eq!(
        server
            .get("/permission")
            .send()
            .await
            .unwrap()
            .json::<Value>()
            .await
            .unwrap(),
        json!([])
    );
    assert_eq!(
        server
            .get("/question")
            .send()
            .await
            .unwrap()
            .json::<Value>()
            .await
            .unwrap(),
        json!([])
    );
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
    let parent = server
        .session(json!([
            {"permission": "*", "pattern": "*", "action": "allow"},
            {"permission": "bash", "pattern": "*", "action": "ask"}
        ]))
        .await;
    let mut events = server.events().await;
    assert_eq!(
        server
            .post(
                &format!("/session/{parent}/prompt_async"),
                &prompt("msg_parent_task", "Delegate the check")
            )
            .send()
            .await
            .unwrap()
            .status(),
        204
    );
    let prefix = events.through("permission.asked").await;
    let request = prefix.last().unwrap();
    let child = request["properties"]["sessionID"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_ne!(child, parent);
    assert_eq!(request["properties"]["tool"]["callID"], "call_child_bash");
    let created = prefix
        .iter()
        .position(|event| {
            event["type"] == "session.created" && event["properties"]["info"]["id"] == child
        })
        .unwrap();
    assert_eq!(prefix[created]["properties"]["info"]["parentID"], parent);
    let first_child_message = prefix
        .iter()
        .position(|event| {
            event["type"] == "message.updated" && event["properties"]["sessionID"] == child
        })
        .unwrap();
    assert!(created < first_child_message);
    assert!(prefix
        .iter()
        .any(|event| event["type"] == "message.part.updated"
            && event["properties"]["part"]["callID"] == "call_parent_task"
            && event["properties"]["part"]["state"]["metadata"]["sessionId"] == child
            && event["properties"]["part"]["state"]["metadata"]["model"]["modelID"]
                == "test-model"));
    let request_id = request["properties"]["id"].as_str().unwrap();
    assert_eq!(
        server
            .post(
                &format!("/permission/{request_id}/reply"),
                &json!({"reply": "once"})
            )
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    loop {
        let event = events.next().await;
        if event["type"] == "session.idle" && event["properties"]["sessionID"] == parent {
            break;
        }
    }
    let children: Vec<Value> = server
        .get(&format!("/session/{parent}/children"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(children.len(), 1);
    assert_eq!(children[0]["id"], child);
    assert!(server
        .messages(&child)
        .await
        .iter()
        .any(|message| message.to_string().contains("CHILD_TOOL_SENTINEL")));
    let parent_messages = server.messages(&parent).await;
    let task = parent_messages
        .iter()
        .flat_map(|message| message["parts"].as_array().unwrap())
        .find(|part| part["callID"] == "call_parent_task")
        .unwrap();
    assert_eq!(task["state"]["status"], "completed");
    assert_eq!(task["state"]["metadata"]["sessionId"], child);
    assert!(task["state"]["output"]
        .as_str()
        .unwrap()
        .contains("CHILD_RESULT_SENTINEL"));
    let mut requests = Vec::new();
    for _ in 0..4 {
        requests.push(provider.request().await);
    }
    assert!(requests
        .last()
        .unwrap()
        .to_string()
        .contains("CHILD_RESULT_SENTINEL"));
}

#[tokio::test]
async fn skill_aliases_authorize_the_name_that_will_actually_load() {
    let provider = Provider::start(vec![
        tool_script("skill", json!({"id":"restricted"}), "aliased-skill"),
        tool_script(
            "skill",
            json!({"name":"permitted","id":"restricted"}),
            "canonical-skill",
        ),
        text_script("Done", ""),
    ])
    .await;
    let server = TestServer::start(&provider, json!({})).await;
    for (name, content) in [
        ("restricted", "PRIVATE_SKILL_CONTENT"),
        ("permitted", "PUBLIC_SKILL_CONTENT"),
    ] {
        let dir = server.directory.join(".opencode/skills").join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: fixture\n---\n{content}\n"),
        )
        .unwrap();
    }
    let sid = server
        .session(json!([
            {"permission":"*","pattern":"*","action":"allow"},
            {"permission":"skill","pattern":"restricted","action":"deny"}
        ]))
        .await;
    server
        .post(
            &format!("/session/{sid}/message"),
            &prompt("msg_skill_alias", "Load skills"),
        )
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let messages = server.messages(&sid).await;
    let tools: Vec<_> = messages
        .iter()
        .flat_map(|message| message["parts"].as_array().unwrap())
        .filter(|part| part["type"] == "tool")
        .collect();
    assert_eq!(tools.len(), 2);
    assert_eq!(tools[0]["state"]["status"], "error");
    assert!(tools[0]["state"]["error"]
        .as_str()
        .unwrap()
        .contains("denied for restricted"));
    assert_eq!(tools[0]["state"]["input"], json!({"id":"restricted"}));
    assert_eq!(tools[1]["state"]["status"], "completed", "{}", tools[1]);
    assert!(tools[1]["state"]["output"]
        .as_str()
        .unwrap()
        .contains("PUBLIC_SKILL_CONTENT"));
    assert!(!json!(messages)
        .to_string()
        .contains("PRIVATE_SKILL_CONTENT"));
}

#[derive(Clone, Copy)]
enum ExecutionGate {
    Approval,
    ShellHook,
    ChildCreation,
}

async fn revoked_owner_cannot_execute(gate: ExecutionGate) {
    let (name, input, hook) = match gate {
        ExecutionGate::Approval => (
            "bash",
            json!({"command":"printf STALE_OWNER > stale-owner-effect"}),
            None,
        ),
        ExecutionGate::ShellHook => (
            "bash",
            json!({"command":"printf STALE_OWNER > stale-owner-effect"}),
            Some("shell.env"),
        ),
        ExecutionGate::ChildCreation => (
            "task",
            json!({"description":"Child task","prompt":"Work","subagent_type":"general"}),
            Some("session.create.before"),
        ),
    };
    let provider = Provider::start(vec![tool_script(name, input, "stale-owner-call")]).await;
    let config = hook.map(|on| json!({"ocmini":{"hooks":{"handlers":[{"id":"execution-gate","on":on,"command":["python3","-c",
        "import json,sys,pathlib,time\nr=json.load(sys.stdin)\nif r['context'].get('tool') == 'bash' or r['context'].get('parentID'):\n pathlib.Path('dispatch-entered').touch()\n while not pathlib.Path('release-dispatch').exists(): time.sleep(.01)"
    ]}]}}})).unwrap_or_else(|| json!({}));
    let server = TestServer::start(&provider, config).await;
    let rules = if hook.is_some() {
        allow()
    } else {
        json!([
            {"permission":"*","pattern":"*","action":"allow"},
            {"permission":"bash","pattern":"*","action":"ask"}
        ])
    };
    let sid = server.session(rules).await;
    let mut events = server.events().await;
    server
        .post(
            &format!("/session/{sid}/prompt_async"),
            &prompt("msg_stale_approval", "Run a tool"),
        )
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let request_id = if hook.is_some() {
        tokio::time::timeout(Duration::from_secs(4), async {
            while !server.directory.join("dispatch-entered").exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("tool did not reach its hook boundary");
        None
    } else {
        let asked = events.through("permission.asked").await.pop().unwrap();
        Some(asked["properties"]["id"].as_str().unwrap().to_owned())
    };
    // Replace the durable owner while the first process is awaiting input.
    let conn = rusqlite::Connection::open(server.root.path().join(".local/share/ocmini/ocmini.db"))
        .unwrap();
    assert_eq!(conn.execute("UPDATE owner SET instance_id='replacement',lease_token='replacement' WHERE session_id=?1", [&sid]).unwrap(), 1);
    drop(conn);
    if let Some(request_id) = request_id {
        server
            .post(
                &format!("/permission/{request_id}/reply"),
                &json!({"reply":"once"}),
            )
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
    } else {
        std::fs::write(server.directory.join("release-dispatch"), "").unwrap();
    }
    let settled = events.through("session.idle").await;
    assert!(
        !server.directory.join("stale-owner-effect").exists(),
        "a superseded owner executed the command"
    );
    if matches!(gate, ExecutionGate::Approval) {
        assert!(
            !settled
                .iter()
                .any(|event| event["type"] == "ocmini.tool.started"),
            "a superseded owner announced tool dispatch"
        );
    }
    let children = server
        .get(&format!("/session/{sid}/children"))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(
        children,
        json!([]),
        "a superseded owner created a child session"
    );
}

#[tokio::test]
async fn ownership_lost_during_approval_prevents_tool_dispatch() {
    revoked_owner_cannot_execute(ExecutionGate::Approval).await;
}

#[tokio::test]
async fn ownership_lost_during_shell_hook_prevents_process_start() {
    revoked_owner_cannot_execute(ExecutionGate::ShellHook).await;
}

#[tokio::test]
async fn ownership_lost_during_child_creation_prevents_session_insert() {
    revoked_owner_cannot_execute(ExecutionGate::ChildCreation).await;
}
