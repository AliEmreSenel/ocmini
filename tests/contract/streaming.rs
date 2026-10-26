use super::*;

#[tokio::test]
async fn streaming_admission_attachments_and_usage_reach_t3() {
    let (release, gate) = oneshot::channel();
    let mut script = text_script("Hello ", "world");
    script.gate = Some(gate);
    let mut provider = Provider::start(vec![script]).await;
    let server = TestServer::start(&provider, json!({})).await;
    let id = server.session(allow()).await;
    let attachment = server.directory.join("notes.txt");
    std::fs::write(&attachment, "ATTACHMENT_SENTINEL").unwrap();
    let mut request = prompt("msg_t3_admission_1", "Say hello");
    request["system"] = json!("T3_SYSTEM_SENTINEL");
    request["variant"] = json!("high");
    request["parts"].as_array_mut().unwrap().push(json!({"type": "file", "mime": "text/plain", "filename": "notes.txt", "url": format!("file://{}", encode_directory(&attachment).replace("%2F", "/"))}));
    let mut events = server.events().await;
    assert_eq!(
        server
            .post(&format!("/session/{id}/prompt_async"), &request)
            .send()
            .await
            .unwrap()
            .status(),
        204
    );
    let model_request = provider.request().await;
    assert!(model_request.to_string().contains("T3_SYSTEM_SENTINEL"));
    assert!(model_request.to_string().contains("ATTACHMENT_SENTINEL"));
    let first = events.through("message.part.delta").await;
    let delta = first.last().unwrap();
    let part_id = &delta["properties"]["partID"];
    assert!(first
        .iter()
        .any(|event| event["type"] == "message.part.updated"
            && event["properties"]["part"]["id"] == *part_id
            && event["properties"]["part"]["text"] == ""));
    assert!(first.iter().any(|event| event["type"] == "message.updated"
        && event["properties"]["info"]["id"] == "msg_t3_admission_1"));
    let status: Value = server
        .get("/session/status")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(status[&id]["type"], "busy");
    assert_eq!(
        server
            .post(&format!("/session/{id}/prompt_async"), &request)
            .send()
            .await
            .unwrap()
            .status(),
        204
    );
    assert!(
        provider.requests.try_recv().is_err(),
        "retry started a second provider request"
    );
    release.send(()).unwrap();
    let final_events = events.through("session.idle").await;
    assert!(final_events
        .iter()
        .any(|event| event["type"] == "message.part.updated"
            && event["properties"]["part"]["type"] == "step-finish"
            && event["properties"]["part"]["tokens"]["output"]
                .as_i64()
                .unwrap_or(0)
                > 0));
    for event in first.iter().chain(&final_events) {
        let properties = &event["properties"];
        match event["type"].as_str() {
            Some("message.updated") => {
                assert_eq!(properties["messageID"], properties["info"]["id"])
            }
            Some("message.part.updated") => {
                assert_eq!(properties["messageID"], properties["part"]["messageID"]);
                assert_eq!(properties["partID"], properties["part"]["id"]);
            }
            _ => {}
        }
    }
    let messages = server.messages(&id).await;
    assert_eq!(messages[0]["info"]["id"], "msg_t3_admission_1");
    let assistant = messages
        .iter()
        .find(|message| message["info"]["role"] == "assistant")
        .unwrap();
    assert_eq!(assistant["info"]["parentID"], "msg_t3_admission_1");
    assert!(assistant["info"]["time"]["completed"].is_number());
    assert!(assistant["parts"]
        .as_array()
        .unwrap()
        .iter()
        .any(|part| part["type"] == "text" && part["text"] == "Hello world"));
    let status: Value = server
        .get("/session/status")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(status.get(&id).is_none());
    let replay = server
        .post(&format!("/session/{id}/message"), &request)
        .send()
        .await
        .unwrap();
    assert_eq!(replay.status(), 200);
    assert_eq!(
        replay.json::<Value>().await.unwrap()["info"]["id"],
        assistant["info"]["id"]
    );
    assert_eq!(server.messages(&id).await.len(), messages.len());
    assert!(
        provider.requests.try_recv().is_err(),
        "completed retry generated again"
    );
}

#[tokio::test]
async fn steering_is_admitted_while_busy_and_processed_before_idle() {
    let (release, gate) = oneshot::channel();
    let mut first = text_script("First ", "response");
    first.gate = Some(gate);
    let mut provider = Provider::start(vec![first, text_script("Steered ", "response")]).await;
    let server = TestServer::start(&provider, json!({})).await;
    let id = server.session(allow()).await;
    let mut events = server.events().await;
    assert_eq!(
        server
            .post(
                &format!("/session/{id}/prompt_async"),
                &prompt("msg_first", "first")
            )
            .send()
            .await
            .unwrap()
            .status(),
        204
    );
    provider.request().await;
    events.through("message.part.delta").await;
    assert_eq!(
        server
            .post(
                &format!("/session/{id}/prompt_async"),
                &prompt("msg_steer", "STEERING_SENTINEL")
            )
            .send()
            .await
            .unwrap()
            .status(),
        204
    );
    assert!(server
        .messages(&id)
        .await
        .iter()
        .any(|message| message["info"]["id"] == "msg_steer"));
    release.send(()).unwrap();
    let next = provider.request().await;
    assert!(next.to_string().contains("STEERING_SENTINEL"));
    let events = events.through("session.idle").await;
    assert!(events.iter().any(|event| event["type"] == "message.updated"
        && event["properties"]["info"]["parentID"] == "msg_steer"
        && event["properties"]["info"]["time"]["completed"].is_number()));
}

#[tokio::test]
async fn abort_cancels_a_stalled_stream_without_waiting_for_another_token() {
    let (_release, gate) = oneshot::channel();
    let mut script = text_script("Started", "must not arrive");
    script.gate = Some(gate);
    let mut provider = Provider::start(vec![script]).await;
    let server = TestServer::start(&provider, json!({})).await;
    let id = server.session(allow()).await;
    let mut events = server.events().await;
    assert_eq!(
        server
            .post(
                &format!("/session/{id}/prompt_async"),
                &prompt("msg_abort", "hello")
            )
            .send()
            .await
            .unwrap()
            .status(),
        204
    );
    provider.request().await;
    events.through("message.part.delta").await;
    assert_eq!(
        server
            .post(&format!("/session/{id}/abort"), &json!({}))
            .send()
            .await
            .unwrap()
            .json::<Value>()
            .await
            .unwrap(),
        true
    );
    let terminal = tokio::time::timeout(Duration::from_secs(3), events.through("session.idle"))
        .await
        .expect("abort waited for stalled provider");
    assert!(terminal.iter().any(|event| event["type"] == "session.error"
        && event["properties"]["error"]["name"] == "MessageAbortedError"));
    assert!(!server
        .messages(&id)
        .await
        .to_vec()
        .iter()
        .any(|message| message.to_string().contains("must not arrive")));
}
