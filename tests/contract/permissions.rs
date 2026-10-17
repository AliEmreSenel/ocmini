use super::*;

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
