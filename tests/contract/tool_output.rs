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
            json!({"command":shell_command("printf BASH_OK", "echo BASH_OK"), "timeout":3_600_000}),
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
    assert_eq!(
        tools[0]["state"]["output"].as_str().unwrap().trim_end(),
        "BASH_OK"
    );
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
    std::fs::File::options()
        .write(true)
        .open(&path)
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
        (
            "bash",
            json!({"command":shell_command("cat input.txt", "type input.txt")}),
        ),
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
        std::fs::File::options()
            .write(true)
            .open(&path)
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
        let messages = request["messages"].to_string();
        let replayed = request["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|message| {
                message["role"] == "tool"
                    && message["content"]
                        .as_str()
                        .is_some_and(|content| content.contains(path.to_str().unwrap()))
            })
            .expect("replayed tool preview");
        assert!(replayed["content"].as_str().unwrap().len() < 52 * 1024);
        assert!(!messages.contains("MIDDLE_SENTINEL"));
        assert!(messages.len() < 65 * 1024);
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

#[tokio::test]
async fn webfetch_rechecks_permissions_on_redirects() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let secret_hit = Arc::new(AtomicBool::new(false));
    let gated_hit = Arc::new(AtomicBool::new(false));
    let hit = secret_hit.clone();
    let gated = gated_hit.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let router = Router::new()
        .route(
            "/to-denied",
            axum::routing::get(|| async { axum::response::Redirect::temporary("/secret") }),
        )
        .route(
            "/to-ask",
            axum::routing::get(|| async { axum::response::Redirect::temporary("/gated") }),
        )
        .route(
            "/to-allowed",
            axum::routing::get(|| async { axum::response::Redirect::temporary("/public#frag") }),
        )
        .route(
            "/secret",
            axum::routing::get(move || {
                let hit = hit.clone();
                async move {
                    hit.store(true, Ordering::SeqCst);
                    "TOP_SECRET"
                }
            }),
        )
        .route(
            "/gated",
            axum::routing::get(move || {
                let gated = gated.clone();
                async move {
                    gated.store(true, Ordering::SeqCst);
                    "GATED_SECRET"
                }
            }),
        )
        .route("/public", axum::routing::get(|| async { "PUBLIC_OK" }));
    let web = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let mut provider = Provider::start(vec![
        tool_script(
            "webfetch",
            json!({"url":format!("{base}/to-denied"),"format":"text"}),
            "denied_redirect",
        ),
        tool_script(
            "webfetch",
            json!({"url":format!("{base}/to-ask"),"format":"text"}),
            "ask_redirect",
        ),
        tool_script(
            "webfetch",
            json!({"url":format!("{base}/to-allowed"),"format":"text"}),
            "allowed_redirect",
        ),
        text_script("Done", ""),
    ])
    .await;
    let server = TestServer::start(&provider, json!({})).await;
    let id = server
        .session(json!([
            {"permission":"*","pattern":"*","action":"allow"},
            {"permission":"webfetch","pattern":"*/secret*","action":"deny"},
            {"permission":"webfetch","pattern":"*/gated*","action":"ask"}
        ]))
        .await;
    let mut events = server.events().await;
    server
        .post(
            &format!("/session/{id}/prompt_async"),
            &prompt("msg_redirect", "Fetch all"),
        )
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let asked = events.through("permission.asked").await.pop().unwrap();
    assert_eq!(asked["properties"]["permission"], "webfetch");
    assert_eq!(
        asked["properties"]["patterns"],
        json!([format!("{base}/gated")])
    );
    let req_id = asked["properties"]["id"].as_str().unwrap();
    server
        .post(
            &format!("/permission/{req_id}/reply"),
            &json!({"reply":"reject"}),
        )
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    events.through("session.idle").await;
    let messages = server.messages(&id).await;
    let parts: Vec<_> = messages
        .iter()
        .flat_map(|m| m["parts"].as_array().unwrap())
        .collect();
    let denied = parts
        .iter()
        .find(|p| p["callID"] == "denied_redirect")
        .unwrap();
    assert_eq!(denied["state"]["status"], "error", "{denied}");
    assert!(denied["state"]["error"]
        .as_str()
        .unwrap()
        .contains("Permission 'webfetch' denied"));
    assert!(!secret_hit.load(Ordering::SeqCst));
    let rejected = parts
        .iter()
        .find(|p| p["callID"] == "ask_redirect")
        .unwrap();
    assert_eq!(rejected["state"]["status"], "error", "{rejected}");
    assert!(rejected["state"]["error"]
        .as_str()
        .unwrap()
        .contains("Permission 'webfetch' denied"));
    assert!(!gated_hit.load(Ordering::SeqCst));
    let allowed = parts
        .iter()
        .find(|p| p["callID"] == "allowed_redirect")
        .unwrap();
    assert_eq!(allowed["state"]["status"], "completed", "{allowed}");
    assert_eq!(allowed["state"]["output"], "PUBLIC_OK");
    let _ = provider.request().await;
    web.abort();
}

#[tokio::test]
#[cfg(unix)]
async fn background_read_polls_session_jobs_and_follows_shell_gating() {
    let mut provider = Provider::start(vec![
        tool_script(
            "background_read",
            json!({"job_id": "job_none", "offset": 0}),
            "call_plan_blocked",
        ),
        text_script("Plan done", ""),
        text_script("Shell disabled done", ""),
        tool_script(
            "bash",
            json!({"command": "printf first; sleep 0.15; printf second; exit 7", "background": true}),
            "call_bg_start",
        ),
        text_script("Started", ""),
    ])
    .await;
    let server = TestServer::start(&provider, json!({})).await;
    let id = server.session(allow()).await;

    // 1. Read-only plan agent must not advertise or execute background_read.
    let mut plan_prompt = prompt("msg_plan_bg", "Inspect background job");
    plan_prompt["agent"] = json!("plan");
    server
        .post(&format!("/session/{id}/message"), &plan_prompt)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let plan_req = provider.request().await;
    let plan_tools: Vec<_> = plan_req["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| t["function"]["name"].as_str())
        .collect();
    assert!(
        !plan_tools.contains(&"background_read"),
        "plan agent must not advertise background_read: {plan_tools:?}"
    );
    let _ = provider.request().await;
    let messages = server.messages(&id).await;
    let blocked = messages
        .iter()
        .flat_map(|m| m["parts"].as_array().unwrap())
        .find(|p| p["callID"] == "call_plan_blocked")
        .unwrap();
    assert_eq!(blocked["state"]["status"], "error", "{blocked}");
    assert!(
        blocked["state"]["error"]
            .as_str()
            .unwrap()
            .contains("disabled for this request"),
        "{blocked}"
    );

    // 2. Disabling shell/bash disables background_read too.
    let mut no_shell = prompt("msg_no_shell_bg", "No shell");
    no_shell["tools"] = json!({"shell": false});
    server
        .post(&format!("/session/{id}/message"), &no_shell)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let no_shell_req = provider.request().await;
    let no_shell_tools: Vec<_> = no_shell_req["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| t["function"]["name"].as_str())
        .collect();
    assert!(
        !no_shell_tools.contains(&"background_read"),
        "disabling shell must hide background_read: {no_shell_tools:?}"
    );

    // 3. Normal build turn launches a background job and polls it across offsets.
    server
        .post(
            &format!("/session/{id}/message"),
            &prompt("msg_bg_launch", "Launch job"),
        )
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let build_req = provider.request().await;
    let build_tools: Vec<_> = build_req["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| t["function"]["name"].as_str())
        .collect();
    assert!(
        build_tools.contains(&"background_read"),
        "build agent should advertise background_read: {build_tools:?}"
    );
    let _ = provider.request().await;
    let messages = server.messages(&id).await;
    let launched = messages
        .iter()
        .flat_map(|m| m["parts"].as_array().unwrap())
        .find(|p| p["callID"] == "call_bg_start")
        .unwrap();
    assert_eq!(launched["state"]["status"], "completed", "{launched}");
    let job_id = launched["state"]["metadata"]["jobID"]
        .as_str()
        .expect("background launch returns jobID")
        .to_owned();

    // Poll first 5 bytes ("first"), then wait for completion and read the rest ("second").
    let mut offset = 0u64;
    let mut collected = String::new();
    let mut observed_exit = None;
    for attempt in 0..40 {
        let call_id = format!("call_bg_poll_{attempt}");
        let limit = if offset == 0 { json!(5) } else { Value::Null };
        let mut args = json!({"job_id": job_id, "offset": offset});
        if !limit.is_null() {
            args["limit"] = limit;
        }
        provider
            .push(tool_script("background_read", args, &call_id))
            .await;
        provider.push(text_script("Polled", "")).await;
        server
            .post(
                &format!("/session/{id}/message"),
                &prompt(&format!("msg_bg_poll_{attempt}"), "Poll job"),
            )
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
        let _ = provider.request().await;
        let _ = provider.request().await;
        let messages = server.messages(&id).await;
        let polled = messages
            .iter()
            .flat_map(|m| m["parts"].as_array().unwrap())
            .find(|p| p["callID"] == call_id)
            .unwrap();
        assert_eq!(polled["state"]["status"], "completed", "{polled}");
        let output = polled["state"]["output"].as_str().unwrap();
        collected.push_str(output.split("\n[background job ").next().unwrap());
        offset = polled["state"]["metadata"]["nextOffset"].as_u64().unwrap();
        if polled["state"]["metadata"]["status"] == "exited" && offset == 11 {
            observed_exit = polled["state"]["metadata"]["exit"].as_i64();
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(collected, "firstsecond");
    assert_eq!(offset, 11);
    assert_eq!(observed_exit, Some(7));
}

#[tokio::test]
#[cfg(unix)]
async fn background_list_and_stop_follow_shell_gating_and_manage_session_jobs() {
    let mut provider = Provider::start(vec![
        tool_script("background_list", json!({}), "call_plan_list"),
        text_script("Plan done", ""),
        text_script("No shell done", ""),
        tool_script(
            "bash",
            json!({"command": "sleep 30", "description": "long sleeper", "background": true}),
            "call_bg_launch",
        ),
        tool_script("background_list", json!({}), "call_bg_list_running"),
        text_script("Listed", ""),
    ])
    .await;
    let server = TestServer::start(&provider, json!({})).await;
    let id = server.session(allow()).await;

    // 1. Read-only plan agent does not advertise or execute background_list/background_stop.
    let mut plan_prompt = prompt("msg_plan_bg_mgmt", "List jobs");
    plan_prompt["agent"] = json!("plan");
    server
        .post(&format!("/session/{id}/message"), &plan_prompt)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let plan_req = provider.request().await;
    let plan_tools: Vec<_> = plan_req["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| t["function"]["name"].as_str())
        .collect();
    assert!(!plan_tools.contains(&"background_list"), "{plan_tools:?}");
    assert!(!plan_tools.contains(&"background_stop"), "{plan_tools:?}");
    let _ = provider.request().await;
    let messages = server.messages(&id).await;
    let blocked = messages
        .iter()
        .flat_map(|m| m["parts"].as_array().unwrap())
        .find(|p| p["callID"] == "call_plan_list")
        .unwrap();
    assert_eq!(blocked["state"]["status"], "error", "{blocked}");

    // 2. Disabling shell/bash hides background_list and background_stop.
    let mut no_shell = prompt("msg_no_shell_bg_mgmt", "No shell");
    no_shell["tools"] = json!({"bash": false});
    server
        .post(&format!("/session/{id}/message"), &no_shell)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let no_shell_req = provider.request().await;
    let no_shell_tools: Vec<_> = no_shell_req["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| t["function"]["name"].as_str())
        .collect();
    assert!(
        !no_shell_tools.contains(&"background_list"),
        "{no_shell_tools:?}"
    );
    assert!(
        !no_shell_tools.contains(&"background_stop"),
        "{no_shell_tools:?}"
    );

    // 3. Build agent launches a background job, lists it, and stops it via background_stop.
    server
        .post(
            &format!("/session/{id}/message"),
            &prompt("msg_bg_launch_mgmt", "Start and list"),
        )
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let build_req = provider.request().await;
    let build_tools: Vec<_> = build_req["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| t["function"]["name"].as_str())
        .collect();
    assert!(build_tools.contains(&"background_list"), "{build_tools:?}");
    assert!(build_tools.contains(&"background_stop"), "{build_tools:?}");
    let _ = provider.request().await;
    let _ = provider.request().await;
    let messages = server.messages(&id).await;
    let listed = messages
        .iter()
        .flat_map(|m| m["parts"].as_array().unwrap())
        .find(|p| p["callID"] == "call_bg_list_running")
        .unwrap();
    assert_eq!(listed["state"]["status"], "completed", "{listed}");
    assert_eq!(listed["state"]["metadata"]["count"], 1, "{listed}");
    let pid = listed["state"]["metadata"]["jobs"][0]["pid"]
        .as_u64()
        .unwrap();
    assert_eq!(listed["state"]["metadata"]["jobs"][0]["status"], "running");
    assert_eq!(
        listed["state"]["metadata"]["jobs"][0]["description"],
        "long sleeper"
    );

    provider
        .push(tool_script(
            "background_stop",
            json!({"pid": pid}),
            "call_bg_stop",
        ))
        .await;
    provider
        .push(tool_script(
            "background_list",
            json!({}),
            "call_bg_list_after",
        ))
        .await;
    provider.push(text_script("Stopped", "")).await;
    server
        .post(
            &format!("/session/{id}/message"),
            &prompt("msg_bg_stop_mgmt", "Stop job"),
        )
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let _ = provider.request().await;
    let _ = provider.request().await;
    let _ = provider.request().await;
    let messages = server.messages(&id).await;
    let stopped = messages
        .iter()
        .flat_map(|m| m["parts"].as_array().unwrap())
        .find(|p| p["callID"] == "call_bg_stop")
        .unwrap();
    assert_eq!(stopped["state"]["status"], "completed", "{stopped}");
    assert_eq!(stopped["state"]["metadata"]["stopped"], true, "{stopped}");
    let after = messages
        .iter()
        .flat_map(|m| m["parts"].as_array().unwrap())
        .find(|p| p["callID"] == "call_bg_list_after")
        .unwrap();
    assert_eq!(
        after["state"]["metadata"]["jobs"][0]["status"], "exited",
        "{after}"
    );
}

#[tokio::test]
async fn todoread_reads_persisted_todos_across_turns_and_in_plan_agent() {
    let todos = json!([
        {"id": "t1", "content": "Ship v2 tools", "status": "in_progress", "priority": "high"},
        {"id": "t2", "content": "Verify tests", "status": "pending", "priority": "medium"}
    ]);
    let mut provider = Provider::start(vec![
        tool_script("todoread", json!({}), "call_todo_empty"),
        tool_script("todowrite", json!({"todos": todos}), "call_todo_write"),
        text_script("Saved todos", ""),
        text_script("Compaction summary", ""),
        tool_script("todoread", json!({}), "call_todo_plan"),
        text_script("Read todos in plan mode", ""),
    ])
    .await;
    let server = TestServer::start(&provider, json!({})).await;
    let id = server.session(allow()).await;

    server
        .post(
            &format!("/session/{id}/message"),
            &prompt("msg_todo_1", "Check and save todos"),
        )
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let _ = provider.request().await;
    let _ = provider.request().await;
    let _ = provider.request().await;

    // Compact history and verify todoread still returns the persisted todos.
    server
        .post(
            &format!("/session/{id}/summarize"),
            &json!({"providerID": "mock", "modelID": "mock-model"}),
        )
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let _ = provider.request().await;

    let mut plan_prompt = prompt("msg_todo_2", "Read todos in plan agent");
    plan_prompt["agent"] = json!("plan");
    server
        .post(&format!("/session/{id}/message"), &plan_prompt)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let plan_req = provider.request().await;
    let plan_tools: Vec<_> = plan_req["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| t["function"]["name"].as_str())
        .collect();
    assert!(plan_tools.contains(&"todoread"), "{plan_tools:?}");
    let _ = provider.request().await;

    let messages = server.messages(&id).await;
    let find_part = |call_id: &str| {
        messages
            .iter()
            .flat_map(|m| m["parts"].as_array().unwrap())
            .find(|p| p["callID"] == call_id)
            .unwrap()
            .clone()
    };
    let empty = find_part("call_todo_empty");
    assert_eq!(empty["state"]["status"], "completed", "{empty}");
    assert_eq!(empty["state"]["metadata"]["todos"], json!([]));
    assert_eq!(empty["state"]["output"], "[]");

    let read_back = find_part("call_todo_plan");
    assert_eq!(read_back["state"]["status"], "completed", "{read_back}");
    assert_eq!(read_back["state"]["metadata"]["todos"], todos);
    assert!(read_back["state"]["output"]
        .as_str()
        .unwrap()
        .contains("Ship v2 tools"));
}

#[tokio::test]
async fn edit_diff_metadata_and_read_path_alias_work_end_to_end() {
    let mut provider = Provider::start(vec![
        tool_script(
            "edit",
            json!({"filePath": "sub/demo.txt", "oldString": "before", "newString": "after"}),
            "call_edit_diff",
        ),
        tool_script("read", json!({"path": "sub/demo.txt"}), "call_read_alias"),
        text_script("Done", ""),
    ])
    .await;
    let server = TestServer::start(&provider, json!({})).await;
    let sub = std::path::Path::new(&server.directory).join("sub");
    std::fs::create_dir_all(&sub).unwrap();
    std::fs::write(sub.join("demo.txt"), "top\nbefore\nbottom\n").unwrap();
    std::fs::write(sub.join("AGENTS.md"), "NESTED_ALIAS_RULE").unwrap();

    let id = server.session(allow()).await;
    server
        .post(
            &format!("/session/{id}/message"),
            &prompt("msg_edit_read", "Edit and read via path alias"),
        )
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let _ = provider.request().await;
    let _ = provider.request().await;
    let _ = provider.request().await;

    let messages = server.messages(&id).await;
    let parts: Vec<_> = messages
        .iter()
        .flat_map(|m| m["parts"].as_array().unwrap())
        .collect();
    let edited = parts
        .iter()
        .find(|p| p["callID"] == "call_edit_diff")
        .unwrap();
    assert_eq!(edited["state"]["status"], "completed", "{edited}");
    let diff = edited["state"]["metadata"]["diff"].as_str().unwrap();
    assert!(diff.contains("-before\n+after\n"), "{diff}");
    assert!(diff.contains("@@ "), "{diff}");

    let read = parts
        .iter()
        .find(|p| p["callID"] == "call_read_alias")
        .unwrap();
    assert_eq!(read["state"]["status"], "completed", "{read}");
    assert!(read["state"]["output"]
        .as_str()
        .unwrap()
        .contains("2: after"));
    assert!(
        parts.iter().any(|p| p["type"] == "ocmini_instruction"
            && p["text"]
                .as_str()
                .is_some_and(|t| t.contains("NESTED_ALIAS_RULE"))),
        "read with path alias should trigger nested AGENTS.md discovery: {parts:?}"
    );
}

#[tokio::test]
async fn invalid_tool_arguments_fail_before_prompting_for_permission() {
    let provider = Provider::start(vec![
        tool_script("bash", json!({"command": "echo hi", "timeout": 0}), "call_bad_bash"),
        tool_script(
            "webfetch",
            json!({"url": "https://example.com", "timeout": 0}),
            "call_bad_webfetch",
        ),
        tool_script(
            "read",
            json!({"filePath": "a.txt", "offset": 0}),
            "call_bad_read_offset",
        ),
        tool_script(
            "read",
            json!({"filePath": "a.txt", "ranges": [{"start": 3, "end": 1}]}),
            "call_bad_read_ranges",
        ),
        tool_script(
            "question",
            json!({"questions": [{"header": "Pick", "question": "Which?", "custom": false, "options": []}]}),
            "call_bad_question",
        ),
        text_script("All rejected before permission", ""),
    ])
    .await;
    let server = TestServer::start(&provider, json!({})).await;
    // Set every tool permission to "ask": if validation ran after permission,
    // the request would block waiting for a user reply instead of finishing.
    let id = server
        .session(json!([
            {"permission": "*", "pattern": "*", "action": "ask"}
        ]))
        .await;
    let response = tokio::time::timeout(
        Duration::from_secs(10),
        server
            .post(
                &format!("/session/{id}/message"),
                &prompt("msg_pre_perm", "Send invalid tool inputs"),
            )
            .send(),
    )
    .await
    .expect("invalid inputs must fail immediately without blocking on permission")
    .unwrap();
    response.error_for_status().unwrap();
    let pending_perms: Value = server
        .get("/permission")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(pending_perms, json!([]));
    let messages = server.messages(&id).await;
    for call_id in [
        "call_bad_bash",
        "call_bad_webfetch",
        "call_bad_read_offset",
        "call_bad_read_ranges",
        "call_bad_question",
    ] {
        let part = messages
            .iter()
            .flat_map(|m| m["parts"].as_array().unwrap())
            .find(|p| p["callID"] == call_id)
            .unwrap();
        assert_eq!(part["state"]["status"], "error", "{part}");
    }
}
