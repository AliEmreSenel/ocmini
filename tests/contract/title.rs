use super::*;

#[tokio::test]
async fn the_title_model_names_the_session_and_failures_fall_back_to_the_prompt() {
    let mut provider = Provider::start_with_titles(
        vec![
            text_script("first answer", ""),
            text_script("second answer", ""),
        ],
        vec![
            text_script("Titled By Model", ""),
            Script::http_status_body(500, "title model exploded"),
        ],
    )
    .await;
    let server = TestServer::start(&provider, json!({"title_model": "mock/test-model"})).await;

    let created: Value = server
        .post("/session", &json!({}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = created["id"].as_str().unwrap().to_owned();
    assert_eq!(created["title"], "New session");

    let mut events = server.events().await;
    server
        .post(
            &format!("/session/{id}/prompt_async"),
            &prompt("msg_title", "Fix the resume picker\nsecond line"),
        )
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let mut turn = events.through("session.idle").await;
    while !turn
        .iter()
        .any(|event| event["type"] == "session.title" && event["properties"]["state"] == "done")
    {
        turn.extend(events.through("session.title").await);
    }
    let titles: Vec<&Value> = turn
        .iter()
        .filter(|event| event["type"] == "session.title")
        .collect();
    assert_eq!(titles[0]["properties"]["state"], "start");
    assert_eq!(titles[1]["properties"]["state"], "done");
    assert_eq!(titles[1]["properties"]["title"], "Titled By Model");
    let session: Value = server
        .get(&format!("/session/{id}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(session["title"], "Titled By Model");

    // The title request carries the prompt, tool-free. The adapter sends the system prompt as the first message, so the body holds
    // the title instruction plus one user turn.
    let requests = [provider.request().await, provider.request().await];
    let title_request = requests
        .iter()
        .find(|request| {
            request["messages"][0]["content"]
                .as_str()
                .is_some_and(|text| text.starts_with("Name this conversation"))
        })
        .unwrap();
    let messages = title_request["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 2, "{title_request}");
    assert_eq!(messages[0]["role"], "system");
    assert!(
        messages[0]["content"]
            .as_str()
            .is_some_and(|text| text.contains("Name this conversation")),
        "{title_request}"
    );
    assert_eq!(messages[1]["role"], "user");
    assert!(
        messages[1]["content"]
            .to_string()
            .contains("Fix the resume picker"),
        "{title_request}"
    );
    assert!(title_request.get("tools").is_none() || title_request["tools"].is_null());

    // A title request that dies falls back to the first prompt line.
    let second: Value = server
        .post("/session", &json!({}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let second_id = second["id"].as_str().unwrap().to_owned();
    server
        .post(
            &format!("/session/{second_id}/prompt_async"),
            &prompt("msg_fallback", "Investigate flaky tests"),
        )
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    events.through("session.idle").await;
    let mut title = None;
    for _ in 0..50 {
        let session: Value = server
            .get(&format!("/session/{second_id}"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        if session["title"] != "New session" {
            title = Some(session["title"].as_str().unwrap().to_owned());
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(title.as_deref(), Some("Investigate flaky tests"));
}

#[tokio::test]
async fn a_session_with_a_title_stops_asking_the_title_model() {
    // Every prompt used to print "generating title…" under `title_model`:
    // admission started a title request for a session that already had a real
    // title, and `set_title` refused the rename, so no "done" ever followed it.
    // Naming is a first-prompt job; a titled session's prompts are only answered.
    let mut provider = Provider::start_with_titles(
        vec![
            text_script("first answer", ""),
            text_script("second answer", ""),
        ],
        vec![text_script("Titled By Model", "")],
    )
    .await;
    let server = TestServer::start(&provider, json!({"title_model": "mock/test-model"})).await;
    let created: Value = server
        .post("/session", &json!({}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = created["id"].as_str().unwrap().to_owned();

    let mut events = server.events().await;
    server
        .post(
            &format!("/session/{id}/prompt_async"),
            &prompt("msg_one", "Fix the resume picker"),
        )
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let mut first = events.through("session.idle").await;
    while !first
        .iter()
        .any(|event| event["type"] == "session.title" && event["properties"]["state"] == "done")
    {
        first.extend(events.through("session.title").await);
    }
    provider.request().await; // The title request or answer.
    provider.request().await; // The answer.

    server
        .post(
            &format!("/session/{id}/prompt_async"),
            &prompt("msg_two", "Keep going"),
        )
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let turn = events.through("session.idle").await;
    assert!(
        turn.iter().all(
            |event| event["type"] != "session.title" || event["properties"]["state"] != "start"
        ),
        "a titled session started writing a title again: {turn:?}"
    );

    // Admission starts a title request before the drain runs, so a title
    // request would arrive as this prompt's first request.
    let answer = provider.request().await;
    assert!(
        !answer["messages"]
            .as_array()
            .is_some_and(
                |messages| messages.iter().any(|message| message["role"] == "system"
                    && message["content"]
                        .as_str()
                        .is_some_and(|text| text.contains("Name this conversation")))
            ),
        "the second prompt asked for a title: {answer}"
    );
    assert!(
        provider.requests.try_recv().is_err(),
        "a titled session asked the title model again"
    );
    let session: Value = server
        .get(&format!("/session/{id}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(session["title"], "Titled By Model");
}

#[tokio::test]
async fn patching_a_title_to_a_placeholder_keeps_explicit_ownership() {
    let provider = Provider::start(vec![text_script("answer", "")]).await;
    let server = TestServer::start(&provider, json!({})).await;
    let sid = server.session(allow()).await;
    server
        .client
        .patch(format!("{}/session/{sid}", server.url))
        .json(&json!({"title":"run"}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    server
        .post(
            &format!("/session/{sid}/message"),
            &prompt(
                "msg_explicit_placeholder",
                "Must preserve the explicit title",
            ),
        )
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let session: Value = server
        .get(&format!("/session/{sid}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(session["title"], "run");
    assert_eq!(session["ocminiTitleSource"], "explicit");
}

#[tokio::test]
#[cfg(unix)]
async fn title_hooks_receive_child_ancestry_from_the_producer() {
    let provider = Provider::start_with_titles(
        vec![text_script("answer", "")],
        vec![text_script("Child title", "")],
    )
    .await;
    let command = "import json,sys,pathlib; r=json.load(sys.stdin); pathlib.Path('child-title-hook').write_text(json.dumps(r['context'])); print('{}')";
    let server = TestServer::start(&provider, json!({"title_model":"mock/test-model","ocmini":{"hooks":{"handlers":[{"id":"child-title","on":"model.headers.transform","match":{"session":"child","requestKind":"title"},"command":["python3","-c",command]}]}}})).await;
    let root = server.session(allow()).await;
    let child: Value = server
        .post("/session", &json!({"parentID":root,"permission":allow()}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let sid = child["id"].as_str().unwrap();
    let mut events = server.events().await;
    server
        .post(
            &format!("/session/{sid}/message"),
            &prompt("msg_child_title_scope", "Name this child"),
        )
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let mut observed = events.through("session.idle").await;
    while !observed
        .iter()
        .any(|event| event["type"] == "session.title" && event["properties"]["state"] == "done")
    {
        observed.extend(events.through("session.title").await);
    }
    let context: Value =
        serde_json::from_slice(&std::fs::read(server.directory.join("child-title-hook")).unwrap())
            .unwrap();
    assert_eq!(context["sessionID"], sid);
    assert_eq!(context["parentID"], root);
    assert_eq!(context["requestKind"], "title");
    assert_eq!(
        context["directory"],
        server.directory.to_string_lossy().as_ref()
    );
}
