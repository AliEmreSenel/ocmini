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

#[tokio::test]
async fn same_file_edits_use_each_preceding_edits_result() {
    for (name, edits, expected, statuses) in [
        (
            "independent",
            vec![
                json!({"oldString":"one","newString":"ONE"}),
                json!({"oldString":"two","newString":"TWO"}),
            ],
            "ONE\nTWO\n",
            vec!["completed", "completed"],
        ),
        (
            "chained",
            vec![
                json!({"oldString":"one","newString":"ONE"}),
                json!({"oldString":"ONE","newString":"FIRST"}),
            ],
            "FIRST\ntwo\n",
            vec!["completed", "completed"],
        ),
        (
            "conflicting",
            vec![
                json!({"oldString":"one","newString":"FIRST"}),
                json!({"oldString":"one","newString":"SECOND"}),
            ],
            "FIRST\ntwo\n",
            vec!["completed", "error"],
        ),
    ] {
        let tool_calls: Vec<_> = edits.into_iter().enumerate().map(|(index, mut args)| {
            args["filePath"] = json!("shared.txt");
            json!({"index":index,"id":format!("call_edit_{index}"),"type":"function","function":{"name":"edit","arguments":args.to_string()}})
        }).collect();
        let mut provider = Provider::start(vec![
            Script::ok(vec![
                json!({"choices":[{"index":0,"delta":{"tool_calls":tool_calls},"finish_reason":null}]}),
                json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}],"usage":usage()}),
            ]),
            text_script("Edits settled", ""),
        ]).await;
        let server = TestServer::start(&provider, json!({})).await;
        std::fs::write(server.directory.join("shared.txt"), "one\ntwo\n").unwrap();
        let id = server.session(allow()).await;
        server
            .post(
                &format!("/session/{id}/message"),
                &prompt(&format!("msg_edits_{name}"), "Edit both lines"),
            )
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
        let messages = server.messages(&id).await;
        let parts: Vec<_> = messages
            .iter()
            .flat_map(|message| message["parts"].as_array().unwrap())
            .filter(|part| part["type"] == "tool")
            .collect();
        assert_eq!(parts.len(), statuses.len(), "{name}: {parts:?}");
        for (index, status) in statuses.iter().enumerate() {
            assert_eq!(parts[index]["callID"], format!("call_edit_{index}"));
            assert_eq!(
                parts[index]["state"]["status"], *status,
                "{name}: {parts:?}"
            );
            if *status == "error" {
                assert!(parts[index]["state"]["error"]
                    .as_str()
                    .unwrap()
                    .contains("oldString was not found"));
            }
        }
        assert_eq!(
            std::fs::read_to_string(server.directory.join("shared.txt")).unwrap(),
            expected,
            "{name}"
        );
        provider.request().await;
        let continuation = provider.request().await;
        let results: Vec<_> = continuation["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|message| message["role"] == "tool")
            .collect();
        assert_eq!(results.len(), statuses.len());
        for (index, result) in results.iter().enumerate() {
            assert_eq!(result["tool_call_id"], format!("call_edit_{index}"));
        }
    }
}

#[tokio::test]
async fn edit_approval_serializes_one_file_without_blocking_other_tools() {
    let calls = [
        (
            "edit",
            json!({"filePath":"shared.txt","oldString":"one","newString":"ONE"}),
        ),
        (
            "edit",
            json!({"filePath":"shared.txt","oldString":"two","newString":"TWO"}),
        ),
        (
            "write",
            json!({"filePath":"unrelated.txt","content":"independent"}),
        ),
    ];
    let tool_calls: Vec<_> = calls.iter().enumerate().map(|(index, (name, args))| {
        json!({"index":index,"id":format!("call_approval_{index}"),"type":"function","function":{"name":name,"arguments":args.to_string()}})
    }).collect();
    let provider = Provider::start(vec![
        Script::ok(vec![
            json!({"choices":[{"index":0,"delta":{"tool_calls":tool_calls},"finish_reason":null}]}),
            json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}],"usage":usage()}),
        ]),
        text_script("Edits approved", ""),
    ]).await;
    let server = TestServer::start(&provider, json!({})).await;
    std::fs::write(server.directory.join("shared.txt"), "one\ntwo\n").unwrap();
    let id = server
        .session(json!([
            {"permission":"*","pattern":"*","action":"allow"},
            {"permission":"edit","pattern":"*shared.txt","action":"ask"}
        ]))
        .await;
    let mut events = server.events().await;
    server
        .post(
            &format!("/session/{id}/prompt_async"),
            &prompt(
                "msg_edit_approvals",
                "Edit both lines and write another file",
            ),
        )
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    for index in 0..2 {
        let asked = events.through("permission.asked").await.pop().unwrap();
        assert_eq!(
            asked["properties"]["tool"]["callID"],
            format!("call_approval_{index}")
        );
        if index == 0 {
            let messages = poll_messages(&server, &id, |messages| {
                messages
                    .iter()
                    .flat_map(|message| message["parts"].as_array().unwrap())
                    .any(|part| {
                        part["callID"] == "call_approval_2"
                            && part["state"]["status"] == "completed"
                    })
            })
            .await;
            assert!(
                messages
                    .iter()
                    .flat_map(|message| message["parts"].as_array().unwrap())
                    .any(|part| {
                        part["callID"] == "call_approval_2"
                            && part["state"]["status"] == "completed"
                    }),
                "unrelated write must complete while the edit waits for approval"
            );
            assert_eq!(
                std::fs::read_to_string(server.directory.join("unrelated.txt")).unwrap(),
                "independent"
            );
            assert_eq!(
                std::fs::read_to_string(server.directory.join("shared.txt")).unwrap(),
                "one\ntwo\n"
            );
        } else {
            assert_eq!(
                std::fs::read_to_string(server.directory.join("shared.txt")).unwrap(),
                "ONE\ntwo\n"
            );
        }
        let request = asked["properties"]["id"].as_str().unwrap();
        server
            .post(
                &format!("/permission/{request}/reply"),
                &json!({"reply":"once"}),
            )
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
    }
    events.through("session.idle").await;
    assert_eq!(
        std::fs::read_to_string(server.directory.join("shared.txt")).unwrap(),
        "ONE\nTWO\n"
    );
}

#[tokio::test]
async fn concurrent_file_tools_preserve_the_last_successful_content() {
    let calls = [
        (
            "edit",
            json!({"filePath":"shared.txt","oldString":"base","newString":"edited"}),
            "edited\n",
        ),
        (
            "write",
            json!({"filePath":"shared.txt","content":"written\n"}),
            "written\n",
        ),
        (
            "apply_patch",
            json!({"patchText":"*** Begin Patch\n*** Update File: shared.txt\n@@\n-base\n+patched\n*** End Patch"}),
            "patched\n",
        ),
    ];
    let tool_calls: Vec<_> = calls.iter().enumerate().map(|(index, (name, args, _))| {
        json!({"index":index,"id":format!("call_file_{index}"),"type":"function","function":{"name":name,"arguments":args.to_string()}})
    }).collect();
    let provider = Provider::start(vec![
        Script::ok(vec![
            json!({"choices":[{"index":0,"delta":{"tool_calls":tool_calls},"finish_reason":null}]}),
            json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}],"usage":usage()}),
        ]),
        text_script("Conflicts need a fresh read", ""),
    ]).await;
    let server = TestServer::start(&provider, json!({})).await;
    std::fs::write(server.directory.join("shared.txt"), "base\n").unwrap();
    let id = server.session(allow()).await;
    let mut events = server.events().await;
    server
        .post(
            &format!("/session/{id}/prompt_async"),
            &prompt("msg_file_conflicts", "Change the shared file"),
        )
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    events.through("session.idle").await;
    let messages = server.messages(&id).await;
    let parts: Vec<_> = messages
        .iter()
        .flat_map(|message| message["parts"].as_array().unwrap())
        .filter(|part| part["type"] == "tool")
        .collect();
    assert_eq!(parts.len(), calls.len());
    // The edit owns the path from preparation onward. A write prepared after
    // it completes may also succeed; one prepared earlier must reject its
    // stale baseline. Neither a stale write nor patch may discard the edit.
    assert_eq!(parts[0]["state"]["status"], "completed", "{parts:?}");
    assert_eq!(parts[2]["state"]["status"], "error", "{parts:?}");
    for index in 0..calls.len() {
        let part = parts
            .iter()
            .find(|part| part["callID"] == format!("call_file_{index}"))
            .unwrap();
        if part["state"]["status"] != "completed" {
            assert_eq!(part["state"]["status"], "error");
            let error = part["state"]["error"].as_str().unwrap();
            assert!(
                error.contains("File changed since preparation")
                    || (index == 2 && error.contains("Expected context was not found")),
                "{error}"
            );
        }
    }
    assert_eq!(
        std::fs::read_to_string(server.directory.join("shared.txt")).unwrap(),
        if parts[1]["state"]["status"] == "completed" {
            calls[1].2
        } else {
            calls[0].2
        }
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
