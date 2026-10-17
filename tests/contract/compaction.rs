use super::*;

#[tokio::test]
async fn compaction_calls_the_model_and_produces_a_real_summary() {
    let mut provider = Provider::start(vec![
        text_script("Prior answer", ""),
        text_script(
            "SUMMARY_SENTINEL: user requested tests and the previous answer was recorded.",
            "",
        ),
        text_script("Continued from summary", ""),
    ])
    .await;
    let server = TestServer::start(&provider, json!({})).await;
    let id = server.session(allow()).await;
    assert_eq!(
        server
            .post(
                &format!("/session/{id}/message"),
                &prompt("msg_before_compact", "Please inspect tests")
            )
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    provider.request().await;
    let mut events = server.events().await;
    let response = server
        .post(
            &format!("/session/{id}/summarize"),
            &json!({"providerID": "mock", "modelID": "test-model", "auto": false}),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.json::<Value>().await.unwrap(), true);
    let request = provider.request().await;
    assert!(request.to_string().contains("Prior answer"));
    events.through("session.compacted").await;
    let messages = server.messages(&id).await;
    assert!(messages
        .iter()
        .any(|message| message.to_string().contains("SUMMARY_SENTINEL")));
    assert!(messages
        .iter()
        .flat_map(|message| message["parts"].as_array().unwrap())
        .any(|part| part["type"] == "compaction"));
    assert_eq!(
        server
            .post(
                &format!("/session/{id}/message"),
                &prompt("msg_after_compact", "Continue")
            )
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    let next = provider.request().await;
    assert!(next.to_string().contains("SUMMARY_SENTINEL"));
    assert!(
        !next.to_string().contains("Prior answer"),
        "compaction replayed the old conversation unchanged"
    );
    assert!(
        !next.to_string().contains("Please inspect tests"),
        "compaction did not replace the old user context"
    );
}

#[tokio::test]
async fn context_overflow_400_compacts_history_and_replays_the_step() {
    // A 262k-context model rejecting the prompt must not end the turn:
    // ocmini compacts, then replays the step against the summary.
    let mut provider = Provider::start(vec![
        Script::http_status_body(400, "This model's maximum context length is 262144 tokens. However, you requested 0 output tokens and your prompt contains at least 262145 input tokens, for a total of at least 262145 tokens. Please reduce the length of the input prompt or the number of requested output tokens. (parameter=input_tokens, value=262145)"),
        text_script("Compacted. ", "Carrying on"),
        text_script("Done ", "after overflow"),
    ]).await;
    let server = TestServer::start(&provider, json!({})).await;
    let id = server.session(allow()).await;
    let mut events = server.events().await;
    let response = server
        .post(
            &format!("/session/{id}/message"),
            &prompt("msg_overflow", "hello"),
        )
        .send()
        .await
        .unwrap();
    assert!(
        response.status().is_success(),
        "overflow must recover, not fail: {:?}",
        response.text().await.unwrap()
    );
    let terminal = events.through("session.idle").await;
    assert!(
        terminal
            .iter()
            .any(|event| event["type"] == "session.compacted"
                && event["properties"]["sessionID"] == id),
        "compaction must be announced"
    );
    let original = provider.request().await;
    assert!(original["messages"]
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m["content"].as_str().is_some_and(|c| c.contains("hello"))));
    let compaction = provider.request().await;
    assert_eq!(
        compaction["messages"].as_array().unwrap()[0]["role"],
        "system"
    );
    assert!(
        compaction["messages"][0]["content"]
            .as_str()
            .unwrap()
            .contains("Summarize the conversation"),
        "second request is the summarizer"
    );
    assert!(
        compaction["tools"]
            .as_array()
            .is_none_or(|tools| tools.is_empty()),
        "summarizer requests carry no tools"
    );
    let replay = provider.request().await;
    let carried = replay["messages"].as_array().unwrap().iter().any(|m| {
        m["content"].as_str().is_some_and(|c| {
            c.contains("Continuation summary") && c.contains("Compacted. Carrying on")
        })
    });
    assert!(
        carried,
        "replayed request must carry the summary, not the overflowing transcript"
    );
    let messages = server.messages(&id).await;
    let assistant = messages
        .iter()
        .rev()
        .find(|m| {
            m["info"]["role"] == "assistant"
                && m["info"]["summary"].is_null()
                && m["info"]["error"].is_null()
        })
        .expect("completed assistant answer");
    let text: String = assistant["parts"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|p| p["type"] == "text")
        .filter_map(|p| p["text"].as_str())
        .collect();
    assert_eq!(text, "Done after overflow");
}

#[tokio::test]
async fn usage_near_the_context_limit_compacts_proactively() {
    // Configured window is 32000; a step reporting 31100 tokens compacts
    // before the next request, so the following turn starts from the summary.
    let mut provider = Provider::start(vec![
        Script::ok(vec![
            json!({"choices": [{"index": 0, "delta": {"content": "working"}, "finish_reason": null}]}),
            json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}], "usage": {"prompt_tokens": 31000, "completion_tokens": 100}}),
        ]),
        text_script("Summary ", "text"),
        text_script("Final ", "answer"),
    ]).await;
    let server = TestServer::start(&provider, json!({})).await;
    let id = server.session(allow()).await;
    let mut events = server.events().await;
    let response = server
        .post(
            &format!("/session/{id}/message"),
            &prompt("msg_near_limit", "first"),
        )
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success());
    let terminal = events.through("session.idle").await;
    assert!(
        terminal
            .iter()
            .any(|event| event["type"] == "session.compacted"),
        "steps near the window must compact proactively"
    );
    provider.request().await;
    let compaction = provider.request().await;
    assert!(compaction["messages"][0]["content"]
        .as_str()
        .unwrap()
        .contains("Summarize the conversation"));
    assert_eq!(
        provider.requests.try_recv().err().map(|_| ()),
        Some(()),
        "compaction ends the turn's requests"
    );
    let response = server
        .post(
            &format!("/session/{id}/message"),
            &prompt("msg_after_compact", "again"),
        )
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success());
    events.through("session.idle").await;
    let follow_up = provider.request().await;
    assert!(
        follow_up["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["content"]
                .as_str()
                .is_some_and(|c| c.contains("Continuation summary") && c.contains("Summary text"))),
        "next turn starts from the summary"
    );
}
