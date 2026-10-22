use super::*;

#[tokio::test]
async fn one_tool_turn_can_compact_proactively_repeatedly() {
    let mut scripts = Vec::new();
    for step in 0..4 {
        let mut script = tool_script(
            "todowrite",
            json!({"todos": [{"content": format!("Progress {step}"), "status": "in_progress", "priority": "medium"}]}),
            &format!("call_{step}"),
        );
        script.chunks.last_mut().unwrap()["usage"] =
            json!({"prompt_tokens": 31000, "completion_tokens": 100});
        scripts.push(script);
        scripts.push(text_script(&format!("CHECKPOINT_{step}"), ""));
    }
    scripts.push(text_script("Finished repeated compactions", ""));
    let mut provider = Provider::start(scripts).await;
    let server = TestServer::start(&provider, json!({})).await;
    let id = server.session(allow()).await;
    let mut events = server.events().await;
    server
        .post(
            &format!("/session/{id}/prompt_async"),
            &prompt("msg_repeated", "ORIGINAL_GOAL"),
        )
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let terminal = events.through("session.idle").await;
    assert!(!terminal
        .iter()
        .any(|event| event["type"] == "session.error"));
    assert_eq!(
        terminal
            .iter()
            .filter(|event| event["type"] == "session.compacted")
            .count(),
        4
    );
    let requests: Vec<_> = std::iter::from_fn(|| provider.requests.try_recv().ok()).collect();
    assert_eq!(requests.len(), 9);
    let final_request = requests.last().unwrap().to_string();
    assert!(final_request.contains("CHECKPOINT_3"));
    assert!(!final_request.contains("CHECKPOINT_0"));
    assert!(final_request.contains("ORIGINAL_GOAL"));
    let messages = server.messages(&id).await;
    assert!(messages.last().unwrap()["parts"]
        .as_array()
        .unwrap()
        .iter()
        .any(|part| part["text"] == "Finished repeated compactions"));
}

#[tokio::test]
async fn independent_context_overflows_recover_throughout_one_tool_turn() {
    let mut scripts = Vec::new();
    for step in 0..4 {
        scripts.push(Script::http_status_body(
            400,
            "maximum context length is 32000 tokens",
        ));
        scripts.push(text_script(&format!("RECOVERED_{step}"), ""));
        scripts.push(tool_script(
            "todowrite",
            json!({"todos": [{"content": format!("Progress {step}"), "status": "in_progress", "priority": "medium"}]}),
            &format!("call_{step}"),
        ));
    }
    scripts.push(text_script("Finished overflow recoveries", ""));
    let mut provider = Provider::start(scripts).await;
    let server = TestServer::start(&provider, json!({})).await;
    let id = server.session(allow()).await;
    let mut events = server.events().await;
    server
        .post(
            &format!("/session/{id}/prompt_async"),
            &prompt("msg_overflows", "ORIGINAL_GOAL"),
        )
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let terminal = events.through("session.idle").await;
    assert!(!terminal
        .iter()
        .any(|event| event["type"] == "session.error"));
    assert_eq!(
        terminal
            .iter()
            .filter(|event| event["type"] == "session.compacted")
            .count(),
        4
    );
    let requests: Vec<_> = std::iter::from_fn(|| provider.requests.try_recv().ok()).collect();
    assert_eq!(requests.len(), 13);
    let final_request = requests.last().unwrap();
    let tool_results: Vec<_> = final_request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|message| message["role"] == "tool")
        .collect();
    assert_eq!(
        tool_results.len(),
        4,
        "recovered responses must survive later compactions exactly once"
    );
    assert!(final_request.to_string().contains("ORIGINAL_GOAL"));
    assert!(server.messages(&id).await.last().unwrap()["parts"]
        .as_array()
        .unwrap()
        .iter()
        .any(|part| part["text"] == "Finished overflow recoveries"));
}

#[tokio::test]
async fn irreducible_context_overflow_stops_retrying_the_same_step() {
    let mut provider = Provider::start(vec![
        Script::http_status_body(400, "maximum context length is 32000 tokens"),
        text_script("First recovery", ""),
        Script::http_status_body(400, "maximum context length is 32000 tokens"),
    ])
    .await;
    let server = TestServer::start(&provider, json!({})).await;
    let id = server.session(allow()).await;
    let mut events = server.events().await;
    server
        .post(
            &format!("/session/{id}/prompt_async"),
            &prompt("msg_irreducible", "Too large"),
        )
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let terminal = events.through("session.idle").await;
    assert!(terminal
        .iter()
        .any(|event| event["type"] == "session.error"));
    assert_eq!(
        terminal
            .iter()
            .filter(|event| event["type"] == "session.compacted")
            .count(),
        1
    );
    assert_eq!(
        std::iter::from_fn(|| provider.requests.try_recv().ok()).count(),
        3
    );
}

#[tokio::test]
async fn repeated_compactions_use_the_latest_summary_and_preserve_recent_history_verbatim() {
    let mut provider = Provider::start(vec![
        text_script("ORIGINAL_REPLY", ""),
        text_script("FIRST_SUMMARY", ""),
        text_script("SECOND_REPLY", ""),
        text_script("LATEST_SUMMARY", ""),
        text_script("Final reply", ""),
    ])
    .await;
    let server = TestServer::start(&provider, json!({})).await;
    let id = server.session(allow()).await;
    let mut events = server.events().await;
    for (index, text) in [(1, "ORIGINAL_PROMPT"), (2, "SECOND_PROMPT")] {
        server
            .post(
                &format!("/session/{id}/message"),
                &prompt(&format!("msg_{index}"), text),
            )
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
        events.through("session.idle").await;
        provider.request().await;
        server
            .post(
                &format!("/session/{id}/summarize"),
                &json!({"providerID":"mock","modelID":"test-model"}),
            )
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
        events.through("session.idle").await;
        provider.request().await;
    }
    server
        .post(
            &format!("/session/{id}/message"),
            &prompt("msg_final", "Continue"),
        )
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let next = provider.request().await;
    let sent = next.to_string();
    assert!(sent.contains("LATEST_SUMMARY"));
    assert!(!sent.contains("FIRST_SUMMARY"));
    for recent in [
        "ORIGINAL_PROMPT",
        "ORIGINAL_REPLY",
        "SECOND_PROMPT",
        "SECOND_REPLY",
    ] {
        assert!(
            sent.matches(recent).count() == 1,
            "recent context must be replayed exactly once: {recent}"
        );
    }
    let saved = serde_json::to_string(&server.messages(&id).await).unwrap();
    for archived in [
        "FIRST_SUMMARY",
        "ORIGINAL_PROMPT",
        "ORIGINAL_REPLY",
        "SECOND_REPLY",
    ] {
        assert!(
            saved.contains(archived),
            "lost browsable history: {archived}"
        );
    }
}

#[tokio::test]
async fn compaction_cancellation_releases_a_stalled_summary_and_allows_the_next_turn() {
    for proactive in [true, false] {
        let (release, gate) = oneshot::channel();
        let mut summary = text_script("partial summary", " that must not be saved");
        summary.gate = Some(gate);
        let first = if proactive {
            Script::ok(vec![
                json!({"choices":[{"index":0,"delta":{"content":"working"},"finish_reason":"stop"}],"usage":{"prompt_tokens":31100,"completion_tokens":1}}),
            ])
        } else {
            Script::http_status_body(
                400,
                "maximum context length is 32000 tokens; prompt contains 32001 tokens",
            )
        };
        let mut provider =
            Provider::start(vec![first, summary, text_script("Continued", "")]).await;
        let server = TestServer::start(&provider, json!({})).await;
        let id = server.session(allow()).await;
        let mut events = server.events().await;
        server
            .post(
                &format!("/session/{id}/prompt_async"),
                &prompt("msg_compacting", "start"),
            )
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
        provider.request().await;
        let request = provider.request().await;
        assert!(request["tools"]
            .as_array()
            .is_none_or(|tools| tools.is_empty()));
        server
            .post(&format!("/session/{id}/abort"), &json!({}))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), events.through("session.idle"))
            .await
            .expect("abort must not wait for the summarizer's next token");
        assert!(!server
            .messages(&id)
            .await
            .iter()
            .any(|m| m["info"]["summary"] == true));
        drop(release);
        let reply: Value = server
            .post(
                &format!("/session/{id}/message"),
                &prompt("msg_after_abort", "continue"),
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
            .any(|part| part["text"] == "Continued"));
    }
}

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
        next.to_string().contains("Prior answer"),
        "compaction must preserve the recent reply verbatim"
    );
    assert!(
        next.to_string().contains("Please inspect tests"),
        "compaction must preserve the recent user instruction verbatim"
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
        text_script("Next turn", ""),
        text_script("Fork turn", ""),
    ]).await;
    let mut server = TestServer::start(&provider, json!({})).await;
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
    server.restart().await;
    server
        .post(
            &format!("/session/{id}/message"),
            &prompt("msg_after_overflow", "Continue from that reply"),
        )
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let next = provider.request().await;
    let sent = next["messages"].as_array().unwrap();
    assert!(sent
        .iter()
        .any(|message| message["role"] == "user" && message["content"] == "hello"));
    assert!(sent.iter().any(
        |message| message["role"] == "assistant" && message["content"] == "Done after overflow"
    ));
    let fork: Value = server
        .post(&format!("/session/{id}/fork"), &json!({}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let fork = fork["id"].as_str().unwrap();
    server
        .post(
            &format!("/session/{fork}/message"),
            &prompt("msg_fork_after_overflow", "Continue in fork"),
        )
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let next = provider.request().await;
    assert!(next["messages"]
        .as_array()
        .unwrap()
        .iter()
        .any(
            |message| message["role"] == "assistant" && message["content"] == "Done after overflow"
        ));
}

#[tokio::test]
async fn recent_tool_messages_replay_verbatim_after_manual_compaction_and_restart() {
    let instruction =
        "Keep this task exact: café 中文\nPreserve the public API; run the remaining tests.";
    let mut provider = Provider::start(vec![
        tool_script("read", json!({"filePath":"task.txt"}), "kept_call"),
        text_script("Next action: run the remaining tests.", ""),
        text_script("Checkpoint", ""),
        text_script("Continued", ""),
    ])
    .await;
    let mut server = TestServer::start(&provider, json!({})).await;
    std::fs::write(server.directory.join("task.txt"), "exact tool result: é\n").unwrap();
    let id = server.session(allow()).await;
    server
        .post(
            &format!("/session/{id}/message"),
            &prompt("msg_tools", instruction),
        )
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    provider.request().await;
    let original = provider.request().await;
    let expected: Vec<Value> = original["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|message| message["role"] != "system")
        .cloned()
        .collect();
    assert_eq!(expected.len(), 3);
    server
        .post(
            &format!("/session/{id}/summarize"),
            &json!({"providerID":"mock","modelID":"test-model"}),
        )
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    provider.request().await;
    server.restart().await;
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
    let messages: Vec<Value> = next["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|message| message["role"] != "system")
        .cloned()
        .collect();
    assert!(messages[0]["content"]
        .as_str()
        .unwrap()
        .contains("Checkpoint"));
    // Chat providers receive one merged assistant message per turn, so the
    // replayed checkpoint merges the turn's steps: the tool call keeps its
    // identifier, the final text rides on the same message, and the exact
    // (unicode) tool result is replayed verbatim.
    assert_eq!(messages[1], expected[0]);
    assert_eq!(messages[2]["role"], "assistant");
    assert_eq!(
        messages[2]["tool_calls"], expected[1]["tool_calls"],
        "tool call identifiers replay verbatim"
    );
    assert_eq!(
        messages[2]["content"],
        "Next action: run the remaining tests."
    );
    assert_eq!(messages[3], expected[2], "tool result replays verbatim");
    assert_eq!(messages[4]["role"], "user");
    assert_eq!(messages[4]["content"], "Continue");
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
    let messages = follow_up["messages"].as_array().unwrap();
    assert!(messages
        .iter()
        .any(|m| m["role"] == "user" && m["content"] == "first"));
    assert!(messages
        .iter()
        .any(|m| m["role"] == "assistant" && m["content"] == "working"));
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

#[tokio::test]
async fn context_overflow_compaction_retries_a_dying_summary_request() {
    // The summary request carries most of the window, so a flaky relay kills it
    // far more often than a normal request. Recovery must retry it like the main
    // loop does; failing it on the first transport death surfaces the raw
    // overflow and throws the whole turn away.
    let mut provider = Provider::start(vec![
        Script::http_status_body(400, "This model's maximum context length is 262144 tokens. However, you requested 0 output tokens and your prompt contains at least 262145 input tokens, for a total of at least 262145 tokens. Please reduce the length of the input prompt or the number of requested output tokens. (parameter=input_tokens, value=262145)"),
        Script::mid_stream_error("provider stream failed: error decoding response body"),
        text_script("Compacted. ", "Carrying on"),
        text_script("Done ", "after a retried compaction"),
    ])
    .await;
    let server = TestServer::start(&provider, json!({})).await;
    let id = server.session(allow()).await;
    let mut events = server.events().await;
    let response = server
        .post(
            &format!("/session/{id}/message"),
            &prompt("msg_flaky_summary", "hello"),
        )
        .send()
        .await
        .unwrap();
    assert!(
        response.status().is_success(),
        "a summary request that dies mid-stream must be retried, not fatal: {:?}",
        response.text().await.unwrap()
    );
    let terminal = events.through("session.idle").await;
    assert!(
        !terminal
            .iter()
            .any(|event| event["type"] == "session.error"),
        "the overflow must not surface: {terminal:?}"
    );
    assert_eq!(
        terminal
            .iter()
            .filter(|event| event["type"] == "session.compacted")
            .count(),
        1
    );
    let requests: Vec<_> = std::iter::from_fn(|| provider.requests.try_recv().ok()).collect();
    assert_eq!(
        requests.len(),
        4,
        "step, dying summary, summary retry, replay: {requests:#?}"
    );
    let replay = requests.last().unwrap();
    assert!(
        replay["messages"].as_array().unwrap().iter().any(|m| {
            m["content"].as_str().is_some_and(|c| {
                c.contains("Continuation summary") && c.contains("Compacted. Carrying on")
            })
        }),
        "the replay must carry the retried summary"
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
    assert_eq!(text, "Done after a retried compaction");
}

#[tokio::test]
async fn compaction_summarizer_caps_tool_outputs_like_upstream() {
    // Upstream serializes the summarizer input with each tool result capped at
    // 2000 characters. Whole tool outputs make the summary request the largest
    // of the turn, the one a flaky relay kills first. The verbatim tail replay
    // keeps full results; only the summarizer input is capped.
    let big = "x".repeat(9000);
    let mut provider = Provider::start(vec![
        tool_script(
            "bash",
            json!({"command": "printf 'x%.0s' $(seq 1 9000)"}),
            "call_0",
        ),
        Script::http_status_body(400, "maximum context length is 32000 tokens"),
        text_script("Capped summary. ", "Ready to continue"),
        text_script("Done ", "with a capped summarizer input"),
    ])
    .await;
    let server = TestServer::start(&provider, json!({})).await;
    let id = server.session(allow()).await;
    let mut events = server.events().await;
    let response = server
        .post(
            &format!("/session/{id}/message"),
            &prompt("msg_capped", "print a lot"),
        )
        .send()
        .await
        .unwrap();
    assert!(
        response.status().is_success(),
        "overflow recovery must succeed: {:?}",
        response.text().await.unwrap()
    );
    let terminal = events.through("session.idle").await;
    assert!(
        !terminal
            .iter()
            .any(|event| event["type"] == "session.error"),
        "the overflow must not surface: {terminal:?}"
    );
    let requests: Vec<_> = std::iter::from_fn(|| provider.requests.try_recv().ok()).collect();
    assert_eq!(
        requests.len(),
        4,
        "tool step, overflow, summarizer, replay: {requests:#?}"
    );
    let summarizer = &requests[2];
    assert_eq!(
        summarizer["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|message| message["role"] == "tool")
            .filter_map(|message| message["content"].as_str())
            .map(|content| content.chars().filter(|c| *c == 'x').count())
            .sum::<usize>(),
        2000,
        "the summarizer input must cap the tool result at 2000 characters"
    );
    let summarizer_text = summarizer.to_string();
    assert!(
        summarizer_text.contains("[truncated]"),
        "the cap must be marked: {summarizer_text}"
    );
    assert!(
        requests[3].to_string().contains(&big),
        "the verbatim tail keeps the full tool result"
    );
}
