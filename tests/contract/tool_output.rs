use super::*;

#[tokio::test]
async fn explicit_shell_timeouts_and_clamped_web_timeouts_execute_normally() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/", listener.local_addr().unwrap());
    let web = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new().route("/", axum::routing::get(|| async { "WEB_OK" })),
        )
        .await
        .unwrap();
    });
    let provider = Provider::start(vec![
        tool_script(
            "bash",
            json!({"command":"printf BASH_OK", "timeout":3_600_000}),
            "long-bash",
        ),
        tool_script(
            "webfetch",
            json!({"url":url,"format":"text","timeout":3600}),
            "long-web",
        ),
        text_script("Done", ""),
    ])
    .await;
    let server = TestServer::start(&provider, json!({})).await;
    let id = server.session(allow()).await;
    server
        .post(
            &format!("/session/{id}/message"),
            &prompt("msg_timeouts", "Run tools with explicit long timeouts"),
        )
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let messages = server.messages(&id).await;
    let tools: Vec<_> = messages
        .iter()
        .flat_map(|message| message["parts"].as_array().unwrap())
        .filter(|part| part["type"] == "tool")
        .collect();
    assert_eq!(tools.len(), 2);
    for tool in &tools {
        assert_eq!(tool["state"]["status"], "completed", "{tool}");
    }
    assert_eq!(tools[0]["state"]["output"], "BASH_OK");
    assert_eq!(tools[1]["state"]["output"], "WEB_OK");
    web.abort();
}

#[tokio::test]
async fn tool_output_expiry_is_opt_in_and_rejects_invalid_settings() {
    let provider = Provider::start(vec![]).await;
    let server = TestServer::start(&provider, json!({})).await;
    let directory = server.root.path().join(".local/share/ocmini/tool-output");
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join("tool_expired.txt");
    std::fs::write(&path, "saved result").unwrap();
    std::fs::File::open(&path)
        .unwrap()
        .set_modified(std::time::SystemTime::now() - Duration::from_secs(8 * 24 * 60 * 60))
        .unwrap();
    for retention in ["invalid", "-1", "0", "7"] {
        let mut child = configured_command(server.root.path(), &server.directory)
            .env("OCMINI_TOOL_OUTPUT_RETENTION_DAYS", retention)
            .args(["serve", "--port=0"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        if matches!(retention, "invalid" | "-1") {
            let output = tokio::time::timeout(Duration::from_secs(3), child.wait_with_output())
                .await
                .unwrap()
                .unwrap();
            assert!(!output.status.success());
            assert!(String::from_utf8_lossy(&output.stderr)
                .contains("OCMINI_TOOL_OUTPUT_RETENTION_DAYS"));
        } else {
            let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
            let banner = tokio::time::timeout(Duration::from_secs(3), lines.next_line())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            assert!(banner.starts_with("opencode server listening on "));
            child.kill().await.unwrap();
            child.wait().await.unwrap();
        }
        assert_eq!(path.exists(), retention != "7");
    }
}

#[tokio::test]
async fn large_shell_and_web_results_save_full_output_and_replay_only_the_preview() {
    let text = format!(
        "BEGIN\n{}MIDDLE_SENTINEL\n{}END\n",
        "ordinary line\n".repeat(20_000),
        "ordinary line\n".repeat(20_000)
    );
    let served = text.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/large", listener.local_addr().unwrap());
    let router = Router::new().route(
        "/large",
        axum::routing::get(move || {
            let text = served.clone();
            async move { text }
        }),
    );
    let web = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });

    for (name, input) in [
        ("bash", json!({"command":"cat input.txt"})),
        ("webfetch", json!({"url":url,"format":"text"})),
    ] {
        let mut provider = Provider::start(vec![
            tool_script(name, input, "large-result"),
            text_script("Saved", ""),
            text_script("Continued", ""),
        ])
        .await;
        let mut server = TestServer::start(&provider, json!({})).await;
        std::fs::write(server.directory.join("input.txt"), &text).unwrap();
        let id = server.session(allow()).await;
        server
            .post(
                &format!("/session/{id}/message"),
                &prompt("msg_large", "Run the tool"),
            )
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
        provider.request().await;
        let request = provider.request().await;
        let sent = request["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|message| message["role"] == "tool")
            .unwrap()["content"]
            .as_str()
            .unwrap();
        assert!(sent.starts_with("BEGIN"));
        assert!(sent.contains("END"));
        assert!(sent.contains("Full tool output saved"));
        assert!(!sent.contains("MIDDLE_SENTINEL"));
        assert!(sent.len() < 52 * 1024);
        let messages = server.messages(&id).await;
        let tool = messages
            .iter()
            .flat_map(|message| message["parts"].as_array().unwrap())
            .find(|part| part["type"] == "tool")
            .unwrap();
        assert_eq!(tool["state"]["metadata"]["truncated"], true);
        assert_eq!(tool["state"]["output"], sent);
        let path = PathBuf::from(tool["state"]["metadata"]["outputPath"].as_str().unwrap());
        assert!(path.starts_with(server.root.path().join(".local/share/ocmini/tool-output")));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), text);

        // A resumed conversation must still be able to read the full result
        // even when it has been saved for longer than the former seven days.
        std::fs::File::open(&path)
            .unwrap()
            .set_modified(std::time::SystemTime::now() - Duration::from_secs(8 * 24 * 60 * 60))
            .unwrap();
        server.restart().await;
        server
            .post(
                &format!("/session/{id}/message"),
                &prompt("msg_after_restart", "Continue"),
            )
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
        let request = provider.request().await;
        let sent = request.to_string();
        assert!(sent.contains(&path.to_string_lossy().into_owned()));
        assert!(!sent.contains("MIDDLE_SENTINEL"));
        assert!(sent.len() < 65 * 1024);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
        assert_eq!(
            std::fs::read_dir(path.parent().unwrap()).unwrap().count(),
            1
        );
    }
    web.abort();
}
