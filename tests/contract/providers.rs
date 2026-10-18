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
