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
    assert_eq!(
        health["version"],
        concat!("2.2.5-ocmini.", env!("CARGO_PKG_VERSION"))
    );
    assert_eq!(server.get("/api/info").send().await.unwrap().status(), 200);
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
            concat!("2.2.5-ocmini.", env!("CARGO_PKG_VERSION"))
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
async fn run_reports_fatal_provider_errors_in_both_formats() {
    let provider = Provider::start(vec![
        Script::http_status_body(400, "invalid request from CLI test"),
        Script::http_status_body(400, "invalid request from CLI test"),
    ])
    .await;
    let server = TestServer::start(&provider, json!({})).await;
    for format in ["text", "json"] {
        let output = configured_command(server.root.path(), &server.directory)
            .args(["run", "--format", format, "failure test"])
            .stdin(Stdio::null())
            .output()
            .await
            .unwrap();
        assert!(!output.status.success(), "{format}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("invalid request from CLI test"),
            "{output:?}"
        );
    }
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

#[tokio::test]
async fn configured_reasoning_options_replace_the_catalog_variants() {
    // OpenCode 1.19.3: provider.<id>.models.<id>.reasoning_options declares the
    // reasoning controls a model accepts. They build the variants a client can
    // offer and replace the discovered ones; configured variants merge on top,
    // and an empty list means the model takes no variants at all.
    let provider = Provider::start(vec![]).await;
    let server = TestServer::start(
        &provider,
        json!({"provider":{"mock":{
            "npm":"@ai-sdk/openai-compatible",
            "name":"Local test model",
            "options":{"baseURL":provider.url},
            "models":{
                "test-model":{
                    "reasoning_options":[{"type":"effort","values":["low",null]}],
                    "variants":{"high":{"headers":{"x-mode":"high"}}}
                },
                "plain":{
                    "name":"Plain model",
                    "reasoning_options":[]
                }
            }
        }}}),
    )
    .await;
    let inventory: Value = server
        .get("/provider")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let models = &inventory["all"][0]["models"];
    assert_eq!(
        models["test-model"]["variants"]["low"]["reasoningEffort"],
        "low"
    );
    assert_eq!(
        models["test-model"]["variants"]["none"]["reasoningEffort"],
        "none"
    );
    assert_eq!(
        models["test-model"]["variants"]["high"]["headers"]["x-mode"],
        "high"
    );
    assert_eq!(models["plain"]["variants"], json!({}));
}

#[tokio::test]
async fn run_retries_partial_responses_without_panicking() {
    let provider = Provider::start(vec![
        Script::mid_stream_error("provider stream failed: error decoding response body"),
        text_script("Recovered answer", ""),
    ])
    .await;
    let server = TestServer::start(&provider, json!({})).await;
    let output = configured_command(server.root.path(), &server.directory)
        .args(["run", "hello"])
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
    assert!(stdout.contains("partial response discarded; retrying"));
    assert!(stdout.contains("Recovered answer"));
}

#[tokio::test]
async fn run_commands_use_templates_models_agents_and_allow_empty_arguments() {
    let mut provider =
        Provider::start(vec![text_script("reviewed", ""), text_script("pong", "")]).await;
    let server = TestServer::start(&provider, json!({
        "provider":{"mock":{"npm":"@ai-sdk/openai-compatible", "options":{"baseURL":provider.url},
            "models":{"test-model":{},"command-model":{}}}},
        "command":{
            "review":{"template":"First $1. Rest $2.", "agent":"reviewer", "model":"mock/command-model"},
            "ping":{"template":"PING_WITHOUT_ARGUMENTS"}
        }
    })).await;
    for args in [
        vec!["run", "--command", "review", "\"alpha beta\" gamma delta"],
        vec!["run", "--command", "ping"],
    ] {
        let output = configured_command(server.root.path(), &server.directory)
            .args(args)
            .stdin(Stdio::null())
            .output()
            .await
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let review = provider.request().await;
    assert_eq!(review["model"], "command-model");
    assert!(review
        .to_string()
        .contains("First alpha beta. Rest gamma delta."));
    assert!(review.to_string().contains("REVIEWER_SYSTEM"));
    assert!(provider
        .request()
        .await
        .to_string()
        .contains("PING_WITHOUT_ARGUMENTS"));
}

#[tokio::test]
#[cfg(unix)]
async fn piped_run_requires_auto_for_shell_and_filters_terminal_controls() {
    let provider = Provider::start(vec![
        tool_script(
            "bash",
            json!({"command":"touch shell-started"}),
            "call_unattended",
        ),
        text_script("before\x1b", "[2Jafter"),
        tool_script(
            "bash",
            json!({"command":"touch shell-started"}),
            "call_auto",
        ),
        text_script("done", ""),
    ])
    .await;
    let server = TestServer::start(&provider, json!({})).await;
    let marker = server.directory.join("shell-started");
    let output = configured_command(server.root.path(), &server.directory)
        .args(["run", "attempt shell"])
        .output()
        .await
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(!marker.exists());
    assert!(!output.stdout.contains(&0x1b));
    assert!(String::from_utf8_lossy(&output.stdout).contains("after"));

    let output = configured_command(server.root.path(), &server.directory)
        .args(["run", "--auto", "allow shell"])
        .output()
        .await
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(marker.exists());
}

#[tokio::test]
async fn sandbox_preset_is_global_without_conflicting_with_subcommands() {
    let provider = Provider::start(vec![]).await;
    let server = TestServer::start(&provider, json!({})).await;
    let sid = server.session(allow()).await;
    for args in [
        vec!["--sandbox-preset", "off", "session", "list"],
        vec!["session", "--sandbox-preset", "off", "list"],
        vec!["session", "list", "--sandbox-preset", "off"],
    ] {
        let output = configured_command(server.root.path(), &server.directory)
            .env("OCMINI_SANDBOX_PRESET", "must-be-overridden")
            .args(&args)
            .output()
            .await
            .unwrap();
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains(&sid));
    }
}

#[cfg(feature = "sandbox")]
#[tokio::test]
async fn sandbox_overrides_refuse_non_execution_commands() {
    let root = tempfile::tempdir().unwrap();
    for args in [
        vec!["--sandbox-read", "/usr", "models"],
        vec!["--sandbox-set", "tmp=true", "session", "list"],
    ] {
        let output = configured_command(root.path(), root.path())
            .args(&args)
            .output()
            .await
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(2),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stderr).contains("Sandbox overrides require"));
    }
}

#[cfg(not(feature = "sandbox"))]
#[tokio::test]
async fn sandbox_free_binary_refuses_saved_active_policies_unless_explicitly_disabled() {
    let provider = Provider::start(vec![text_script("Disabled explicitly", "")]).await;
    let server = TestServer::start(&provider, json!({})).await;
    let sid = server.session(allow()).await;
    let store = ocmini_core::store::Store::open(
        &server.root.path().join(".local/share/ocmini/ocmini.db"),
        "saved-policy-test",
    )
    .unwrap();
    store.mutate_session(&sid, |session| {
        session.sandbox = Some(json!({"preset":"locked-read","config":{"fs":{"readable":["${workspace}"]},"env":{"clear":true},"network":{"mode":"isolated"}}}));
        Ok(true)
    }).unwrap();
    let denied = configured_command(server.root.path(), &server.directory)
        .args(["run", "--session", &sid, "--auto", "Must refuse"])
        .stdin(Stdio::null())
        .output()
        .await
        .unwrap();
    assert_eq!(
        denied.status.code(),
        Some(78),
        "{}",
        String::from_utf8_lossy(&denied.stderr)
    );
    let response = server
        .post(
            &format!("/session/{sid}/message"),
            &prompt("msg_saved_active", "Must refuse"),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 400);
    let allowed = configured_command(server.root.path(), &server.directory)
        .args([
            "run",
            "--session",
            &sid,
            "--auto",
            "--sandbox-preset",
            "off",
            "Disable explicitly",
        ])
        .stdin(Stdio::null())
        .output()
        .await
        .unwrap();
    assert!(
        allowed.status.success(),
        "{}",
        String::from_utf8_lossy(&allowed.stderr)
    );
    let saved = store.get_session(&sid).unwrap().unwrap().sandbox.unwrap();
    assert_eq!(saved, json!({"preset":"off","config":{"off":true}}));
}

#[cfg(not(feature = "sandbox"))]
#[tokio::test]
async fn sandbox_free_binary_refuses_workspace_sandbox_requirements() {
    let provider = Provider::start(vec![]).await;
    let server = TestServer::start(&provider, json!({})).await;
    for (name, sandbox) in [
        ("restricted", json!({"preset":"locked-read"})),
        ("restricted-name", json!("locked-read")),
        ("malformed", json!({"preset":true})),
    ] {
        let directory = server.root.path().join(name);
        std::fs::create_dir(&directory).unwrap();
        std::fs::write(
            directory.join("opencode.json"),
            json!({"sandbox":sandbox}).to_string(),
        )
        .unwrap();
        let response = server
            .post("/session", &json!({"title":"Must refuse"}))
            .header("x-opencode-directory", encode_directory(&directory))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 400, "{name}");
        assert!(response
            .text()
            .await
            .unwrap()
            .contains("Sandbox startup refused:"));
    }
}

#[cfg(unix)]
#[tokio::test]
async fn context_inspection_does_not_run_turn_hooks_or_start_mcp() {
    let hook = r#"import json,sys,pathlib
json.load(sys.stdin);pathlib.Path('context-hook-ran').touch();print('{}')"#;
    let mcp = r#"import pathlib;pathlib.Path('context-mcp-started').touch()"#;
    let provider = Provider::start(vec![]).await;
    let server = TestServer::start(
        &provider,
        json!({
            "ocmini":{"hooks":{"version":1,"handlers":[
                {"id":"turn-start","on":"turn.start.before","command":[PYTHON,"-S","-c",hook]},
                {"id":"catalog","on":"tool.catalog.transform","command":[PYTHON,"-S","-c",hook]}
            ]}},
            "mcp":{"probe":{"type":"local","command":[PYTHON,"-S","-c",mcp],"enabled":true}}
        }),
    )
    .await;
    tui_terminal(
        &server,
        json!([
            [["F1 help"], "/co"],
            [["/compact  Summarize", "/context  Inspect"], "\u{1b}[B\t\r"],
            [["Context for"], ""]
        ]),
    )
    .await;
    assert!(!server.directory.join("context-hook-ran").exists());
    assert!(!server.directory.join("context-mcp-started").exists());
}

#[cfg(unix)]
#[tokio::test]
async fn tui_completes_and_runs_configured_commands() {
    let mut provider = Provider::start(vec![text_script("Reviewed", "")]).await;
    let server = TestServer::start(&provider, json!({"ui": {"scroll_mode": "fullscreen"}})).await;
    tui_terminal(
        &server,
        json!([
            [["F1 help"], "/rev"],
            [
                ["/revert  Revert", "/review  Review arguments"],
                "\u{1b}[B\t\"alpha beta\" gamma delta\r"
            ],
            [["Reviewed"], ""]
        ]),
    )
    .await;
    assert!(provider
        .request()
        .await
        .to_string()
        .contains("First alpha beta. Rest gamma delta."));
}

#[cfg(unix)]
async fn tui_terminal(server: &TestServer, interactions: Value) {
    let script = r#"import errno,fcntl,json,os,pty,select,struct,subprocess,sys,termios,time
fd,slave=pty.openpty()
fcntl.ioctl(slave,termios.TIOCSWINSZ,struct.pack('HHHH',40,120,0,0))
os.set_blocking(fd,False)
with os.fdopen(slave, 'rb', buffering=0) as terminal:
 child=subprocess.Popen([sys.argv[1],'tui'],stdin=terminal,stdout=terminal,stderr=terminal)
deadline=time.monotonic()+15
def expect(markers):
 data=b''
 markers=[marker.encode() for marker in markers]
 while time.monotonic()<deadline and not all(marker in data for marker in markers):
  ready,_,_=select.select([fd],[],[],0.1)
  if ready:
   try: chunk=os.read(fd,65536)
   except BlockingIOError: continue
   except OSError as err:
    if err.errno != errno.EIO: raise
    break
   if not chunk: break
   data+=chunk
 missing=[marker for marker in markers if marker not in data]
 if missing:
  print(data.decode(errors='replace'))
  sys.exit(f'Missing terminal output: {missing!r}')
try:
 for markers, keys in json.loads(sys.argv[2]):
  expect(markers)
  if keys:
   os.write(fd,keys.encode())
finally:
 os.close(fd)
 child.kill()
 child.wait(timeout=3)
"#;
    let mut command = tokio::process::Command::new(PYTHON);
    command
        .args(["-S", "-c", script, env!("CARGO_BIN_EXE_ocmini")])
        .arg(interactions.to_string())
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", server.root.path())
        .env("XDG_CONFIG_HOME", server.root.path().join(".config"))
        .env("XDG_DATA_HOME", server.root.path().join(".local/share"))
        .env("OPENCODE_CONFIG_CONTENT", "{}")
        .env("OPENCODE_AUTH_CONTENT", "{}")
        .env("TERM", "xterm")
        .current_dir(&server.directory)
        .kill_on_drop(true);
    let output = tokio::time::timeout(Duration::from_secs(25), command.output())
        .await
        .expect("terminal interaction test timed out")
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[tokio::test]
async fn cli_session_mcp_and_config_subcommands_cover_lifecycle_and_json() {
    let provider = Provider::start(vec![text_script("first", ""), text_script("forked", "")]).await;
    let server = TestServer::start(
        &provider,
        json!({
            "mcp": {
                "demo": {
                    "type": "local",
                    "command": ["echo", "hi"],
                    "enabled": false
                }
            }
        }),
    )
    .await;

    // Create a session via `run --directory` (alias for --dir)
    let out = configured_command(server.root.path(), &server.directory)
        .args([
            "run",
            "--auto",
            "--directory",
            ".",
            "--title",
            "Original",
            "hello world",
        ])
        .stdin(Stdio::null())
        .output()
        .await
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Fork using -c --fork (not only -s)
    let out = configured_command(server.root.path(), &server.directory)
        .args(["run", "--auto", "-c", "--fork", "branching out"])
        .stdin(Stdio::null())
        .output()
        .await
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    // session list --json --roots --limit 1
    let out = configured_command(server.root.path(), &server.directory)
        .args(["session", "list", "--json", "--roots", "--limit", "1"])
        .output()
        .await
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let limited: Vec<Value> = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(limited.len(), 1);

    let out = configured_command(server.root.path(), &server.directory)
        .args(["session", "list", "--json"])
        .output()
        .await
        .unwrap();
    let all: Vec<Value> = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(all.len(), 2);
    let sid = all[0]["id"].as_str().unwrap().to_owned();

    // session rename
    let out = configured_command(server.root.path(), &server.directory)
        .args(["session", "rename", &sid, "Renamed Title"])
        .output()
        .await
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("Renamed Title"));

    // session show --json
    let out = configured_command(server.root.path(), &server.directory)
        .args(["session", "show", &sid, "--json"])
        .output()
        .await
        .unwrap();
    assert!(out.status.success());
    let shown: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(shown["session"]["title"], "Renamed Title");
    assert!(!shown["messages"].as_array().unwrap().is_empty());

    // session export
    let out = configured_command(server.root.path(), &server.directory)
        .args(["session", "export", &sid])
        .output()
        .await
        .unwrap();
    assert!(out.status.success());
    let exported: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(exported["session"]["id"], sid);
    assert!(exported["messages"].is_array());
    assert!(exported["todos"].is_array());

    // session fork --json
    let out = configured_command(server.root.path(), &server.directory)
        .args(["session", "fork", &sid, "--json"])
        .output()
        .await
        .unwrap();
    assert!(out.status.success());
    let forked: Value = serde_json::from_slice(&out.stdout).unwrap();
    let forked_id = forked["id"].as_str().unwrap().to_owned();
    assert_ne!(forked_id, sid);

    // session delete
    let out = configured_command(server.root.path(), &server.directory)
        .args(["session", "delete", &forked_id])
        .output()
        .await
        .unwrap();
    assert!(out.status.success());
    let out = configured_command(server.root.path(), &server.directory)
        .args(["session", "show", &forked_id])
        .output()
        .await
        .unwrap();
    assert!(!out.status.success());

    // mcp list --json
    let out = configured_command(server.root.path(), &server.directory)
        .args(["mcp", "list", "--json"])
        .output()
        .await
        .unwrap();
    assert!(out.status.success());
    let mcps: Vec<Value> = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(mcps.len(), 1);
    assert_eq!(mcps[0]["name"], "demo");
    assert_eq!(mcps[0]["enabled"], false);

    // config --json
    let out = configured_command(server.root.path(), &server.directory)
        .args(["config", "--json"])
        .output()
        .await
        .unwrap();
    assert!(out.status.success());
    let cfg: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(cfg["provider"]["mock"].is_object());
}

#[tokio::test]
async fn tui_and_run_share_consistent_cli_flags_and_interactive_quit() {
    let tui_help = Command::new(env!("CARGO_BIN_EXE_ocmini"))
        .args(["tui", "--help"])
        .output()
        .await
        .unwrap();
    assert!(tui_help.status.success());
    let text = String::from_utf8(tui_help.stdout).unwrap();
    for flag in [
        "-m",
        "--model",
        "--agent",
        "--variant",
        "--fork",
        "--title",
        "-f",
        "--file",
        "--directory",
    ] {
        assert!(text.contains(flag), "tui --help missing {flag}: {text}");
    }

    let run_help = Command::new(env!("CARGO_BIN_EXE_ocmini"))
        .args(["run", "--help"])
        .output()
        .await
        .unwrap();
    assert!(run_help.status.success());
    let text = String::from_utf8(run_help.stdout).unwrap();
    for flag in ["-r", "--resume", "--fork", "--dir"] {
        assert!(text.contains(flag), "run --help missing {flag}: {text}");
    }

    // Non-TTY `run -i` accepts `/quit` cleanly.
    let provider = Provider::start(vec![text_script("first reply", "")]).await;
    let server = TestServer::start(&provider, json!({})).await;
    let mut child = configured_command(server.root.path(), &server.directory)
        .args(["run", "--auto", "-i", "initial prompt"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    {
        let mut stdin = child.stdin.take().unwrap();
        stdin.write_all(b"/quit\n").await.unwrap();
    }
    let output = child.wait_with_output().await.unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("first reply"));
}

#[cfg(unix)]
#[tokio::test]
async fn tui_runtime_slash_commands_switch_state_and_attach_files() {
    let provider = Provider::start(vec![]).await;
    let server = TestServer::start(&provider, json!({})).await;
    std::fs::write(server.directory.join("notes.txt"), "hello attachment\n").unwrap();
    tui_terminal(
        &server,
        json!([
            [["F1 help"], "/agent plan\r"],
            [["agent: plan"], "/model mock/test-model#high\r"],
            [["model: mock/test-model#high"], "/rename Parity Session\r"],
            [["title: Parity Session"], "/attach notes.txt\r"],
            [["attached: notes.txt"], "/exit\r"]
        ]),
    )
    .await;
}
