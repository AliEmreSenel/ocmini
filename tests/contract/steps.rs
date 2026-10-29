use super::*;

fn progress_script(step: usize) -> Script {
    tool_script(
        "todowrite",
        json!({"todos": [{"content": format!("Step {step}"), "status": "in_progress", "priority": "medium"}]}),
        &format!("call_{step}"),
    )
}

#[tokio::test]
async fn unconfigured_agent_continues_past_100_steps_to_its_final_answer() {
    let mut scripts: Vec<_> = (0..101).map(progress_script).collect();
    scripts.push(text_script("Finished the long task", ""));
    let mut provider = Provider::start(scripts).await;
    let server = TestServer::start(&provider, json!({})).await;
    let id = server.session(allow()).await;
    let mut events = server.events().await;
    server
        .post(
            &format!("/session/{id}/prompt_async"),
            &prompt("msg_long_task", "Complete all the steps"),
        )
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let terminal = events.through("session.idle").await;
    assert!(
        !terminal
            .iter()
            .any(|event| event["type"] == "session.error"),
        "a progressing agent must not hit an implicit step limit"
    );
    let messages = server.messages(&id).await;
    assert_eq!(
        messages
            .iter()
            .flat_map(|message| message["parts"].as_array().unwrap())
            .filter(|part| part["type"] == "tool" && part["state"]["status"] == "completed")
            .count(),
        101
    );
    assert!(messages.last().unwrap()["parts"]
        .as_array()
        .unwrap()
        .iter()
        .any(|part| part["text"] == "Finished the long task"));
    let requests: Vec<_> = std::iter::from_fn(|| provider.requests.try_recv().ok()).collect();
    assert_eq!(requests.len(), 102);
    assert!(requests.last().unwrap()["messages"]
        .as_array()
        .unwrap()
        .iter()
        .any(|message| message["role"] == "tool" && message["tool_call_id"] == "call_100"));
}

#[tokio::test]
async fn explicit_agent_step_limits_stop_and_preserve_results_for_continuation() {
    for field in ["steps", "maxSteps"] {
        let mut provider = Provider::start(vec![
            progress_script(0),
            progress_script(1),
            text_script("Continued from saved tools", ""),
        ])
        .await;
        let server = TestServer::start(&provider, json!({"agent": {"build": {(field): 2}}})).await;
        let id = server.session(allow()).await;
        let mut events = server.events().await;
        server
            .post(
                &format!("/session/{id}/prompt_async"),
                &prompt("msg_limited", "Start"),
            )
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
        let terminal = events.through("session.idle").await;
        assert!(terminal.iter().any(|event| event["type"] == "session.error"
            && event["properties"]["error"]["data"]["message"]
                .as_str()
                .is_some_and(|message| message.contains("Agent step limit (2) reached"))));
        provider.request().await;
        provider.request().await;
        assert!(provider.requests.try_recv().is_err());
        server
            .post(
                &format!("/session/{id}/message"),
                &prompt("msg_continue", "Continue"),
            )
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
        let next = provider.request().await;
        let tool_results: Vec<_> = next["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|message| message["role"] == "tool")
            .collect();
        assert_eq!(tool_results.len(), 2);
        assert_eq!(tool_results[1]["tool_call_id"], "call_1");
    }
}

#[tokio::test]
async fn final_answer_at_the_explicit_step_limit_completes_normally() {
    let provider = Provider::start(vec![progress_script(0), text_script("Done", "")]).await;
    let server = TestServer::start(&provider, json!({"agent": {"build": {"steps": 2}}})).await;
    let id = server.session(allow()).await;
    let mut events = server.events().await;
    let reply: Value = server
        .post(
            &format!("/session/{id}/message"),
            &prompt("msg_at_limit", "Finish in two steps"),
        )
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(reply["info"]["error"].is_null());
    assert!(reply["parts"]
        .as_array()
        .unwrap()
        .iter()
        .any(|part| part["text"] == "Done"));
    assert!(!events
        .through("session.idle")
        .await
        .iter()
        .any(|event| event["type"] == "session.error"));
}

#[tokio::test]
async fn tool_calls_in_one_step_execute_concurrently() {
    // Two shell calls gate on each other's marker file. Sequential execution
    // times the first call out; only concurrent execution opens both gates.
    let provider = Provider::start(vec![
        Script::ok(vec![
            json!({"choices": [{"index": 0, "delta": {"tool_calls": [
                {"index": 0, "id": "call_gate_a", "type": "function", "function": {"name": "bash", "arguments": json!({"command": "touch gate-a; for i in $(seq 1 200); do [ -f gate-b ] && { echo GATE_A_OPEN; exit 0; }; sleep 0.05; done; echo GATE_A_TIMEOUT", "timeout": 30000}).to_string()}},
                {"index": 1, "id": "call_gate_b", "type": "function", "function": {"name": "bash", "arguments": json!({"command": "touch gate-b; for i in $(seq 1 200); do [ -f gate-a ] && { echo GATE_B_OPEN; exit 0; }; sleep 0.05; done; echo GATE_B_TIMEOUT", "timeout": 30000}).to_string()}}
            ]}, "finish_reason": null}]}),
            json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}], "usage": usage()}),
        ]),
        text_script("Both gates opened", ""),
    ])
    .await;
    let server = TestServer::start(&provider, json!({})).await;
    let id = server.session(allow()).await;
    let mut events = server.events().await;
    server
        .post(
            &format!("/session/{id}/prompt_async"),
            &prompt("msg_parallel_tools", "Open both gates"),
        )
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    events.through("session.idle").await;
    let messages = server.messages(&id).await;
    let outputs: Vec<String> = ["call_gate_a", "call_gate_b"]
        .iter()
        .map(|call| {
            messages
                .iter()
                .flat_map(|message| message["parts"].as_array().unwrap())
                .find(|part| part["callID"] == *call)
                .unwrap()["state"]["output"]
                .as_str()
                .unwrap_or_default()
                .to_string()
        })
        .collect();
    assert!(
        outputs[0].contains("GATE_A_OPEN") && outputs[1].contains("GATE_B_OPEN"),
        "tool calls from one model step must run concurrently, got {outputs:?}"
    );
}

async fn poll_messages(
    server: &TestServer,
    id: &str,
    ready: impl Fn(&[Value]) -> bool,
) -> Vec<Value> {
    for _ in 0..200 {
        let messages = server.messages(id).await;
        if ready(&messages) {
            return messages;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    server.messages(id).await
}

#[tokio::test]
async fn background_task_returns_immediately_and_delivers_its_result_to_the_parent() {
    // The parent consumes: launch, continuation, delivery turn. The child
    // consumes one completion turn. All answer scripts share one text, so the
    // test is deterministic under any interleaving: the delivery is answered
    // either by a fresh turn (idle parent) or by the steered live turn.
    let mut provider = Provider::start(vec![
        tool_script("task", json!({"description":"Background work","prompt":"Do the work","subagent_type":"general","background":true}), "call_bg"),
        text_script("OK", ""),
        text_script("OK", ""),
        text_script("OK", ""),
    ])
    .await;
    let server = TestServer::start(&provider, json!({})).await;
    let id = server.session(allow()).await;
    let mut events = server.events().await;
    server
        .post(
            &format!("/session/{id}/prompt_async"),
            &prompt("msg_bg", "Launch a background task"),
        )
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let ended = events.through("ocmini.task.ended").await;
    assert!(
        ended
            .iter()
            .any(|event| event["properties"]["status"] == "completed"
                && event["properties"]["parentSessionID"] == id),
        "the parent must see its background task complete"
    );
    let messages = poll_messages(&server, &id, |messages| {
        messages
            .iter()
            .position(|message| {
                message["info"]["role"] == "user"
                    && message["parts"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|part| part["synthetic"] == true)
            })
            .is_some_and(|index| {
                messages.get(index + 1).is_some_and(|answer| {
                    answer["info"]["role"] == "assistant"
                        && answer["info"]["time"]["completed"].is_number()
                        && answer["parts"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .any(|part| part["text"] == "OK")
                })
            })
    })
    .await;
    let tool = messages
        .iter()
        .flat_map(|message| message["parts"].as_array().unwrap().clone())
        .find(|part| part["callID"] == "call_bg")
        .expect("task tool part");
    assert_eq!(tool["state"]["status"], "completed");
    assert_eq!(tool["state"]["metadata"]["background"], true);
    let output = tool["state"]["output"].as_str().unwrap();
    assert!(
        output.starts_with("Task session: ") && output.contains("DO NOT sleep"),
        "{output}"
    );
    let child_id = tool["state"]["metadata"]["sessionId"]
        .as_str()
        .unwrap()
        .to_owned();
    let delivery = messages
        .iter()
        .find(|message| {
            message["info"]["role"] == "user"
                && message["parts"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|part| part["synthetic"] == true)
        })
        .expect("synthetic delivery message in parent history");
    let text = delivery["parts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|part| part["synthetic"] == true)
        .unwrap()["text"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(
        text.starts_with(&format!("<task id=\"{child_id}\" state=\"completed\">")),
        "{text}"
    );
    assert!(
        text.contains("<summary>Background task completed: Background work</summary>"),
        "{text}"
    );
    assert!(text.contains("<task_result>\nOK\n</task_result>"), "{text}");
    let requests: Vec<_> = std::iter::from_fn(|| provider.requests.try_recv().ok()).collect();
    assert!(
        (3..=4).contains(&requests.len()),
        "parent launch/continuation, child answer and (when the parent was idle) a delivery turn, got {}",
        requests.len()
    );
}

#[tokio::test]
async fn cancelling_the_child_silences_its_background_delivery() {
    let (release, gate) = oneshot::channel();
    let mut stalled = text_script("Too late", "");
    stalled.gate = Some(gate);
    let provider = Provider::start(vec![
        tool_script("task", json!({"description":"Slow work","prompt":"Take your time","subagent_type":"general","background":true}), "call_bg2"),
        text_script("Launched", ""),
        stalled,
        text_script("Noted", ""),
    ])
    .await;
    let server = TestServer::start(&provider, json!({})).await;
    let id = server.session(allow()).await;
    let mut events = server.events().await;
    server
        .post(
            &format!("/session/{id}/prompt_async"),
            &prompt("msg_bg2", "Launch a slow background task"),
        )
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    // The parent's first turn settles while the child's response sits on its gate.
    // If the child claimed the parent's continuation script instead, releasing
    // the gate lets the parent's steered delivery turn finish.
    let mut release = Some(release);
    let mut released = false;
    loop {
        let batch = events.through("session.idle").await;
        let parent_idle = batch.last().is_some_and(|event| {
            event["type"] == "session.idle" && event["properties"]["sessionID"] == id
        });
        if parent_idle {
            break;
        }
        if !released {
            drop(release.take());
            released = true;
        }
    }
    let mut child_id = None;
    for _ in 0..200 {
        let messages = server.messages(&id).await;
        child_id = messages
            .iter()
            .flat_map(|message| message["parts"].as_array().unwrap().clone())
            .find(|part| part["callID"] == "call_bg2")
            .and_then(|part| {
                part["state"]["metadata"]["sessionId"]
                    .as_str()
                    .map(str::to_owned)
            });
        if child_id.is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    let child_id = child_id.expect("background child session");
    let users_before = server
        .messages(&id)
        .await
        .iter()
        .filter(|message| message["info"]["role"] == "user")
        .count();
    server
        .post(&format!("/session/{child_id}/abort"), &json!({}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    if !released {
        drop(release.take());
    }
    tokio::time::sleep(std::time::Duration::from_millis(750)).await;
    let users_after = server
        .messages(&id)
        .await
        .iter()
        .filter(|message| message["info"]["role"] == "user")
        .count();
    assert_eq!(
        users_after, users_before,
        "a cancelled child must not wake the parent with a delivery"
    );
}
