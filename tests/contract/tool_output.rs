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

#[tokio::test]
#[cfg(unix)]
async fn background_shell_commands_return_a_pid_and_keep_running_after_the_call() {
    let provider = Provider::start(vec![
        tool_script(
            "bash",
            json!({
                "command": "for i in 1 2 3 4 5 6 7 8; do echo tick $i; sleep 0.25; done",
                "background": true
            }),
            "call_bg",
        ),
        text_script("Done", ""),
    ])
    .await;
    let server = TestServer::start(&provider, json!({})).await;
    let id = server.session(allow()).await;
    server
        .post(
            &format!("/session/{id}/message"),
            &prompt("msg_bg", "Count in the background"),
        )
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let messages = server.messages(&id).await;
    let parts: Vec<_> = messages
        .iter()
        .flat_map(|message| message["parts"].as_array().unwrap())
        .filter(|part| part["type"] == "tool")
        .collect();
    assert_eq!(parts.len(), 1);
    assert_eq!(parts[0]["state"]["status"], "completed", "{:#}", parts[0]);
    let pid = parts[0]["state"]["metadata"]["pid"]
        .as_i64()
        .expect("background tool reports its pid");
    let log = PathBuf::from(
        parts[0]["state"]["metadata"]["outputPath"]
            .as_str()
            .expect("background tool reports its log path"),
    );
    assert!(
        parts[0]["state"]["output"]
            .as_str()
            .unwrap()
            .contains("background"),
        "{:#}",
        parts[0]
    );
    // The turn finished long before the job did, and the job kept writing.
    for _ in 0..100 {
        if std::fs::read_to_string(&log).is_ok_and(|text| text.contains("tick 8")) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let text = std::fs::read_to_string(&log).unwrap();
    assert!(text.contains("tick 8"), "{text}");
    std::process::Command::new("kill")
        .arg(pid.to_string())
        .status()
        .unwrap();
    for _ in 0..40 {
        if !Path::new(&format!("/proc/{pid}")).exists() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("background job {pid} ignored its termination signal");
}

#[tokio::test]
#[cfg(unix)]
async fn a_timed_out_shell_command_keeps_the_output_it_printed_first() {
    let mut provider = Provider::start(vec![
        tool_script(
            "bash",
            json!({"command":"echo BEFORE_LIMIT; sleep 30", "timeout":700}),
            "call_timeout",
        ),
        text_script("Done", ""),
    ])
    .await;
    let server = TestServer::start(&provider, json!({})).await;
    let id = server.session(allow()).await;
    server
        .post(
            &format!("/session/{id}/message"),
            &prompt("msg_timeout", "Run a command that overruns its timeout"),
        )
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let messages = server.messages(&id).await;
    let parts: Vec<_> = messages
        .iter()
        .flat_map(|message| message["parts"].as_array().unwrap())
        .filter(|part| part["type"] == "tool")
        .collect();
    assert_eq!(parts.len(), 1);
    assert_eq!(parts[0]["state"]["status"], "completed", "{:#}", parts[0]);
    assert_eq!(parts[0]["state"]["metadata"]["timeout"], true);
    let output = parts[0]["state"]["output"].as_str().unwrap();
    assert!(output.contains("BEFORE_LIMIT"), "{output}");
    assert!(output.contains("exceeding timeout 700 ms"), "{output}");
    // The model sees the captured output, not just a bare failure.
    loop {
        let request = provider.request().await;
        let Some(tool) = request["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|message| message["role"] == "tool")
        else {
            continue;
        };
        let content = tool["content"].as_str().unwrap();
        assert!(content.contains("BEFORE_LIMIT"), "{content}");
        assert!(content.contains("exceeding timeout 700 ms"), "{content}");
        break;
    }
}

#[tokio::test]
#[cfg(target_os = "linux")]
async fn aborting_a_session_stops_the_background_jobs_it_started() {
    let provider = Provider::start(vec![
        tool_script(
            "bash",
            json!({"command":"sleep 300", "background":true}),
            "call_bg_abort",
        ),
        text_script("Done", ""),
    ])
    .await;
    let server = TestServer::start(&provider, json!({})).await;
    let id = server.session(allow()).await;
    server
        .post(
            &format!("/session/{id}/message"),
            &prompt("msg_bg_abort", "Start a long background job"),
        )
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let messages = server.messages(&id).await;
    let parts: Vec<_> = messages
        .iter()
        .flat_map(|message| message["parts"].as_array().unwrap())
        .filter(|part| part["type"] == "tool")
        .collect();
    let pid = parts[0]["state"]["metadata"]["pid"].as_i64().unwrap();
    assert!(Path::new(&format!("/proc/{pid}")).exists());
    server
        .post(&format!("/session/{id}/abort"), &json!({}))
        .send()
        .await
        .unwrap();
    for _ in 0..40 {
        if !Path::new(&format!("/proc/{pid}")).exists() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("background job {pid} survived the session abort");
}

#[tokio::test]
async fn malformed_optional_shell_fields_fail_without_effects_and_remain_visible() {
    let input = json!({"command":"printf SHOULD_NOT_RUN > malformed-effect", "background":"false"});
    let mut provider = Provider::start(vec![
        tool_script("bash", input.clone(), "malformed-shell"),
        text_script("Correct the tool input", ""),
    ])
    .await;
    let server = TestServer::start(&provider, json!({})).await;
    let sid = server.session(allow()).await;
    server
        .post(
            &format!("/session/{sid}/message"),
            &prompt("msg_malformed_shell", "Run command"),
        )
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    assert!(!server.directory.join("malformed-effect").exists());
    let messages = server.messages(&sid).await;
    let tool = messages
        .iter()
        .flat_map(|message| message["parts"].as_array().unwrap())
        .find(|part| part["callID"] == "malformed-shell")
        .unwrap();
    assert_eq!(tool["state"]["status"], "error");
    assert_eq!(tool["state"]["input"], input);
    provider.request().await;
    let continued = provider.request().await;
    assert!(continued["messages"]
        .as_array()
        .unwrap()
        .iter()
        .any(|message| message["role"] == "tool"
            && message["content"]
                .as_str()
                .is_some_and(|text| text.contains("boolean"))));
}
