use super::*;

#[tokio::test]
async fn configured_commands_expand_arguments_and_preserve_admission_id() {
    let mut provider = Provider::start(vec![text_script("Review complete", "")]).await;
    let server = TestServer::start(&provider, json!({})).await;
    let id = server.session(allow()).await;
    let response = server.post(&format!("/session/{id}/command"), &json!({"command": "review", "arguments": "\"alpha beta\" gamma delta", "model": "mock/test-model", "messageID": "msg_command", "agent": "reviewer", "variant": "high", "parts": []})).send().await.unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.json::<Value>().await.unwrap()["info"]["role"],
        "assistant"
    );
    let request = provider.request().await;
    assert!(request
        .to_string()
        .contains("First alpha beta. Rest gamma delta."));
    assert!(request.to_string().contains("REVIEWER_SYSTEM"));
    assert!(server
        .messages(&id)
        .await
        .iter()
        .any(|message| message["info"]["id"] == "msg_command"));
    let commands: Value = server
        .get("/command")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(commands[0]["name"], "review");
    assert_eq!(commands[0]["hints"], json!(["$1", "$2"]));
}

#[tokio::test]
async fn omitted_selections_use_the_configured_default_agent_and_its_model() {
    let mut provider = Provider::start(vec![text_script("Default agent answered", "")]).await;
    let server = TestServer::start(&provider, json!({
        "default_agent": "reviewer",
        "provider": {"mock": {"npm": "@ai-sdk/openai-compatible", "options": {"baseURL": provider.url},
            "models": {"test-model": {"name": "Global model"}, "agent-model": {"name": "Agent model"}}
        }},
        "agent": {"reviewer": {"mode": "primary", "model": "mock/agent-model", "prompt": "DEFAULT_AGENT_SENTINEL"}}
    })).await;
    let id = server.session(allow()).await;
    let response = server.post(&format!("/session/{id}/message"), &json!({
        "messageID": "msg_default_agent", "parts": [{"type": "text", "text": "Use my defaults"}]
    })).send().await.unwrap();
    assert_eq!(response.status(), 200);
    let request = provider.request().await;
    assert_eq!(request["model"], "agent-model");
    assert!(request.to_string().contains("DEFAULT_AGENT_SENTINEL"));
    let messages = server.messages(&id).await;
    assert_eq!(messages[0]["info"]["agent"], "reviewer");
    assert_eq!(messages[0]["info"]["model"]["modelID"], "agent-model");
}
