use super::*;

#[tokio::test]
async fn startup_auth_and_cli_inventory_match_t3() {
    let provider = Provider::start(vec![]).await;
    let server = TestServer::start(&provider, json!({})).await;
    assert!(!server.url.ends_with(":0"));
    let health: Value = server
        .get("/global/health")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(health["healthy"], true);
    assert_eq!(health["version"], "1.19.2-ocmini.0.2.0");
    assert_eq!(server.get("/api/info").send().await.unwrap().status(), 404);
    for path in ["/global/health", "/api/info", "/event", "/session"] {
        assert_eq!(
            reqwest::get(format!("{}{path}", server.url))
                .await
                .unwrap()
                .status(),
            401,
            "{path}"
        );
    }
    let inventory: Value = server
        .get("/provider")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(inventory["connected"], json!(["mock"]));
    assert_eq!(
        inventory["all"][0]["models"]["test-model"]["id"],
        "test-model"
    );
    assert!(inventory["all"][0]["models"]["test-model"]["variants"]["high"].is_object());
    let output = configured_command(server.root.path(), &server.directory)
        .args(["models", "--verbose"])
        .output()
        .await
        .unwrap();
    assert!(output.status.success());
    let output = String::from_utf8(output.stdout).unwrap();
    let mut lines = output.lines();
    assert_eq!(lines.next(), Some("mock/test-model"));
    let model: Value = serde_json::from_str(lines.next().unwrap()).unwrap();
    assert_eq!(model["id"], "test-model");
    assert_eq!(model["limit"]["context"], 32000);
    let output = configured_command(server.root.path(), &server.directory)
        .args(["agent", "list"])
        .output()
        .await
        .unwrap();
    assert!(output.status.success());
    let output = String::from_utf8(output.stdout).unwrap();
    assert!(output.contains("reviewer (primary)"));
    for pair in output.lines().collect::<Vec<_>>().chunks(2) {
        assert!(pair[0].ends_with(')'));
        assert!(serde_json::from_str::<Value>(pair[1]).unwrap().is_array());
    }
    let output = configured_command(server.root.path(), &server.directory)
        .args(["serve", "--port", "garbage"])
        .output()
        .await
        .unwrap();
    assert!(!output.status.success());
}

#[tokio::test]
async fn cli_help_version_and_invalid_arguments_do_not_start_the_runtime() {
    for arguments in [
        vec!["--help"],
        vec!["tui", "--help"],
        vec!["serve", "--help"],
        vec!["models", "--help"],
        vec!["session", "--help"],
        vec!["session", "list", "--help"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_ocmini"))
            .args(arguments)
            .output()
            .await
            .unwrap();
        assert!(output.status.success());
        assert!(String::from_utf8(output.stdout).unwrap().contains("Usage:"));
    }
    for arguments in [
        ["--version"].as_slice(),
        ["-V"].as_slice(),
        ["version"].as_slice(),
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_ocmini"))
            .args(arguments)
            .output()
            .await
            .unwrap();
        assert!(output.status.success());
        assert_eq!(
            String::from_utf8(output.stdout).unwrap().trim(),
            "1.19.2-ocmini.0.2.0"
        );
    }
    for arguments in [
        vec!["serve", "--port=65536"],
        vec!["serve", "--hostname="],
        vec!["serve", "--unknown"],
        vec!["tui", "--model"],
        vec!["--model", "invalid"],
        vec!["--continue", "--session", "ses_example"],
        vec!["tui", "--continue", "--session", "ses_example"],
        vec!["--resume", "--session", "ses_example"],
        vec!["tui", "--resume", "--continue"],
        vec!["models", "a", "b"],
        vec!["--model", "local/test", "serve"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_ocmini"))
            .args(&arguments)
            .output()
            .await
            .unwrap();
        assert!(!output.status.success(), "{arguments:?}");
        assert!(!output.stderr.is_empty(), "{arguments:?}");
    }
}

#[tokio::test]
async fn interactive_session_picker_reports_the_missing_terminal_without_touching_sessions() {
    let provider = Provider::start(vec![]).await;
    let server = TestServer::start(&provider, json!({})).await;
    let id = server.session(allow()).await;
    // Piped test runs have no TTY: --resume must fail cleanly and list must stay intact.
    let output = configured_command(server.root.path(), &server.directory)
        .args(["--resume"])
        .output()
        .await
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("needs a terminal"));
    assert!(server
        .get(&format!("/session/{id}"))
        .send()
        .await
        .unwrap()
        .status()
        .is_success());
}

#[tokio::test]
async fn session_listing_and_resume_errors_work_without_a_terminal() {
    let provider = Provider::start(vec![]).await;
    let server = TestServer::start(&provider, json!({})).await;
    let id = server.session(allow()).await;
    for arguments in [
        vec!["session", "list"],
        vec!["session", "list", "--directory", "."],
    ] {
        let output = configured_command(server.root.path(), &server.directory)
            .args(arguments)
            .output()
            .await
            .unwrap();
        assert!(output.status.success(), "{:?}", output);
        assert!(String::from_utf8(output.stdout).unwrap().contains(&id));
    }
    for arguments in [
        vec!["--session", "missing"],
        vec!["tui", "--session", "missing"],
    ] {
        let output = configured_command(server.root.path(), &server.directory)
            .args(arguments)
            .output()
            .await
            .unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8(output.stderr)
            .unwrap()
            .contains("Session not found: missing"));
    }
    assert_eq!(
        server
            .get("/session")
            .send()
            .await
            .unwrap()
            .json::<Vec<Value>>()
            .await
            .unwrap()
            .len(),
        1
    );
}
