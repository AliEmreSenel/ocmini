use super::*;
#[tokio::test]
async fn default_preset_routes_agent_tools_and_runner_failure_is_terminal() {
    let provider = Provider::start(vec![
        tool_script(
            "bash",
            json!({"command":"printf SANDBOX_ROUTED > routed.txt; printf SANDBOX_ROUTED"}),
            "sandbox-one",
        ),
        text_script("Done", ""),
        tool_script(
            "bash",
            json!({"command":"kill -KILL $PPID"}),
            "sandbox-crash",
        ),
        text_script("Runner ended", ""),
        tool_script(
            "bash",
            json!({"command":"touch must-not-replay"}),
            "sandbox-after-crash",
        ),
        text_script("No replay", ""),
    ])
    .await;
    let server =
        TestServer::start(&provider, json!({"sandbox":{"preset":"workspace-write"}})).await;
    let response = server
        .post(
            "/session",
            &json!({"permission":[{"permission":"*","pattern":"*","action":"allow"}]}),
        )
        .send()
        .await
        .unwrap();
    let status = response.status();
    let text = response.text().await.unwrap();
    assert_eq!(status, 200, "{text}");
    let sid = serde_json::from_str::<Value>(&text).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    for (n, call) in [
        (1, "sandbox-one"),
        (2, "sandbox-crash"),
        (3, "sandbox-after-crash"),
    ] {
        let response = server
            .post(
                &format!("/session/{sid}/message"),
                &prompt(&format!("msg_sandbox_{n}"), "Run tool"),
            )
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200, "{}", response.text().await.unwrap());
        let messages = server.messages(&sid).await;
        let part = messages
            .iter()
            .flat_map(|m| m["parts"].as_array().unwrap())
            .find(|p| p["callID"] == call)
            .unwrap();
        if n == 1 {
            assert_eq!(part["state"]["status"], "completed", "{part}");
            assert_eq!(
                std::fs::read_to_string(server.directory.join("routed.txt")).unwrap(),
                "SANDBOX_ROUTED"
            );
        } else {
            assert_eq!(part["state"]["status"], "error", "{part}");
        }
    }
    assert!(!server.directory.join("must-not-replay").exists());
}

#[tokio::test]
async fn chained_edits_keep_model_order_in_a_runner() {
    let provider = Provider::start(vec![
        Script::ok(vec![
            json!({"choices":[{"index":0,"delta":{"tool_calls":[
                {"index":0,"id":"edit_first","type":"function","function":{"name":"edit","arguments":json!({"filePath":"ordered.txt","oldString":"start","newString":"middle"}).to_string()}},
                {"index":1,"id":"edit_second","type":"function","function":{"name":"edit","arguments":json!({"filePath":"ordered.txt","oldString":"middle","newString":"finish"}).to_string()}}
            ]},"finish_reason":null}]}),
            json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}],"usage":usage()}),
        ]),
        text_script("Done", ""),
    ]).await;
    let server =
        TestServer::start(&provider, json!({"sandbox":{"preset":"workspace-write"}})).await;
    std::fs::write(server.directory.join("ordered.txt"), "start\n").unwrap();
    let sid = server.session(allow()).await;
    server
        .post(
            &format!("/session/{sid}/message"),
            &prompt("msg_ordered_edits", "Edit twice"),
        )
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(server.directory.join("ordered.txt")).unwrap(),
        "finish\n"
    );
    let messages = server.messages(&sid).await;
    for id in ["edit_first", "edit_second"] {
        let part = messages
            .iter()
            .flat_map(|m| m["parts"].as_array().unwrap())
            .find(|p| p["callID"] == id)
            .unwrap();
        assert_eq!(part["state"]["status"], "completed", "{part}");
    }
}

#[tokio::test]
async fn concurrent_session_runners_coordinate_edits_to_the_same_file() {
    let (first_tx, first_rx) = oneshot::channel();
    let (second_tx, second_rx) = oneshot::channel();
    let mut first = tool_script(
        "edit",
        json!({"filePath":"shared.txt","oldString":"ALPHA","newString":"alpha"}),
        "edit_alpha",
    );
    let mut second = tool_script(
        "edit",
        json!({"filePath":"shared.txt","oldString":"BETA","newString":"beta"}),
        "edit_beta",
    );
    first.gate = Some(first_rx);
    second.gate = Some(second_rx);
    let mut provider = Provider::start(vec![
        first,
        second,
        text_script("Done", ""),
        text_script("Done", ""),
    ])
    .await;
    let server =
        TestServer::start(&provider, json!({"sandbox":{"preset":"workspace-write"}})).await;
    std::fs::write(server.directory.join("shared.txt"), "ALPHA BETA\n").unwrap();
    let a = server.session(allow()).await;
    let b = server.session(allow()).await;
    let requests = async {
        tokio::join!(
            server
                .post(
                    &format!("/session/{a}/message"),
                    &prompt("msg_edit_a", "Edit alpha")
                )
                .send(),
            server
                .post(
                    &format!("/session/{b}/message"),
                    &prompt("msg_edit_b", "Edit beta")
                )
                .send()
        )
    };
    let release = async {
        provider.request().await;
        provider.request().await;
        first_tx.send(()).unwrap();
        second_tx.send(()).unwrap();
    };
    let ((a_response, b_response), ()) = tokio::join!(requests, release);
    a_response.unwrap().error_for_status().unwrap();
    b_response.unwrap().error_for_status().unwrap();
    assert_eq!(
        std::fs::read_to_string(server.directory.join("shared.txt")).unwrap(),
        "alpha beta\n"
    );
    for sid in [a, b] {
        let messages = server.messages(&sid).await;
        let parts: Vec<_> = messages
            .iter()
            .flat_map(|m| m["parts"].as_array().unwrap())
            .filter(|p| p["type"] == "tool")
            .collect();
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0]["state"]["status"], "completed", "{}", parts[0]);
    }
}

#[tokio::test]
async fn presets_follow_each_session_workspace_and_discover_current_user_toolchains() {
    let provider = Provider::start(vec![
        tool_script("bash", json!({"command":"printf FIRST > dynamic.txt"}), "first_workspace"),
        text_script("Done", ""),
        tool_script("bash", json!({"command":"pwd; dynamic-user-tool; printf SECOND > dynamic.txt; cat '../project café %/startup-secret' 2>/dev/null || printf STARTUP_WORKSPACE_BLOCKED"}), "second_workspace"),
        text_script("Done", ""),
    ]).await;
    let server =
        TestServer::start(&provider, json!({"sandbox":{"preset":"workspace-write"}})).await;
    std::fs::write(
        server.directory.join("startup-secret"),
        "FIRST_WORKSPACE_SECRET",
    )
    .unwrap();
    let first = server.session(allow()).await;
    server
        .post(
            &format!("/session/{first}/message"),
            &prompt("msg_dynamic_first", "Write in the first workspace"),
        )
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(server.directory.join("dynamic.txt")).unwrap(),
        "FIRST"
    );

    // Neither this project nor this user's toolchain existed at installation.
    // Literal brackets and ${home} in the project name must remain path bytes.
    let other = server
        .root
        .path()
        .join("second workspace [literal] ${home}");
    std::fs::create_dir(&other).unwrap();
    let bin = server.root.path().join(".cargo/bin");
    std::fs::create_dir_all(&bin).unwrap();
    let tool = bin.join("dynamic-user-tool");
    std::fs::write(&tool, "#!/bin/sh\nprintf '%s\\n' CURRENT_USER_TOOLCHAIN\n").unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
    let response = server
        .post("/session", &json!({"permission":allow()}))
        .header("x-opencode-directory", encode_directory(&other))
        .send()
        .await
        .unwrap();
    let status = response.status();
    let response: Value = response.json().await.unwrap();
    assert_eq!(status, 200, "{response}");
    let second = response["id"].as_str().unwrap();
    server
        .post(
            &format!("/session/{second}/message"),
            &prompt("msg_dynamic_second", "Use the second workspace"),
        )
        .header("x-opencode-directory", encode_directory(&other))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(other.join("dynamic.txt")).unwrap(),
        "SECOND"
    );
    assert_eq!(
        std::fs::read_to_string(server.directory.join("dynamic.txt")).unwrap(),
        "FIRST"
    );
    let messages: Vec<Value> = server
        .get(&format!("/session/{second}/message"))
        .header("x-opencode-directory", encode_directory(&other))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let part = messages
        .iter()
        .flat_map(|m| m["parts"].as_array().unwrap())
        .find(|p| p["callID"] == "second_workspace")
        .unwrap();
    assert_eq!(part["state"]["status"], "completed", "{part}");
    let output = part["state"]["output"].as_str().unwrap();
    assert!(output.contains(other.to_str().unwrap()), "{output}");
    assert!(output.contains("CURRENT_USER_TOOLCHAIN"), "{output}");
    assert!(output.contains("STARTUP_WORKSPACE_BLOCKED"), "{output}");
    assert!(!output.contains("FIRST_WORKSPACE_SECRET"), "{output}");
}
