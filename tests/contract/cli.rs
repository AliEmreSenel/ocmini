use super::*;
use tokio::io::AsyncWriteExt;

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
    assert_eq!(health["version"], "1.19.3-ocmini.0.3.1");
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
            "1.19.3-ocmini.0.3.1"
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

#[tokio::test]
async fn run_command_prints_the_answer_and_continues_the_session() {
    let mut provider = Provider::start(vec![
        text_script("first answer", ""),
        text_script("second answer", ""),
    ])
    .await;
    let server = TestServer::start(&provider, json!({})).await;

    let output = configured_command(server.root.path(), &server.directory)
        .args(["run", "--auto", "-m", "mock/test-model", "first prompt"])
        .stdin(Stdio::null())
        .output()
        .await
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    eprintln!(
        "STDOUT:\n{stdout}\nSTDERR:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout.contains("> build · mock/test-model"), "{stdout}");
    assert!(stdout.contains("first answer"), "{stdout}");
    let request = provider.request().await;
    assert!(
        request["messages"].to_string().contains("first prompt"),
        "{request}"
    );

    // -c resumes the latest session in this directory: the second request
    // carries the first exchange as history.
    let output = configured_command(server.root.path(), &server.directory)
        .args([
            "run",
            "--auto",
            "-m",
            "mock/test-model",
            "-c",
            "second prompt",
        ])
        .stdin(Stdio::null())
        .output()
        .await
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("second answer"),
        "stdout: {stdout}"
    );
    let request = provider.request().await;
    let history = request["messages"].to_string();
    assert!(history.contains("first prompt"), "{history}");
    assert!(history.contains("first answer"), "{history}");
    assert!(history.contains("second prompt"), "{history}");
}

#[tokio::test]
async fn run_json_format_emits_event_lines() {
    let provider = Provider::start(vec![Script::ok(vec![
        json!({"choices": [{"index": 0, "delta": {"role": "assistant", "reasoning_content": "deep thought"}, "finish_reason": null}]}),
        json!({"choices": [{"index": 0, "delta": {"content": "final answer"}, "finish_reason": null}]}),
        json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}], "usage": usage()}),
    ])])
    .await;
    let server = TestServer::start(&provider, json!({})).await;

    let output = configured_command(server.root.path(), &server.directory)
        .args([
            "run",
            "--auto",
            "-m",
            "mock/test-model",
            "--format",
            "json",
            "--thinking",
            "think about it",
        ])
        .stdin(Stdio::null())
        .output()
        .await
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let events: Vec<Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    let kinds: Vec<&str> = events
        .iter()
        .filter_map(|event| event["type"].as_str())
        .collect();
    for expected in ["step_start", "reasoning", "text", "step_finish"] {
        assert!(kinds.contains(&expected), "kinds: {kinds:?}");
    }
    let text = events.iter().find(|event| event["type"] == "text").unwrap();
    assert_eq!(text["text"], "final answer");
    let reasoning = events
        .iter()
        .find(|event| event["type"] == "reasoning")
        .unwrap();
    assert_eq!(reasoning["text"], "deep thought");
}

#[tokio::test]
async fn run_command_appends_piped_stdin_to_the_prompt() {
    let mut provider = Provider::start(vec![text_script("ok", "")]).await;
    let server = TestServer::start(&provider, json!({})).await;

    let mut command = configured_command(server.root.path(), &server.directory);
    let mut child = command
        .args(["run", "--auto", "-m", "mock/test-model", "head line"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    {
        let mut stdin = child.stdin.take().unwrap();
        stdin.write_all(b"piped tail\n").await.unwrap();
    }
    let output = child.wait_with_output().await.unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let request = provider.request().await;
    let text = request["messages"].to_string();
    assert!(text.contains("head line"), "{text}");
    assert!(text.contains("piped tail"), "{text}");
}
