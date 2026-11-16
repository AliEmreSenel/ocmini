use super::*;

#[tokio::test]
async fn server_refuses_a_stale_jail_after_external_sandbox_changes() {
    let provider = Provider::start(vec![
        tool_script(
            "bash",
            json!({"command":"printf NEW_POLICY"}),
            "external_selection",
        ),
        text_script("Done", ""),
        tool_script(
            "bash",
            json!({"command":"printf STALE_POLICY > must-not-use-old-jail"}),
            "stale_selection",
        ),
        text_script("Done", ""),
    ])
    .await;
    let mut server = TestServer::start(&provider, json!({"sandbox":"workspace-write"})).await;
    let sid = server.session(allow()).await;
    let child: Value = server
        .post("/session", &json!({"parentID":sid,"permission":allow()}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let child = child["id"].as_str().unwrap();
    let output = configured_command(server.root.path(), &server.directory)
        .args([
            "run",
            "--session",
            &sid,
            "--auto",
            "--sandbox-preset",
            "read-only",
            "Change saved policy",
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
    let saved: Value = server
        .get(&format!("/session/{sid}"))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(saved["ocminiSandbox"]["preset"], "read-only");
    let child_response = server
        .post(
            &format!("/session/{child}/message"),
            &prompt("msg_stale_child", "Must use the root policy"),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(child_response.status(), 400);
    let new_child_response = server
        .post("/session", &json!({"parentID":sid,"permission":allow()}))
        .send()
        .await
        .unwrap();
    assert_eq!(new_child_response.status(), 400);
    let response = server
        .post(
            &format!("/session/{sid}/message"),
            &prompt("msg_stale_policy", "Must use saved policy"),
        )
        .send()
        .await
        .unwrap();
    let status = response.status();
    let body = response.text().await.unwrap();
    assert_eq!(status, 400, "{body}");
    assert!(body.contains("restart"), "{body}");
    assert!(!server.directory.join("must-not-use-old-jail").exists());
    server.restart().await;
    server
        .post(
            &format!("/session/{sid}/message"),
            &prompt("msg_stale_policy", "Use the saved read-only policy"),
        )
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    assert!(!server.directory.join("must-not-use-old-jail").exists());
}

#[tokio::test]
async fn cli_resume_keeps_saved_settings_and_applies_explicit_changes() {
    let provider = Provider::start((0..8).flat_map(|_| [
        tool_script("bash", json!({"command":"if cat \"$HOME/first/secret\" >/dev/null 2>&1; then printf FIRST_OK; else printf FIRST_DENIED; fi; if cat \"$HOME/second/secret\" >/dev/null 2>&1; then printf SECOND_OK; else printf SECOND_DENIED; fi; if printf test > probe; then printf WRITE_OK; else printf WRITE_DENIED; fi"}), "saved_policy"),
        text_script("Done", ""),
    ]).collect()).await;
    let mut provider = provider;
    let server = TestServer::start(&provider, json!({})).await;
    for name in ["first", "second"] {
        let directory = server.root.path().join(name);
        std::fs::create_dir(&directory).unwrap();
        std::fs::write(directory.join("secret"), name).unwrap();
    }
    let sid = server.session(allow()).await;
    let scenarios = [
        (
            vec![
                "--sandbox-preset",
                "read-only",
                "--sandbox-read",
                "${home}/first",
                "--sandbox-set",
                "process_isolation=full",
            ],
            "read-only",
            ["FIRST_OK", "SECOND_DENIED", "WRITE_DENIED"],
        ),
        (
            vec![],
            "read-only",
            ["FIRST_OK", "SECOND_DENIED", "WRITE_DENIED"],
        ),
        (
            vec!["--sandbox-read", "${home}/second"],
            "read-only",
            ["FIRST_OK", "SECOND_OK", "WRITE_DENIED"],
        ),
        (
            vec![
                "--sandbox-preset",
                "read-only",
                "--sandbox-hide",
                "${home}/first/secret",
            ],
            "read-only",
            ["FIRST_OK", "SECOND_OK", "WRITE_DENIED"],
        ),
        (
            vec!["--sandbox-preset", "workspace-write"],
            "workspace-write",
            ["FIRST_DENIED", "SECOND_DENIED", "WRITE_OK"],
        ),
        (
            vec![],
            "workspace-write",
            ["FIRST_DENIED", "SECOND_DENIED", "WRITE_OK"],
        ),
        (
            vec!["--sandbox-preset", "off"],
            "off",
            ["FIRST_OK", "SECOND_OK", "WRITE_OK"],
        ),
        (vec![], "off", ["FIRST_OK", "SECOND_OK", "WRITE_OK"]),
    ];
    let config_path = server.root.path().join(".config/opencode/opencode.json");
    for (index, (flags, expected_preset, expected_output)) in scenarios.into_iter().enumerate() {
        let mut config: Value =
            serde_json::from_slice(&std::fs::read(&config_path).unwrap()).unwrap();
        config["sandbox"] = json!(if index == 7 { "locked-read" } else { "off" });
        std::fs::write(&config_path, config.to_string()).unwrap();
        let output = configured_command(server.root.path(), &server.directory)
            .args(["run", "--session", &sid, "--auto"])
            .args(flags)
            .arg("Check saved sandbox")
            .stdin(Stdio::null())
            .output()
            .await
            .unwrap();
        assert!(
            output.status.success(),
            "case {index}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        provider.request().await;
        let continuation = provider.request().await;
        let output = continuation["messages"]
            .as_array()
            .unwrap()
            .iter()
            .rev()
            .find(|m| m["role"] == "tool")
            .and_then(|m| m["content"].as_str())
            .expect("tool output");
        for expected in expected_output {
            assert!(output.contains(expected), "case {index}: {output}");
        }
        let session: Value = server
            .get(&format!("/session/{sid}"))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(session["ocminiSandbox"]["preset"], expected_preset);
        if index == 2 {
            let response = server
                .client
                .patch(format!("{}/session/{sid}", server.url))
                .json(&json!({"metadata":{"note":"keep saved settings"}}))
                .send()
                .await
                .unwrap();
            let updated: Value = response.error_for_status().unwrap().json().await.unwrap();
            assert_eq!(updated["ocminiSandbox"], session["ocminiSandbox"]);
            let response = server
                .client
                .patch(format!("{}/session/{sid}", server.url))
                .json(&json!({"metadata":{"ocminiSandbox":{"preset":"off","config":{"off":true}}}}))
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), 400);
        }
    }
}

#[tokio::test]
async fn forks_of_saved_children_use_the_roots_last_sandbox_settings() {
    let provider = Provider::start(vec![
        tool_script(
            "bash",
            json!({"command":"printf PARENT > parent-selection"}),
            "parent_selection",
        ),
        text_script("Done", ""),
        tool_script(
            "bash",
            json!({"command":"printf FORK > fork-selection"}),
            "fork_selection",
        ),
        text_script("Done", ""),
    ])
    .await;
    let server = TestServer::start(&provider, json!({"sandbox":"read-only"})).await;
    let parent = server.session(allow()).await;
    let child: Value = server
        .post("/session", &json!({"parentID":parent,"permission":allow()}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let child = child["id"].as_str().unwrap();
    let output = configured_command(server.root.path(), &server.directory)
        .args([
            "run",
            "--session",
            &parent,
            "--auto",
            "--sandbox-preset",
            "workspace-write",
            "Change sandbox",
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
    let fork: Value = server
        .post(&format!("/session/{child}/fork"), &json!({}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(fork["ocminiSandbox"]["preset"], "workspace-write");
    let fork = fork["id"].as_str().unwrap();
    server
        .post(
            &format!("/session/{fork}/message"),
            &prompt("msg_fork_selection", "Write using inherited sandbox"),
        )
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(server.directory.join("fork-selection")).unwrap(),
        "FORK"
    );
}

#[tokio::test]
async fn saved_settings_survive_preset_changes_and_server_restart() {
    let provider = Provider::start(vec![
        tool_script("bash", json!({"command":"if printf escaped > forbidden-after-restart; then printf WRITE_ALLOWED; else printf WRITE_DENIED; fi"}), "saved_restart"),
        text_script("Done", ""),
        tool_script("bash", json!({"command":"if printf escaped > forbidden-after-restart; then printf WRITE_ALLOWED; else printf WRITE_DENIED; fi"}), "saved_deleted_preset"),
        text_script("Done", ""),
    ]).await;
    let mut server = TestServer::start(&provider, json!({"sandbox":"read-only"})).await;
    let sid = server.session(allow()).await;
    let preset = server
        .root
        .path()
        .join(".config/opencode/sandbox/presets/read-only.json");
    let mut config: Value = serde_json::from_slice(&std::fs::read(&preset).unwrap()).unwrap();
    config["fs"]["writable"] = json!(["${workspace}"]);
    std::fs::write(preset, config.to_string()).unwrap();
    server.restart().await;
    server
        .post(
            &format!("/session/{sid}/message"),
            &prompt("msg_saved_restart", "Try writing"),
        )
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    assert!(!server.directory.join("forbidden-after-restart").exists());
    let messages = server.messages(&sid).await;
    let part = messages
        .iter()
        .flat_map(|m| m["parts"].as_array().unwrap())
        .find(|p| p["callID"] == "saved_restart")
        .unwrap();
    assert!(
        part["state"]["output"]
            .as_str()
            .is_some_and(|s| s.contains("WRITE_DENIED")),
        "{part}"
    );
    std::fs::remove_file(
        server
            .root
            .path()
            .join(".config/opencode/sandbox/presets/read-only.json"),
    )
    .unwrap();
    let config_path = server.root.path().join(".config/opencode/opencode.json");
    let mut config: Value = serde_json::from_slice(&std::fs::read(&config_path).unwrap()).unwrap();
    config["sandbox"] = json!("removed-default-preset");
    std::fs::write(config_path, config.to_string()).unwrap();
    server.restart().await;
    server
        .post(
            &format!("/session/{sid}/message"),
            &prompt("msg_deleted_preset", "Try writing again"),
        )
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    assert!(!server.directory.join("forbidden-after-restart").exists());
    let messages = server.messages(&sid).await;
    let part = messages
        .iter()
        .flat_map(|m| m["parts"].as_array().unwrap())
        .find(|p| p["callID"] == "saved_deleted_preset")
        .unwrap();
    assert!(
        part["state"]["output"]
            .as_str()
            .is_some_and(|s| s.contains("WRITE_DENIED")),
        "{part}"
    );
}

#[tokio::test]
async fn read_all_and_locked_read_presets_enforce_their_filesystem_access() {
    let mut provider = Provider::start((0..3).flat_map(|_| [
        tool_script("bash", json!({"command":"if cat \"$HOME/secret\" >/dev/null 2>&1; then printf READ_OK; else printf READ_DENIED; fi; if printf test > workspace-write-test; then printf WORKSPACE_OK; else printf WORKSPACE_DENIED; fi; if printf escaped > \"$HOME/outside-write\"; then printf OUTSIDE_WRITE; fi"}), "preset_access"),
        text_script("Done", ""),
    ]).collect()).await;
    let server = TestServer::start(&provider, json!({})).await;
    std::fs::write(server.root.path().join("secret"), "SECRET").unwrap();
    for preset in ["read-all", "read-all-net", "locked-read"] {
        let output = configured_command(server.root.path(), &server.directory)
            .args([
                "run",
                "--auto",
                "--sandbox-preset",
                preset,
                "--sandbox-set",
                "tmp=false",
                "Check access",
            ])
            .stdin(Stdio::null())
            .output()
            .await
            .unwrap();
        assert!(
            output.status.success(),
            "{preset}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        provider.request().await;
        let continuation = provider.request().await;
        let output = continuation["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["role"] == "tool")
            .and_then(|m| m["content"].as_str())
            .expect("tool output");
        for expected in if preset == "locked-read" {
            ["READ_DENIED", "WORKSPACE_DENIED"]
        } else {
            ["READ_OK", "WORKSPACE_OK"]
        } {
            assert!(output.contains(expected), "{preset}: {output}");
        }
        assert!(!output.contains("OUTSIDE_WRITE"), "{preset}: {output}");
        assert!(!server.root.path().join("outside-write").exists());
    }
}

#[tokio::test]
async fn cli_resume_uses_the_saved_workspaces_sandbox() {
    let mut provider = Provider::start(vec![
        tool_script(
            "bash",
            json!({"command":"if printf escaped > forbidden.txt; then printf WRITE_ALLOWED; else printf WRITE_DENIED; fi"}),
            "resume_policy",
        ),
        text_script("Done", ""),
    ])
    .await;
    let server = TestServer::start(&provider, json!({})).await;
    let workspace = server.root.path().join("saved workspace");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::write(
        workspace.join("opencode.json"),
        r#"{"sandbox":"read-only"}"#,
    )
    .unwrap();
    let session: Value = server
        .post("/session", &json!({"permission":allow()}))
        .header("x-opencode-directory", encode_directory(&workspace))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let output = configured_command(server.root.path(), &server.directory)
        .args([
            "run",
            "--session",
            session["id"].as_str().unwrap(),
            "--auto",
            "Try writing",
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
    assert!(!workspace.join("forbidden.txt").exists());
    provider.request().await;
    let continuation = provider.request().await;
    assert!(
        continuation["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["role"] == "tool"
                && m["content"]
                    .as_str()
                    .is_some_and(|s| s.contains("WRITE_DENIED"))),
        "{continuation}"
    );
}

#[tokio::test]
async fn restored_children_freeze_the_policy_for_the_whole_session_tree() {
    let provider = Provider::start(vec![
        tool_script("bash", json!({"command":"if printf escaped > child-write; then printf WRITE_ALLOWED; else printf WRITE_DENIED; fi"}), "child_policy"),
        text_script("Done", ""),
        tool_script("bash", json!({"command":"if printf escaped > parent-write; then printf WRITE_ALLOWED; else printf WRITE_DENIED; fi"}), "parent_policy"),
        text_script("Done", ""),
    ]).await;
    let mut server = TestServer::start(&provider, json!({"sandbox":"read-only"})).await;
    let parent = server.session(allow()).await;
    let child: Value = server
        .post("/session", &json!({"parentID":parent,"permission":allow()}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let child = child["id"].as_str().unwrap().to_owned();
    server.restart().await;
    server
        .post(
            &format!("/session/{child}/message"),
            &prompt("msg_child_policy", "Try writing"),
        )
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    assert!(
        !server.directory.join("child-write").exists(),
        "{:?}",
        server.messages(&child).await
    );
    let preset = server
        .root
        .path()
        .join(".config/opencode/sandbox/presets/read-only.json");
    let mut config: Value = serde_json::from_slice(&std::fs::read(&preset).unwrap()).unwrap();
    config["fs"]["writable"] = json!(["${workspace}"]);
    std::fs::write(preset, config.to_string()).unwrap();
    server
        .post(
            &format!("/session/{parent}/message"),
            &prompt("msg_parent_policy", "Try writing"),
        )
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    for (sid, call, file) in [
        (&child, "child_policy", "child-write"),
        (&parent, "parent_policy", "parent-write"),
    ] {
        assert!(!server.directory.join(file).exists(), "{file}");
        let messages = server.messages(sid).await;
        let part = messages
            .iter()
            .flat_map(|m| m["parts"].as_array().unwrap())
            .find(|p| p["callID"] == call)
            .unwrap();
        assert!(
            part["state"]["output"]
                .as_str()
                .is_some_and(|s| s.contains("WRITE_DENIED")),
            "{part}"
        );
    }
}

#[tokio::test]
async fn v2_waking_saved_input_initializes_the_sandbox_without_new_admission() {
    let provider = Provider::start(vec![
        tool_script("shell", json!({"command":"if printf escaped > wake-write; then printf WRITE_ALLOWED; else printf WRITE_DENIED; fi"}), "wake_policy"),
        text_script("Done", ""),
    ]).await;
    let mut server = TestServer::start(&provider, json!({"sandbox":"read-only"})).await;
    let sid = server.session(allow()).await;
    server
        .post(
            &format!("/api/session/{sid}/prompt"),
            &json!({"text":"Try writing", "resume":false}),
        )
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    server.restart().await;
    let mut events = server.events_at("/api/event").await;
    server
        .client
        .delete(format!("{}/api/session/{sid}/revert", server.url))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    events.through("session.execution.succeeded").await;
    assert!(!server.directory.join("wake-write").exists());
    let messages = server.messages(&sid).await;
    let part = messages
        .iter()
        .flat_map(|m| m["parts"].as_array().unwrap())
        .find(|p| p["callID"] == "wake_policy")
        .unwrap();
    assert!(
        part["state"]["output"]
            .as_str()
            .is_some_and(|s| s.contains("WRITE_DENIED")),
        "{part}"
    );
}

#[tokio::test]
async fn v2_interrupt_stops_background_jobs_inside_the_runner() {
    let (_release, gate) = oneshot::channel();
    let mut stalled = text_script("Waiting", "");
    stalled.gate = Some(gate);
    let provider = Provider::start(vec![
        tool_script(
            "shell",
            json!({"command":"exec sleep 30","background":true}),
            "background_job",
        ),
        stalled,
    ])
    .await;
    let server = TestServer::start(&provider, json!({"sandbox":"workspace-write"})).await;
    let sid = server.session(allow()).await;
    let mut events = server.events_at("/api/event").await;
    server
        .post(
            &format!("/api/session/{sid}/prompt"),
            &json!({"text":"Start background work"}),
        )
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    events.through("session.text.delta").await;
    let messages = server.messages(&sid).await;
    let part = messages
        .iter()
        .flat_map(|m| m["parts"].as_array().unwrap())
        .find(|p| p["callID"] == "background_job")
        .unwrap();
    let pid = part["state"]["metadata"]["pid"]
        .as_i64()
        .expect("background pid") as i32;
    assert_eq!(unsafe { libc::kill(pid, 0) }, 0);
    let interrupted: Value = server
        .post(&format!("/api/session/{sid}/interrupt"), &json!({}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(interrupted["interrupted"], true);
    events.through("session.execution.interrupted").await;
    let stopped = tokio::time::timeout(Duration::from_secs(3), async {
        while unsafe { libc::kill(pid, 0) } == 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .is_ok();
    if !stopped {
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
    }
    assert!(
        stopped,
        "interrupted runner left background job {pid} alive"
    );
}

#[tokio::test]
async fn v2_moves_use_the_new_directory_without_widening_the_sandbox() {
    let provider = Provider::start(vec![
        tool_script(
            "shell",
            json!({"command":"printf MOVED > moved.txt"}),
            "moved_tool",
        ),
        text_script("Done", ""),
    ])
    .await;
    let server = TestServer::start(&provider, json!({"sandbox":"workspace-write"})).await;
    let destination = server.directory.join("destination");
    let outside = server.root.path().join("outside");
    std::fs::create_dir(&destination).unwrap();
    std::fs::create_dir(&outside).unwrap();
    let sid = server.session(allow()).await;
    let mut events = server.events_at("/api/event").await;
    server
        .post(
            &format!("/api/session/{sid}/move"),
            &json!({"directory":destination}),
        )
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    events.through("session.moved").await;
    events.through("session.execution.succeeded").await;
    server
        .post(
            &format!("/api/session/{sid}/prompt"),
            &json!({"text":"Write in the moved workspace"}),
        )
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    events.through("session.execution.succeeded").await;
    assert!(!server.directory.join("moved.txt").exists());
    assert_eq!(
        std::fs::read_to_string(destination.join("moved.txt")).unwrap(),
        "MOVED"
    );
    server
        .post(
            &format!("/api/session/{sid}/move"),
            &json!({"directory":outside}),
        )
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    events.through("session.execution.failed").await;
    let session: Value = server
        .get(&format!("/api/session/{sid}"))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        session["data"]["location"]["directory"],
        destination.to_string_lossy().as_ref()
    );
}

#[tokio::test]
async fn environment_hide_lists_refuse_missing_targets() {
    let provider = Provider::start(vec![]).await;
    let server = TestServer::start(&provider, json!({})).await;
    let sid = server.session(allow()).await;
    let output = configured_command(server.root.path(), &server.directory)
        .env(
            "OCMINI_TEST_HIDDEN",
            server.root.path().join("missing-secret"),
        )
        .args([
            "run",
            "--session",
            &sid,
            "--auto",
            "--sandbox-preset",
            "read-only",
            "--sandbox-set",
            "process_isolation=full",
            "--sandbox-read",
            "${home}",
            "--sandbox-hide",
            "${env:OCMINI_TEST_HIDDEN}",
            "Check hidden paths",
        ])
        .stdin(Stdio::null())
        .output()
        .await
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(78),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("hide target does not exist"));
}

#[tokio::test]
async fn read_only_preset_runs_without_broad_proc_sys_or_dev_read_grants() {
    let provider = Provider::start(vec![
        tool_script("bash", json!({"command":"for path in /proc /sys /dev; do if ls \"$path\" >/dev/null 2>&1; then printf 'EXPOSED:%s\\n' \"$path\"; else printf 'DENIED:%s\\n' \"$path\"; fi; done"}), "read_only_roots"),
        text_script("Done", ""),
    ]).await;
    let server = TestServer::start(&provider, json!({"sandbox":"read-only"})).await;
    let sid = server.session(allow()).await;
    server
        .post(
            &format!("/session/{sid}/message"),
            &prompt("msg_read_only_roots", "Read system directories"),
        )
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let messages = server.messages(&sid).await;
    let part = messages
        .iter()
        .flat_map(|m| m["parts"].as_array().unwrap())
        .find(|p| p["callID"] == "read_only_roots")
        .unwrap();
    assert_eq!(part["state"]["status"], "completed", "{part}");
    let output = part["state"]["output"].as_str().unwrap();
    for path in ["/proc", "/sys", "/dev"] {
        assert!(output.contains(&format!("DENIED:{path}")), "{output}");
    }
    assert!(!output.contains("EXPOSED"), "{output}");
}

#[tokio::test]
async fn cli_environment_lists_grant_each_explicit_directory_before_clearing_the_environment() {
    let provider = Provider::start(vec![
        tool_script(
            "bash",
            json!({"command":"first-env-tool > first.result; second-env-tool > second.result; priority-env-tool > priority.result; printf '%s\\n' \"${OCMINI_TEST_TOOL_DIRS-unset}\" > environment.result"}),
            "environment_list",
        ),
        text_script("Done", ""),
    ])
    .await;
    let server = TestServer::start(&provider, json!({})).await;
    let first = server.root.path().join("tools [literal] ${home}");
    let second = server.root.path().join("other tools");
    use std::os::unix::fs::PermissionsExt;
    for (directory, tool, message) in [
        (&first, "first-env-tool", "FIRST_ENV_DIR"),
        (&second, "second-env-tool", "SECOND_ENV_DIR"),
    ] {
        std::fs::create_dir(directory).unwrap();
        let path = directory.join(tool);
        std::fs::write(&path, format!("#!/bin/sh\nprintf '%s\\n' {message}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        let priority = directory.join("priority-env-tool");
        std::fs::write(&priority, format!("#!/bin/sh\nprintf '%s\\n' {message}\n")).unwrap();
        std::fs::set_permissions(&priority, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let directories = std::env::join_paths([&first, &second, &first]).unwrap();
    let output = configured_command(server.root.path(), &server.directory)
        .env("OCMINI_TEST_TOOL_DIRS", &directories)
        .args([
            "run",
            "--auto",
            "--format",
            "json",
            "--sandbox-preset",
            "workspace-write",
            "--sandbox-read",
            "${env:OCMINI_TEST_TOOL_DIRS}",
            "--sandbox-exec",
            "${env:OCMINI_TEST_TOOL_DIRS}",
            "Run tools",
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
    assert_eq!(
        std::fs::read_to_string(server.directory.join("first.result")).unwrap(),
        "FIRST_ENV_DIR\n"
    );
    assert_eq!(
        std::fs::read_to_string(server.directory.join("second.result")).unwrap(),
        "SECOND_ENV_DIR\n"
    );
    assert_eq!(
        std::fs::read_to_string(server.directory.join("priority.result")).unwrap(),
        "FIRST_ENV_DIR\n"
    );
    assert_eq!(
        std::fs::read_to_string(server.directory.join("environment.result")).unwrap(),
        "unset\n"
    );
    for template in ["${env:PATH}", "${env:OCMINI_TEST_TOOL_DIRS}"] {
        let output = configured_command(server.root.path(), &server.directory)
            .env("PATH", &directories)
            .env("OCMINI_TEST_TOOL_DIRS", &directories)
            .args([
                "tui",
                "--session",
                "missing",
                "--sandbox-preset",
                "read-only",
                "--sandbox-read",
                template,
                "--sandbox-exec",
                template,
            ])
            .output()
            .await
            .unwrap();
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("Session not found: missing"),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[tokio::test]
async fn v2_session_ids_cannot_escape_sandbox_storage() {
    let provider = Provider::start(vec![]).await;
    let server = TestServer::start(&provider, json!({"sandbox":{"preset":"read-only"}})).await;
    for id in [
        "ses_probe/../../escape",
        "ses_probe\\escape",
        "ses_probe\0escape",
    ] {
        let response = server
            .post(
                "/api/session",
                &json!({"id":id,"location":{"directory":server.directory}}),
            )
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 400, "{id:?}");
        assert!(response
            .text()
            .await
            .unwrap()
            .contains("single path component"));
    }
    assert!(!server
        .root
        .path()
        .join(".local/share/ocmini/escape")
        .exists());
}

#[tokio::test]
async fn cli_overrides_grant_explicit_folders_and_replace_isolated_networking() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let command = format!(
        "printf CLI_WRITE > \"$HOME/shared/output\"; \
         cat \"$HOME/blocked/secret\" > \"$HOME/shared/leak\" 2>/dev/null || printf READ_DENIED > \"$HOME/shared/leak\"; \
         bash -c 'printf CLI_NETWORK >/dev/tcp/127.0.0.1/{port}'"
    );
    let provider = Provider::start(vec![
        tool_script("bash", json!({"command":command}), "cli_overrides"),
        text_script("Done", ""),
    ])
    .await;
    let server = TestServer::start(&provider, json!({})).await;
    for name in ["shared", "blocked"] {
        std::fs::create_dir(server.root.path().join(name)).unwrap();
    }
    std::fs::write(server.root.path().join("blocked/secret"), "HOST_SECRET").unwrap();
    let preset = server
        .root
        .path()
        .join(".config/opencode/sandbox/presets/read-only.json");
    let before = std::fs::read(&preset).unwrap();
    let overlay = server.root.path().join("override.json");
    std::fs::write(&overlay, r#"{"env":{"allow":["HOME","LANG"]}}"#).unwrap();
    let output = configured_command(server.root.path(), &server.directory)
        .args([
            "--sandbox-read",
            "${env:HOME}/shared",
            "run",
            "--auto",
            "--sandbox-preset",
            "read-only",
            "--sandbox-config",
        ])
        .arg(&overlay)
        .args([
            "--sandbox-network",
            "host",
            "--sandbox-write",
            "${home}/shared",
            "Run tool",
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
    assert_eq!(
        std::fs::read_to_string(server.root.path().join("shared/output")).unwrap(),
        "CLI_WRITE"
    );
    assert_eq!(
        std::fs::read_to_string(server.root.path().join("shared/leak")).unwrap(),
        "READ_DENIED"
    );
    let (mut connection, _) = tokio::time::timeout(Duration::from_secs(2), listener.accept())
        .await
        .unwrap()
        .unwrap();
    use tokio::io::AsyncReadExt;
    let mut received = String::new();
    connection.read_to_string(&mut received).await.unwrap();
    assert_eq!(received, "CLI_NETWORK");
    assert_eq!(std::fs::read(&preset).unwrap(), before);
}

#[tokio::test]
async fn invalid_cli_policy_overrides_refuse_startup() {
    let root = tempfile::tempdir().unwrap();
    for args in [
        vec!["--sandbox-preset", "off", "--sandbox-read", "${home}"],
        vec![
            "--sandbox-preset",
            "read-only",
            "--sandbox-set",
            "unknown=true",
        ],
        vec!["--sandbox-preset", "read-only", "--sandbox-add", "tmp=true"],
        vec![
            "--sandbox-preset",
            "read-only",
            "--sandbox-exec",
            "/outside",
        ],
        vec![
            "--sandbox-preset",
            "read-only",
            "--sandbox-config",
            "/missing-overlay.json",
        ],
        vec![
            "--sandbox-preset",
            "read-only",
            "--sandbox-read",
            "${env:OCMINI_TEST_TOOLS}/subdir",
        ],
    ] {
        let output = tokio::time::timeout(
            Duration::from_secs(10),
            configured_command(root.path(), root.path())
                .env(
                    "OCMINI_TEST_TOOLS",
                    std::env::join_paths(["/usr/bin", "/bin"]).unwrap(),
                )
                .args(["serve", "--port", "0"])
                .args(&args)
                .kill_on_drop(true)
                .output(),
        )
        .await
        .expect("invalid policy should refuse startup")
        .unwrap();
        assert_eq!(
            output.status.code(),
            Some(78),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stderr).contains("Sandbox startup refused:"));
    }
}

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
    let server = TestServer::start(&provider, json!({"sandbox":"workspace-write"})).await;
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
async fn presets_follow_each_session_workspace_and_explicit_user_toolchain_paths() {
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

    // Neither this project nor this explicitly configured toolchain existed at installation.
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
    let preset_path = server
        .root
        .path()
        .join(".config/opencode/sandbox/presets/workspace-write.json");
    let mut preset: Value = serde_json::from_slice(&std::fs::read(&preset_path).unwrap()).unwrap();
    for list in ["readable", "exec"] {
        preset["fs"][list]
            .as_array_mut()
            .unwrap()
            .push(json!("${env:HOME}/.cargo/bin"));
    }
    std::fs::write(&preset_path, serde_json::to_vec_pretty(&preset).unwrap()).unwrap();
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
