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
