use super::*;

#[tokio::test]
async fn unreachable_mcp_is_never_reported_as_connected() {
    let provider = Provider::start(vec![]).await;
    let server = TestServer::start(&provider, json!({})).await;
    let response = server.post("/mcp", &json!({"name": "unreachable", "config": {"type": "remote", "url": "http://127.0.0.1:1/mcp", "oauth": false}})).send().await.unwrap();
    assert_eq!(response.status(), 200);
    let status = response.json::<Value>().await.unwrap();
    assert_ne!(status["unreachable"]["status"], "connected");
    assert!(status["unreachable"]["status"].is_string());
    assert!(!server
        .post("/mcp/unreachable/connect", &json!({}))
        .send()
        .await
        .unwrap()
        .status()
        .is_success());
    let other = server.root.path().join("other-mcp-project");
    std::fs::create_dir_all(&other).unwrap();
    let status: Value = server
        .get("/mcp")
        .header("x-opencode-directory", encode_directory(&other))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(status.get("unreachable").is_none());
}
