use super::*;

async fn send(server: &TestServer, id: &str, mid: &str, text: &str) -> Value {
    server
        .post(&format!("/session/{id}/message"), &prompt(mid, text))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap()
}

fn near_window() -> Script {
    Script::ok(vec![
        json!({"choices":[{"index":0,"delta":{"content":"working"},"finish_reason":"stop"}],
        "usage":{"prompt_tokens":28000,"completion_tokens":100}}),
    ])
}

#[tokio::test]
async fn automatic_compaction_can_be_disabled_without_disabling_manual_summaries() {
    let mut high = near_window();
    high.chunks[0]["usage"]["prompt_tokens"] = json!(31100);
    let mut provider = Provider::start(vec![
        high,
        Script::http_status_body(400, "maximum context length is 32000 tokens"),
        text_script("MANUAL_CHECKPOINT", ""),
        text_script("Continued", ""),
    ])
    .await;
    let server = TestServer::start(&provider, json!({"compaction":{"auto":false}})).await;
    let id = server.session(allow()).await;
    send(&server, &id, "msg_high", "Start").await;
    provider.request().await;
    assert!(provider.requests.try_recv().is_err());
    let failed = server
        .post(
            &format!("/session/{id}/message"),
            &prompt("msg_overflow_disabled", "Overflow"),
        )
        .send()
        .await
        .unwrap();
    assert!(!failed.status().is_success());
    provider.request().await;
    assert!(
        provider.requests.try_recv().is_err(),
        "disabled automatic recovery must not call a summarizer"
    );
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
    send(&server, &id, "msg_after_manual", "Continue").await;
    assert!(provider
        .request()
        .await
        .to_string()
        .contains("MANUAL_CHECKPOINT"));
}

#[tokio::test]
async fn compaction_reserves_model_output_and_honors_configured_input_headroom() {
    for (reserved, compacted) in [(None, true), (Some(500), false), (Some(4000), true)] {
        let mut scripts = vec![near_window()];
        if compacted {
            scripts.push(text_script("HEADROOM_CHECKPOINT", ""));
        }
        let mut provider = Provider::start(scripts).await;
        let settings = reserved
            .map(|value| json!({"reserved":value}))
            .unwrap_or(json!({}));
        let server = TestServer::start(&provider, json!({
            "provider":{"mock":{"npm":"@ai-sdk/openai-compatible","options":{"baseURL":provider.url},
                "models":{"test-model":{"limit":{"context":32000,"input":30000,"output":2048}}}}},
            "compaction":settings
        })).await;
        let id = server.session(allow()).await;
        let mut events = server.events().await;
        send(&server, &id, "msg_headroom", "Check headroom").await;
        let terminal = events.through("session.idle").await;
        assert_eq!(
            terminal
                .iter()
                .any(|event| event["type"] == "session.compacted"),
            compacted
        );
        assert_eq!(
            std::iter::from_fn(|| provider.requests.try_recv().ok()).count(),
            if compacted { 2 } else { 1 }
        );
    }
}

#[tokio::test]
async fn overflowing_steps_can_compact_more_than_twice_while_context_keeps_shrinking() {
    let mut scripts = Vec::new();
    for size in [9000, 6000, 3000, 1000] {
        scripts.push(Script::http_status_body(
            400,
            "maximum context length is 32000 tokens",
        ));
        scripts.push(text_script(&format!("GOAL {}", "s".repeat(size)), ""));
    }
    scripts.push(text_script("Finished four recoveries", ""));
    let mut provider = Provider::start(scripts).await;
    let server = TestServer::start(
        &provider,
        json!({"compaction":{"preserve_recent_tokens":0}}),
    )
    .await;
    let id = server.session(allow()).await;
    let mut events = server.events().await;
    let reply = send(
        &server,
        &id,
        "msg_shrinking",
        &format!("GOAL {}", "x".repeat(16000)),
    )
    .await;
    assert!(reply["parts"]
        .as_array()
        .unwrap()
        .iter()
        .any(|part| part["text"] == "Finished four recoveries"));
    let terminal = events.through("session.idle").await;
    assert_eq!(
        terminal
            .iter()
            .filter(|event| event["type"] == "session.compacted")
            .count(),
        4
    );
    assert_eq!(
        std::iter::from_fn(|| provider.requests.try_recv().ok()).count(),
        9
    );
}

#[tokio::test]
async fn pruning_old_tool_results_is_opt_in_and_keeps_the_archive() {
    for enabled in [false, true] {
        let mut scripts = (0..8)
            .map(|index| {
                tool_script(
                    "bash",
                    json!({"command":format!("printf 'OLD_{index}_'; printf '%050000d' {index}")}),
                    &format!("call_{index}"),
                )
            })
            .collect::<Vec<_>>();
        scripts.extend([
            text_script("Initial tools done", ""),
            text_script("Second turn", ""),
            text_script("Third turn", ""),
            text_script("Fourth turn", ""),
        ]);
        let mut provider = Provider::start(scripts).await;
        let server = TestServer::start(
            &provider,
            json!({"compaction":{"auto":false,"prune":enabled}}),
        )
        .await;
        let id = server.session(allow()).await;
        for index in 0..3 {
            send(&server, &id, &format!("msg_prune_{index}"), "Continue").await;
        }
        let archived = server.messages(&id).await;
        let parts: Vec<_> = archived
            .iter()
            .flat_map(|message| message["parts"].as_array().unwrap())
            .filter(|part| part["type"] == "tool")
            .collect();
        assert_eq!(parts.len(), 8);
        let pruned = parts
            .iter()
            .filter(|part| part["state"]["time"]["compacted"].is_number())
            .count();
        assert_eq!(pruned > 0, enabled);
        assert!(
            parts
                .iter()
                .all(|part| part["state"]["output"].as_str().unwrap().contains("OLD_")),
            "pruning must preserve browsable results"
        );
        while provider.requests.try_recv().is_ok() {}
        send(&server, &id, "msg_prune_final", "Continue").await;
        let next = provider.request().await;
        let cleared = next["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|message| message["content"] == "[Old tool result content cleared]")
            .count();
        assert_eq!(cleared, pruned);
    }
}

#[tokio::test]
async fn subagent_nesting_defaults_to_eight_and_honors_zero_and_larger_depths() {
    for setting in [None, Some(0u64), Some(2), Some(10)] {
        let limit = setting.unwrap_or(8);
        let mut scripts = (0..=limit).map(|depth| tool_script("task", json!({
            "description":format!("Level {depth}"),"prompt":format!("Enter level {depth}"),"subagent_type":"general"
        }), &format!("call_depth_{depth}"))).collect::<Vec<_>>();
        scripts.extend((0..=limit).map(|_| text_script("Level complete", "")));
        let provider = Provider::start(scripts).await;
        let config = setting
            .map(|value| json!({"subagent_depth":value}))
            .unwrap_or(json!({}));
        let server = TestServer::start(&provider, config).await;
        let root = server.session(allow()).await;
        send(&server, &root, "msg_nesting", "Delegate recursively").await;
        let mut sid = root;
        for _ in 0..limit {
            let children: Vec<Value> = server
                .get(&format!("/session/{sid}/children"))
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            assert_eq!(children.len(), 1);
            sid = children[0]["id"].as_str().unwrap().to_string();
        }
        let children: Vec<Value> = server
            .get(&format!("/session/{sid}/children"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(children.is_empty());
        let messages = server.messages(&sid).await;
        assert!(messages
            .iter()
            .flat_map(|message| message["parts"].as_array().unwrap())
            .any(|part| {
                part["state"]["error"].as_str().is_some_and(|error| {
                    error.contains(&format!("Subagent nesting limit ({limit})"))
                })
            }));
    }
}

#[tokio::test]
async fn a_provider_can_recover_on_the_fifth_retry() {
    let mut scripts = (0..5)
        .map(|_| Script::http_status(503, Some("0")))
        .collect::<Vec<_>>();
    scripts.push(text_script("Fifth retry recovered", ""));
    let mut provider = Provider::start(scripts).await;
    let server = TestServer::start(&provider, json!({})).await;
    let id = server.session(allow()).await;
    let mut events = server.events().await;
    send(&server, &id, "msg_retry_five", "Retry").await;
    let terminal = events.through("session.idle").await;
    assert_eq!(
        terminal
            .iter()
            .filter(|event| event["type"] == "session.status"
                && event["properties"]["status"]["type"] == "retry")
            .count(),
        5
    );
    assert_eq!(
        std::iter::from_fn(|| provider.requests.try_recv().ok()).count(),
        6
    );
}
