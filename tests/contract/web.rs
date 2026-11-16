use super::*;

fn query(value: &Path) -> String {
    value
        .to_string_lossy()
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

async fn post(server: &TestServer, path: &str, body: Value) -> Value {
    json_of(server.post(path, &body).send().await.unwrap(), 200).await
}

async fn json_of(response: reqwest::Response, status: u16) -> Value {
    assert_eq!(response.status(), status);
    response.json().await.unwrap()
}

#[tokio::test]
async fn web_interface_shares_the_password_gate_and_serves_the_page() {
    let provider = Provider::start(vec![]).await;
    let server = TestServer::start(&provider, json!({})).await;
    let anonymous = reqwest::get(format!("{}/", server.url)).await.unwrap();
    assert_eq!(
        anonymous.status(),
        401,
        "the page sits behind the server password"
    );
    let page = server.get("/").send().await.unwrap();
    assert_eq!(page.status(), 200);
    assert!(page.text().await.unwrap().contains("/web/app.js"));
}

#[tokio::test]
async fn sessions_choose_a_sandbox_at_creation_and_change_it_while_idle() {
    let provider = Provider::start(vec![]).await;
    let server = TestServer::start(&provider, json!({})).await;
    let created = post(
        &server,
        "/api/session",
        json!({"location":{"directory":server.directory},"sandbox":"off"}),
    )
    .await;
    let sid = created["data"]["id"].as_str().unwrap().to_owned();
    let state = json_of(
        server
            .get(&format!("/web/api/session/{sid}/sandbox"))
            .send()
            .await
            .unwrap(),
        200,
    )
    .await;
    assert_eq!(state["preset"], "off");
    assert_eq!(state["root"], true);
    let status = json_of(
        server
            .get(&format!("/web/api/session/{sid}/status"))
            .send()
            .await
            .unwrap(),
        200,
    )
    .await;
    assert_eq!(status, json!({"busy":false,"local":false}));

    for body in [
        json!({"location":{"directory":server.directory},"sandbox":"no-such-preset"}),
        json!({"location":{"directory":server.directory},"sandbox":42}),
        json!({"parentID":sid,"sandbox":"off"}),
    ] {
        let response = server.post("/api/session", &body).send().await.unwrap();
        assert_eq!(response.status(), 400, "{body}");
    }

    let changed = server
        .client
        .put(format!("{}/web/api/session/{sid}/sandbox", server.url))
        .json(&json!({"preset":"off"}))
        .send()
        .await
        .unwrap();
    assert_eq!(json_of(changed, 200).await["preset"], "off");
    let child = post(&server, "/api/session", json!({"parentID":sid})).await;
    let child = child["data"]["id"].as_str().unwrap();
    let refused = server
        .client
        .put(format!("{}/web/api/session/{child}/sandbox", server.url))
        .json(&json!({"preset":"off"}))
        .send()
        .await
        .unwrap();
    assert_eq!(
        refused.status(),
        400,
        "subagents use their parent's sandbox"
    );
}

#[tokio::test]
async fn settings_edit_project_configuration_and_reject_invalid_results() {
    let provider = Provider::start(vec![]).await;
    let server = TestServer::start(&provider, json!({})).await;
    let file = server.directory.join("opencode.json");
    let patch = |set: Value| {
        server
            .client
            .patch(format!("{}/web/api/config", server.url))
            .json(&json!({"scope":"project","directory":server.directory,"set":set}))
            .send()
    };

    let saved = json_of(
        patch(json!({"small_model":"mock/test-model","compaction.prune":true}))
            .await
            .unwrap(),
        200,
    )
    .await;
    assert_eq!(saved["effective"]["small_model"], "mock/test-model");
    assert_eq!(saved["effective"]["compaction"]["prune"], true);
    let written: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    assert_eq!(written["compaction"], json!({"prune":true}));

    let before = std::fs::read_to_string(&file).unwrap();
    for set in [
        json!({"sandbox":"no-such-preset"}),
        json!({"sandbox":{"other":1}}),
    ] {
        assert_eq!(patch(set).await.unwrap().status(), 400);
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            before,
            "invalid settings are rolled back"
        );
    }
    assert_eq!(
        patch(json!({"provider":{}})).await.unwrap().status(),
        400,
        "only form fields are editable"
    );

    std::fs::write(
        &file,
        "{\n  // a comment\n  \"small_model\": \"mock/test-model\"\n}\n",
    )
    .unwrap();
    assert_eq!(
        patch(json!({"model":"mock/test-model"}))
            .await
            .unwrap()
            .status(),
        409,
        "JSONC is not rewritten"
    );
    let snapshot = json_of(
        server
            .get(&format!(
                "/web/api/config?directory={}",
                query(&server.directory)
            ))
            .send()
            .await
            .unwrap(),
        200,
    )
    .await;
    assert_eq!(snapshot["files"]["project"]["editable"], false);
    assert_eq!(snapshot["effective"]["small_model"], "mock/test-model");
}
