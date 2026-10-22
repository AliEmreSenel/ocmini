use super::*;

#[tokio::test]
async fn truncated_provider_streams_fail_instead_of_reporting_success() {
    let script = Script::truncated();
    let provider = Provider::start(vec![script]).await;
    let server = TestServer::start(&provider, json!({})).await;
    let id = server.session(allow()).await;
    let mut events = server.events().await;
    let response = server
        .post(
            &format!("/session/{id}/message"),
            &prompt("msg_truncated", "hello"),
        )
        .send()
        .await
        .unwrap();
    assert!(!response.status().is_success());
    let terminal = events.through("session.idle").await;
    assert!(terminal
        .iter()
        .any(|event| event["type"] == "session.error"));
    assert!(server
        .messages(&id)
        .await
        .iter()
        .any(|message| message["info"]["role"] == "assistant"
            && message["info"]["error"].is_object()));
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
    ])
    .await;
    let server = TestServer::start(&provider, json!({})).await;
    let id = server.session(allow()).await;
    let mut events = server.events().await;
    let response = server
        .post(
            &format!("/session/{id}/message"),
            &prompt("msg_retry_flaky", "hello"),
        )
        .send()
        .await
        .unwrap();
    assert!(
        response.status().is_success(),
        "flaky provider turn must complete: {:?}",
        response.text().await.unwrap()
    );
    let terminal = events.through("session.idle").await;
    let retries = terminal
        .iter()
        .filter(|event| {
            event["type"] == "session.status" && event["properties"]["status"]["type"] == "retry"
        })
        .count();
    assert!(
        retries >= 3,
        "retry status events must be visible, saw {retries}"
    );
    let messages = server.messages(&id).await;
    let assistant = messages
        .iter()
        .rev()
        .find(|m| m["info"]["role"] == "assistant")
        .unwrap();
    let text: String = assistant["parts"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|p| p["type"] == "text")
        .filter_map(|p| p["text"].as_str())
        .collect();
    assert_eq!(text, "Recovery works");
}

#[tokio::test]
async fn retry_exhaustion_reports_a_visible_failure() {
    let provider = Provider::start(vec![
        Script::http_status(500, Some("0")),
        Script::http_status(503, Some("0")),
        Script::http_status(429, Some("0")),
        Script::http_status(502, Some("0")),
        Script::http_status(500, Some("0")),
        Script::http_status(503, Some("0")),
    ])
    .await;
    let server = TestServer::start(&provider, json!({})).await;
    let id = server.session(allow()).await;
    let mut events = server.events().await;
    let response = server
        .post(
            &format!("/session/{id}/message"),
            &prompt("msg_retry_dead", "hello"),
        )
        .send()
        .await
        .unwrap();
    assert!(!response.status().is_success());
    let terminal = events.through("session.idle").await;
    assert!(terminal
        .iter()
        .any(|event| event["type"] == "session.error"));
    let retries = terminal
        .iter()
        .filter(|event| {
            event["type"] == "session.status" && event["properties"]["status"]["type"] == "retry"
        })
        .count();
    assert_eq!(retries, 5, "exactly five bounded retries per step");
}

#[tokio::test]
async fn chat_requests_merge_reasoning_of_every_step_into_one_assistant_message() {
    // OpenCode's Chat Completions converter replays chain of thought as
    // reasoning_content on the assistant message. ocmini merges a turn's
    // steps into that single message: one assistant message per turn, the
    // steps' reasoning concatenated in order, tool calls and results kept.
    let step = |thinking: &str, id: &str| {
        Script::ok(vec![
            json!({"choices": [{"index": 0, "delta": {"role": "assistant", "reasoning_content": thinking}, "finish_reason": null}]}),
            json!({"choices": [{"index": 0, "delta": {"tool_calls": [{"index": 0, "id": id, "type": "function", "function": {"name": "read", "arguments": "{\"filePath\":\"note.txt\"}"}}]}, "finish_reason": null}]}),
            json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}], "usage": usage()}),
        ])
    };
    let mut provider = Provider::start(vec![
        step("first thinking", "call_one"),
        step("second thinking", "call_two"),
        text_script("done", ""),
    ])
    .await;
    let server = TestServer::start(&provider, json!({})).await;
    std::fs::write(server.directory.join("note.txt"), "the note\n").unwrap();
    let id = server.session(allow()).await;
    server
        .post(
            &format!("/session/{id}/message"),
            &prompt("msg_reasoning", "read note.txt twice"),
        )
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    provider.request().await; // step 1: only the user message is sent
    let second = provider.request().await; // step 2: step 1 is replayed
    let second: Vec<Value> = second["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|message| message["role"] != "system")
        .cloned()
        .collect();
    assert_eq!(
        second.iter().filter(|m| m["role"] == "assistant").count(),
        1
    );
    assert_eq!(second[1]["reasoning_content"], "first thinking");
    assert_eq!(second[1]["tool_calls"][0]["id"], "call_one");
    assert_eq!(second[2]["role"], "tool");
    let third = provider.request().await; // step 3: the whole turn is replayed
    let third: Vec<Value> = third["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|message| message["role"] != "system")
        .cloned()
        .collect();
    assert_eq!(
        third.iter().filter(|m| m["role"] == "assistant").count(),
        1,
        "both steps merge into a single assistant message"
    );
    assert_eq!(
        third[1]["reasoning_content"],
        "first thinking\nsecond thinking"
    );
    assert_eq!(third[1]["tool_calls"][0]["id"], "call_one");
    assert_eq!(third[1]["tool_calls"][1]["id"], "call_two");
    assert_eq!(third[2]["role"], "tool");
    assert_eq!(third[2]["tool_call_id"], "call_one");
    assert_eq!(third[3]["role"], "tool");
    assert_eq!(third[3]["tool_call_id"], "call_two");
}

#[tokio::test]
async fn configured_reasoning_budgets_match_the_advertised_variant_on_the_wire() {
    let script = || {
        Script::ok(vec![
            json!({"type":"message_start","message":{"id":"native","usage":{"input_tokens":1}}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":"done"}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":1}}),
            json!({"type":"message_stop"}),
        ])
    };
    let mut provider = Provider::start(vec![script(), script()]).await;
    let server = TestServer::start(
        &provider,
        json!({
            "provider":{"mock":{"npm":"@ai-sdk/anthropic", "options":{"baseURL":provider.url},
                "models":{"test-model":{"limit":{"context":100000,"output":20000},
                    "reasoning_options":[{"type":"budget_tokens","min":1024,"max":16000}]}}}}
        }),
    )
    .await;
    let inventory: Value = server
        .get("/provider")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let budget = inventory["all"][0]["models"]["test-model"]["variants"]["max"]["thinking"]
        ["budgetTokens"]
        .clone();
    assert_eq!(budget, 15999);
    let sid = server.session(allow()).await;
    let mut body = prompt("reasoning_budget", "hello");
    body["variant"] = json!("max");
    server
        .post(&format!("/session/{sid}/message"), &body)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let request = provider.request().await;
    assert_eq!(request["thinking"]["budget_tokens"], budget);
    assert_eq!(request["max_tokens"], 20000);
    let output = configured_command(server.root.path(), &server.directory)
        .args(["run", "--variant", "max", "hello"])
        .stdin(Stdio::null())
        .output()
        .await
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        provider.request().await["thinking"]["budget_tokens"],
        budget
    );
}

#[tokio::test]
async fn disabled_generated_variants_stay_disabled_during_execution() {
    let provider = Provider::start(vec![]).await;
    let server = TestServer::start(&provider, json!({
        "provider":{"mock":{"npm":"@ai-sdk/openai-compatible", "options":{"baseURL":provider.url},
            "models":{"test-model":{"reasoning_options":[{"type":"effort","values":["high","low"]}],
                "variants":{"high":{"disabled":true}}}}}}
    })).await;
    let inventory: Value = server
        .get("/provider")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(inventory["all"][0]["models"]["test-model"]["variants"]
        .get("high")
        .is_none());
    let sid = server.session(allow()).await;
    let mut body = prompt("disabled_preset", "hello");
    body["variant"] = json!("high");
    let response = server
        .post(&format!("/session/{sid}/message"), &body)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 400);
    assert!(response.text().await.unwrap().contains("disabled"));
}
