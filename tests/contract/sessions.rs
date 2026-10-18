use super::*;

#[tokio::test]
async fn session_list_limits_apply_after_directory_root_and_unicode_search_filters() {
    let provider = Provider::start(vec![]).await;
    let server = TestServer::start(&provider, json!({})).await;
    let mut created = Vec::new();
    for title in ["CAFÉ older", "café recent", "unrelated newest"] {
        let session: Value = server
            .post("/session", &json!({"title":title}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        created.push(session);
    }
    let child: Value = server
        .post(
            "/session",
            &json!({"title":"café child","parentID":created[0]["id"]}),
        )
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let other = server.root.path().join("other-project");
    std::fs::create_dir(&other).unwrap();
    server
        .post("/session", &json!({"title":"café other","directory":other}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let directory = server
        .directory
        .join("unused/..")
        .to_string_lossy()
        .into_owned();
    for limit in [0usize, 1, 2, 10] {
        let rows: Vec<Value> = server
            .get("/session")
            .query(&[
                ("directory", directory.as_str()),
                ("search", "CAFÉ"),
                ("roots", "true"),
            ])
            .query(&[("limit", limit)])
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let expected = [&created[1]["id"], &created[0]["id"]];
        assert_eq!(
            rows.iter().map(|row| &row["id"]).collect::<Vec<_>>(),
            expected[..limit.min(2)]
        );
    }
    let rows: Vec<Value> = server
        .get("/session")
        .query(&[("search", "CAFÉ"), ("limit", "1")])
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(rows[0]["id"], child["id"]);
}

#[tokio::test]
async fn session_scoping_and_validation_are_consistent() {
    let provider = Provider::start(vec![]).await;
    let server = TestServer::start(&provider, json!({})).await;
    let id = server.session(allow()).await;
    let session: Value = server
        .get(&format!("/session/{id}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        session["directory"],
        server.directory.to_string_lossy().as_ref()
    );
    let other = server.root.path().join("other-project");
    std::fs::create_dir_all(&other).unwrap();
    let response = server
        .get(&format!("/session/{id}"))
        .header("x-opencode-directory", encode_directory(&other))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 404);
    let all: Vec<Value> = server
        .get("/session")
        .query(&[("directory", server.directory.to_string_lossy().as_ref())])
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(all.len(), 1);
    assert_eq!(
        server
            .post(
                "/session/missing/prompt_async",
                &prompt("msg_missing", "hello")
            )
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
    assert_eq!(
        server
            .post(
                &format!("/session/{id}/prompt_async"),
                &json!({"parts": [{"type": "file", "url": "file:///tmp/x"}]})
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
                &format!("/session/{id}/command"),
                &json!({"command": "missing", "arguments": ""})
            )
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
    assert_eq!(
        server
            .post("/permission/missing/reply", &json!({"reply": "invalid"}))
            .send()
            .await
            .unwrap()
            .status(),
        400
    );
    assert_eq!(
        server
            .post("/permission/missing/reply", &json!({"reply": "once"}))
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
}

#[tokio::test]
async fn fork_excludes_boundary_remaps_references_and_survives_restart() {
    let provider = Provider::start(vec![
        text_script("Answer one", ""),
        text_script("Answer two", ""),
    ])
    .await;
    let mut server = TestServer::start(&provider, json!({})).await;
    let id = server.session(allow()).await;
    let mut events = server.events().await;
    for (mid, text) in [("msg_turn_one", "one"), ("msg_turn_two", "two")] {
        assert_eq!(
            server
                .post(&format!("/session/{id}/message"), &prompt(mid, text))
                .send()
                .await
                .unwrap()
                .status(),
            200
        );
        events.through("session.idle").await;
    }
    let original = server.messages(&id).await;
    let boundary_index = original
        .iter()
        .position(|message| message["info"]["id"] == "msg_turn_two")
        .unwrap();
    let fork: Value = server
        .post(
            &format!("/session/{id}/fork"),
            &json!({"messageID": "msg_turn_two"}),
        )
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let fork_id = fork["id"].as_str().unwrap().to_owned();
    assert!(fork.get("parentID").is_none());
    let messages = server.messages(&fork_id).await;
    assert_eq!(messages.len(), boundary_index);
    assert_eq!(messages[1]["info"]["parentID"], messages[0]["info"]["id"]);
    for (message, source) in messages.iter().zip(&original) {
        assert_eq!(message["info"]["sessionID"], fork_id);
        assert_ne!(message["info"]["id"], source["info"]["id"]);
        for part in message["parts"].as_array().unwrap() {
            assert_eq!(part["sessionID"], fork_id);
            assert_eq!(part["messageID"], message["info"]["id"]);
        }
    }
    assert_eq!(server.messages(&id).await, original);
    server.restart().await;
    assert_eq!(server.messages(&fork_id).await, messages);
    assert_eq!(server.messages(&id).await, original);
    let deleted = server
        .client
        .delete(format!("{}/session/{fork_id}", server.url))
        .send()
        .await
        .unwrap();
    assert_eq!(deleted.json::<Value>().await.unwrap(), true);
    assert_eq!(
        server
            .get(&format!("/session/{fork_id}/message"))
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
}

#[tokio::test]
async fn directory_event_streams_exclude_other_projects() {
    let provider = Provider::start(vec![]).await;
    let server = TestServer::start(&provider, json!({})).await;
    let mut events = server.events().await;
    let first = server.session(allow()).await;
    let other = server.root.path().join("separate-project");
    std::fs::create_dir_all(&other).unwrap();
    let response = server
        .post(
            "/session",
            &json!({"directory": other, "permission": allow()}),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let last = server.session(allow()).await;
    let first_event = events.through("session.created").await.pop().unwrap();
    let last_event = events.through("session.created").await.pop().unwrap();
    assert_eq!(first_event["properties"]["info"]["id"], first);
    assert_eq!(last_event["properties"]["info"]["id"], last);
}

#[tokio::test]
async fn no_reply_and_duplicate_admission_persist_without_a_model_call() {
    let mut provider = Provider::start(vec![]).await;
    let mut server = TestServer::start(&provider, json!({})).await;
    let id = server.session(allow()).await;
    let mut request = prompt("msg_seed", "Retain this context");
    request["noReply"] = json!(true);
    for _ in 0..2 {
        let response = server
            .post(&format!("/session/{id}/message"), &request)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(
            response.json::<Value>().await.unwrap()["info"]["role"],
            "user"
        );
    }
    assert_eq!(server.messages(&id).await.len(), 1);
    server.restart().await;
    assert_eq!(server.messages(&id).await.len(), 1);
    assert!(provider.requests.try_recv().is_err());
}
