use super::*;

#[tokio::test]
async fn the_title_model_names_the_session_and_failures_fall_back_to_the_prompt() {
    // Requests arrive in order: title, answer, title (dies), answer.
    let mut provider = Provider::start(vec![
        text_script("Titled By Model", ""),
        text_script("first answer", ""),
        Script::http_status_body(500, "title model exploded"),
        text_script("second answer", ""),
    ])
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
    let turn = events.through("session.idle").await;
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

    // The title request carries the prompt, tool-free, before the answer. The
    // adapter sends the system prompt as the first message, so the body holds
    // the title instruction plus one user turn.
    let title_request = provider.request().await;
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
    provider.request().await; // The answer request.

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
    let mut provider = Provider::start(vec![
        text_script("Titled By Model", ""),
        text_script("first answer", ""),
        text_script("second answer", ""),
    ])
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
    events.through("session.idle").await;
    provider.request().await; // The title request.
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
