use super::*;
use crate::config::HookConfigSource;
use serde_json::json;
use std::path::Path;
use std::sync::atomic::Ordering;

fn source(value: Value, name: &str, trusted: bool) -> HookConfigSource {
    HookConfigSource {
        value,
        source: name.into(),
        base: Path::new("/config").join(name),
        trusted,
    }
}
fn sources(values: Vec<HookConfigSource>) -> Config {
    Config {
        hook_sources: Some(Arc::new(values)),
        ..Config::default()
    }
}
fn runtime(value: Value, directory: &Path) -> Arc<Hooks> {
    let config: Config = serde_json::from_value(json!({"ocmini":{"hooks":value}})).unwrap();
    Hooks::build(
        Resolved::from_config(&config, directory.to_str().unwrap()).unwrap(),
        false,
    )
    .unwrap()
}
fn context(directory: &Path) -> Value {
    json!({"directory":directory,"sessionID":"ses_test","turnID":"msg_turn","turnKind":"primary"})
}
fn handler(id: &str, on: &str, script: &str) -> Value {
    json!({"id":id,"on":on,"command":["sh","-c",script]})
}

#[test]
fn catalog_has_exactly_the_planned_names() {
    assert_eq!(types::HOOK_NAMES.len(), 26);
    assert_eq!(types::EVENT_NAMES.len(), 49);
    let mut hooks = types::HOOK_NAMES.to_vec();
    hooks.sort();
    hooks.dedup();
    assert_eq!(hooks.len(), 26);
    let mut events = types::EVENT_NAMES.to_vec();
    events.sort();
    events.dedup();
    assert_eq!(events.len(), 49);
}
#[test]
fn project_cannot_self_authorize_replace_or_erase_trusted_handlers() {
    let global = json!({"handlers":[handler("trusted","turn.start.before","exit 0")]});
    for project in [
        json!({"allowProjectHooks":true,"handlers":[handler("project","turn.start.before","exit 0")]}),
        json!({"handlers":[]}),
        json!({"handlers":[{"id":"trusted","enabled":false}]}),
    ] {
        let cfg = sources(vec![
            source(global.clone(), "global", true),
            source(project, "project", false),
        ]);
        let resolved = Resolved::from_config(&cfg, "/project").unwrap();
        assert!(resolved.handlers[0].active());
        assert!(resolved.handlers[0].trusted);
        assert!(resolved
            .handlers
            .iter()
            .filter(|r| !r.trusted)
            .all(|r| !r.active()));
    }
}
#[test]
fn trusted_order_policy_and_id_merge_are_stable() {
    let cfg = sources(vec![
        source(
            json!({"handlers":[handler("one","turn.start.before","echo first"),handler("two","turn.start.before","echo second")]}),
            "global",
            true,
        ),
        source(
            json!({"handlers":[handler("project","turn.start.before","echo project"),{"id":"one","enabled":false}]}),
            "project",
            false,
        ),
        source(
            json!({"allowProjectHooks":["/allowed/*"],"handlers":[handler("one","turn.start.before","echo launch")]}),
            "launch",
            true,
        ),
    ]);
    let resolved = Resolved::from_config(&cfg, "/allowed/repo").unwrap();
    let ids = resolved
        .handlers
        .iter()
        .filter(|r| r.active())
        .map(|r| r.registration.id.as_str())
        .collect::<Vec<_>>();
    assert_eq!(ids, ["one", "two", "project"]);
    assert_eq!(resolved.handlers[0].source, "launch");
    assert!(resolved.handlers.last().unwrap().blocked.is_some());
}
#[test]
fn source_resolves_only_the_direct_executable() {
    let cfg = sources(vec![source(
        json!({"handlers":[{"id":"path","on":"turn.start.before","command":["./bin/hook","relative-script.py"]}]}),
        "global",
        true,
    )]);
    let r = Resolved::from_config(&cfg, "/project").unwrap();
    assert_eq!(
        r.handlers[0].registration.command,
        ["/config/global/./bin/hook", "relative-script.py"]
    );
}
#[test]
fn settings_reject_unknown_names_fields_bad_budgets_and_unbatched_deltas() {
    for value in [
        json!({"wat":true}),
        json!({"version":2}),
        json!({"chainTimeoutMs":0}),
        json!({"admissionChainTimeoutMs":5001}),
        json!({"maxOutputBytes":0}),
        json!({"maxEndContinuations":-1}),
        json!({"handlers":[handler("x","turn.unknown","exit 0")]}),
        json!({"handlers":[{"id":"x","on":"turn.start.before","command":["true"],"match":{"turnkind":"primary"}}]}),
        json!({"observers":[handler("x","message.part.delta","exit 0")]}),
        json!({"observers":[handler("x","unknown.event","exit 0")]}),
    ] {
        let cfg: Config = serde_json::from_value(json!({"ocmini":{"hooks":value}})).unwrap();
        assert!(Resolved::from_config(&cfg, "/project").is_err(), "{value}");
    }
    assert_eq!(
        Resolved::from_config(&Config::default(), "/project")
            .unwrap()
            .settings
            .max_end_continuations,
        Some(3)
    );
}
#[test]
fn long_execution_and_continuation_overrides_are_explicit() {
    let cfg: Config = serde_json::from_value(json!({"ocmini":{"hooks":{
        "chainTimeoutMs":null,"finalizerChainTimeoutMs":null,"maxEndContinuations":null,
        "maxGlobalProcesses":32,"maxGlobalObservers":12,"maxPendingHooks":256,
        "handlers":[{"id":"long","on":"turn.end.before","command":["true"],"timeoutMs":600000}]
    }}}))
    .unwrap();
    let resolved = Resolved::from_config(&cfg, "/project").unwrap();
    assert_eq!(resolved.settings.chain_timeout_ms, None);
    assert_eq!(resolved.settings.max_end_continuations, None);
    let debug = resolved.debug();
    assert_eq!(
        debug["effectiveBudgets"][0]["boundaries"][0]["executionCeilingMs"],
        600000
    );
    assert!(debug["effectiveBudgets"][0]["boundaries"][0]["chainTimeoutMs"].is_null());
    assert_eq!(debug["processPolicy"]["maxGlobalProcesses"], 32);
    for on in [
        json!("*"),
        json!("file.edited"),
        json!(["ocmini.turn.ended", "message.updated"]),
    ] {
        let cfg:Config=serde_json::from_value(json!({"ocmini":{"hooks":{"observers":[{"id":"invalid-durable","on":on,"command":["true"],"delivery":"durable"}]}}})).unwrap();
        assert!(Resolved::from_config(&cfg, "/project").is_err());
    }
}

#[tokio::test]
#[cfg(unix)]
async fn verbose_stderr_does_not_invalidate_a_valid_hook() {
    let dir = tempfile::tempdir().unwrap();
    let h = json!({"id":"noisy","on":"turn.start.before","maxStderrBytes":64,
        "command":["python3","-c","import sys,json; sys.stdin.read(); sys.stderr.write('noise'*500000); print(json.dumps({'set':{'context':'ok'}}))"]});
    let hooks = runtime(json!({"handlers":[h]}), dir.path());
    let result = hooks
        .run(
            "turn.start.before",
            &context(dir.path()),
            json!({"context":"before"}),
            None,
        )
        .await
        .unwrap();
    assert_eq!(result.input["context"], "ok");
}

#[tokio::test]
#[cfg(unix)]
async fn optional_failures_keep_previous_valid_values_but_explicit_denials_stop() {
    let dir = tempfile::tempdir().unwrap();
    for script in [
        "printf '%s' '{\"set\":{\"context\":\"uncommitted\"}}'; exit 1",
        "printf '%s' '{\"set\":{\"context\":\"uncommitted\",\"immutable\":true}}'",
        "printf '%s' 'malformed'",
    ] {
        let mut first = handler("optional", "turn.start.before", script);
        first["failurePolicy"] = json!("continue");
        let second = json!({"id":"next","on":"turn.start.before","command":["python3","-c","import json,sys; x=json.load(sys.stdin); assert x['input']['context']=='original'; print(json.dumps({'set':{'context':'verified'}}))"]});
        let hooks = runtime(json!({"handlers":[first,second]}), dir.path());
        assert_eq!(
            hooks
                .run(
                    "turn.start.before",
                    &context(dir.path()),
                    json!({"context":"original"}),
                    None
                )
                .await
                .unwrap()
                .input["context"],
            "verified"
        );
    }
    let mut deny = handler(
        "deny",
        "turn.start.before",
        "printf '%s' '{\"decision\":\"block\",\"reason\":\"required\"}'",
    );
    deny["failurePolicy"] = json!("continue");
    let hooks = runtime(json!({"handlers":[deny]}), dir.path());
    assert!(hooks
        .run(
            "turn.start.before",
            &context(dir.path()),
            json!({"context":"original"}),
            None
        )
        .await
        .unwrap_err()
        .contains("hook rejected: required"));

    let mut permission = handler(
        "optional-permission",
        "permission.decide",
        "printf '%s' '{\"set\":{\"decision\":\"allow\"}}'",
    );
    permission["failurePolicy"] = json!("continue");
    let hooks = runtime(json!({"handlers":[permission]}), dir.path());
    assert_eq!(
        hooks
            .run(
                "permission.decide",
                &context(dir.path()),
                json!({"decision":"ask"}),
                None
            )
            .await
            .unwrap()
            .input["decision"],
        "ask"
    );
    for response in [
        "{\"decision\":\"deny\"}",
        "{\"decision\":\"block\",\"reason\":42}",
    ] {
        let mut deny = handler(
            "deny",
            "turn.start.before",
            &format!("printf '%s' '{response}'"),
        );
        deny["failurePolicy"] = json!("continue");
        let hooks = runtime(json!({"handlers":[deny]}), dir.path());
        assert!(hooks
            .run(
                "turn.start.before",
                &context(dir.path()),
                json!({"context":"before"}),
                None
            )
            .await
            .is_err());
    }
}

#[tokio::test]
#[cfg(unix)]
async fn optional_invalid_environment_and_runtime_metadata_are_discarded() {
    let dir = tempfile::tempdir().unwrap();
    let h = json!({"id":"env","on":"shell.env","failurePolicy":"continue","command":["python3","-c","print('{\"set\":{\"env\":{\"OCMINI_HOOK_SESSION_ID\":\"wrong\"}}}')"]});
    let hooks = runtime(json!({"handlers":[h]}), dir.path());
    assert_eq!(
        hooks
            .run(
                "shell.env",
                &context(dir.path()),
                json!({"env":{"SAFE":"original"}}),
                None
            )
            .await
            .unwrap()
            .input["env"],
        json!({"SAFE":"original"})
    );
    let h = json!({"id":"metadata","on":"tool.execute.after","failurePolicy":"continue","command":["python3","-c","print('{\"set\":{\"metadata\":{\"pid\":2}}}')"]});
    let hooks = runtime(json!({"handlers":[h]}), dir.path());
    assert_eq!(
        hooks
            .run(
                "tool.execute.after",
                &context(dir.path()),
                json!({"metadata":{"pid":1}}),
                None
            )
            .await
            .unwrap()
            .input["metadata"]["pid"],
        1
    );
}
#[test]
fn model_matchers_require_a_known_model() {
    let matcher: types::Matcher = serde_json::from_value(json!({"model":"*"})).unwrap();
    for value in [
        json!({}),
        json!({"model":null}),
        json!({"model":{"providerID":"mock"}}),
    ] {
        assert!(!matcher.matches("session.update.before", &value, true));
    }
    assert!(matcher.matches(
        "session.update.before",
        &json!({"model":{"providerID":"mock","modelID":"test"}}),
        true
    ));
}

#[tokio::test]
#[cfg(unix)]
async fn optional_failure_policy_cannot_swallow_cancellation() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = handler("optional", "turn.start.before", "cat >/dev/null; sleep 30");
    h["failurePolicy"] = json!("continue");
    let hooks = runtime(json!({"handlers":[h]}), dir.path());
    let cancel = Arc::new(AtomicBool::new(false));
    let flag = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        flag.store(true, Ordering::Release);
    });
    assert!(hooks
        .run(
            "turn.start.before",
            &context(dir.path()),
            json!({}),
            Some(&cancel)
        )
        .await
        .unwrap_err()
        .contains("cancelled"));
}

#[tokio::test]
#[cfg(unix)]
async fn explicit_chain_deadline_remains_authoritative() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = handler(
        "short-chain",
        "turn.start.before",
        "cat >/dev/null; sleep 10",
    );
    h["timeoutMs"] = json!(600000);
    let hooks = runtime(json!({"chainTimeoutMs":100,"handlers":[h]}), dir.path());
    assert!(hooks
        .run("turn.start.before", &context(dir.path()), json!({}), None)
        .await
        .unwrap_err()
        .contains("deadline"));
}

#[tokio::test]
#[cfg(unix)]
#[ignore = "121-second subprocess smoke check; run explicitly for timeout policy changes"]
async fn long_execution_override_survives_two_minutes() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = handler(
        "long-check",
        "turn.end.before",
        "cat >/dev/null; sleep 121; printf '%s' '{\"decision\":\"allow\"}'",
    );
    h["timeoutMs"] = json!(150000);
    let hooks = runtime(json!({"handlers":[h]}), dir.path());
    hooks
        .run("turn.end.before", &context(dir.path()), json!({}), None)
        .await
        .unwrap();
}
#[test]
fn default_kind_matching_is_explicit_and_resource_hooks_are_unfiltered() {
    let m = types::Matcher::default();
    let compaction = json!({"turnKind":"compaction","requestKind":"compaction"});
    assert!(!m.matches("turn.start.before", &compaction, true));
    assert!(!m.matches("model.context.transform", &compaction, true));
    assert!(m.matches("compaction.before", &compaction, true));
    assert!(m.matches("file.change.before", &compaction, true));
    let opt: types::Matcher =
        serde_json::from_value(json!({"turnKind":"*","requestKind":["primary","compaction"]}))
            .unwrap();
    assert!(opt.matches("turn.start.before", &compaction, true));
    assert!(!opt.matches("turn.end.before", &compaction, true));
    assert!(!m.matches(
        "message.text.complete",
        &json!({"requestKind":"title"}),
        true
    ));
}
#[test]
fn strict_protocol_rejects_mutations_decisions_and_bad_shapes() {
    for (name, response) in [
        ("turn.start.before", json!({"set":{"sessionID":"evil"}})),
        ("turn.start.before", json!({"set":{"context":[]}})),
        (
            "turn.start.before",
            json!({"decision":"continue","feedback":"again"}),
        ),
        (
            "turn.end.before",
            json!({"decision":"continue","feedback":" "}),
        ),
        ("turn.end.before", json!({"feedback":"again"})),
        (
            "turn.end.before",
            json!({"decision":"continue","feedback":"again","set":{}}),
        ),
        ("turn.end.after", json!({"decision":"allow"})),
        ("turn.end.after", json!({"set":{}})),
        ("session.delete.before", json!({"set":{"title":"x"}})),
        (
            "model.headers.transform",
            json!({"set":{"headers":{"Authorization":"secret"}}}),
        ),
        ("tool.execute.after", json!({"set":{"metadata":{"exit":0}}})),
        (
            "tool.execute.before",
            json!({"set":{"args":{}},"unknown":true}),
        ),
    ] {
        let mut input = json!({});
        assert!(
            types::apply_response(name, response.clone(), &mut input).is_err(),
            "{name} {response}"
        );
    }
    let mut input = json!({"error":"executed failure"});
    assert!(types::apply_response(
        "tool.execute.after",
        json!({"set":{"output":"success"}}),
        &mut input
    )
    .is_err());
}
#[test]
fn snapshots_roundtrip_and_fingerprints_ignore_object_key_order() {
    let config = Config::default();
    let resolved = Resolved::from_config(&config, "/project").unwrap();
    let value = serde_json::to_value(&resolved).unwrap();
    let replay: Resolved = serde_json::from_value(value.clone()).unwrap();
    replay.validate_snapshot("/project").unwrap();
    assert!(replay.validate_snapshot("/other").is_err());
    assert_eq!(
        config::fingerprint(&json!({"b":2,"a":1})),
        config::fingerprint(&json!({"a":1,"b":2}))
    );
    assert_ne!(config::fingerprint(&value), config::fingerprint(&json!({})));
}
#[tokio::test]
async fn no_handlers_returns_the_same_owned_input_allocation() {
    let dir = tempfile::tempdir().unwrap();
    let hooks = runtime(json!({}), dir.path());
    let input = json!({"text":"a large string".repeat(100)});
    let ptr = input["text"].as_str().unwrap().as_ptr();
    let result = hooks
        .run("turn.start.before", &context(dir.path()), input, None)
        .await
        .unwrap();
    assert_eq!(ptr, result.input["text"].as_str().unwrap().as_ptr());
    assert!(hooks.is_empty());
}
#[tokio::test]
#[cfg(unix)]
async fn sequential_handlers_receive_the_previous_replacement() {
    let dir = tempfile::tempdir().unwrap();
    let hooks = runtime(
        json!({"handlers":[handler("one","turn.start.before","cat >/dev/null; printf '%s' '{\"set\":{\"context\":\"first\"}}'"),json!({"id":"two","on":"turn.start.before","command":["python3","-c","import json,sys; x=json.load(sys.stdin); print(json.dumps({'set':{'context':x['input']['context']+' second'}}))"]})]}),
        dir.path(),
    );
    let result = hooks
        .run(
            "turn.start.before",
            &context(dir.path()),
            json!({"context":""}),
            None,
        )
        .await
        .unwrap();
    assert_eq!(result.input["context"], "first second");
}
#[tokio::test]
#[cfg(unix)]
async fn continue_stops_remaining_gate_handlers() {
    let dir = tempfile::tempdir().unwrap();
    let hooks = runtime(
        json!({"handlers":[handler("one","turn.end.before","printf '%s' '{\"decision\":\"continue\",\"feedback\":\"run tests\"}'"),handler("two","turn.end.before","touch should-not-run")]}),
        dir.path(),
    );
    let result = hooks
        .run("turn.end.before", &context(dir.path()), json!({}), None)
        .await
        .unwrap();
    assert_eq!(result.feedback.as_deref(), Some("run tests"));
    assert!(!dir.path().join("should-not-run").exists());
}
#[tokio::test]
#[cfg(unix)]
async fn argv_and_stdin_are_never_interpolated() {
    let dir = tempfile::tempdir().unwrap();
    let injection = "$(touch injected); `touch injected`";
    let hooks = runtime(
        json!({"handlers":[{"id":"literal","on":"turn.start.before","command":["python3","-c","import json,sys; x=json.load(sys.stdin); assert sys.argv[1]==x['input']['context']; print(json.dumps({'set':{'context':sys.argv[1]}}))",injection]}]}),
        dir.path(),
    );
    let out = hooks
        .run(
            "turn.start.before",
            &context(dir.path()),
            json!({"context":injection}),
            None,
        )
        .await
        .unwrap();
    assert_eq!(out.input["context"], injection);
    assert!(!dir.path().join("injected").exists());
}
#[tokio::test]
#[cfg(unix)]
async fn malformed_and_oversized_process_io_fails_without_hanging() {
    let dir = tempfile::tempdir().unwrap();
    for (script, extra) in [
        ("printf 'not-json'", json!({})),
        ("printf '{}{}'", json!({})),
        ("head -c 1024 /dev/zero", json!({"maxOutputBytes":64})),
        ("touch should-not-run", json!({"maxInputBytes":10})),
    ] {
        let mut h = handler("io", "turn.start.before", script);
        for (key, value) in extra.as_object().unwrap() {
            h[key] = value.clone();
        }
        let hooks = runtime(json!({"handlers":[h]}), dir.path());
        assert!(
            tokio::time::timeout(
                Duration::from_secs(3),
                hooks.run(
                    "turn.start.before",
                    &context(dir.path()),
                    json!({"context":""}),
                    None
                )
            )
            .await
            .unwrap()
            .is_err(),
            "{script}"
        );
    }
    assert!(!dir.path().join("should-not-run").exists());
}
#[tokio::test]
#[cfg(target_os = "linux")]
async fn timeout_kills_descendants_and_reaps_direct_child() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = handler(
        "timeout",
        "turn.start.before",
        "sleep 30 & echo $$ $! > pids; wait",
    );
    h["timeoutMs"] = json!(150);
    let hooks = runtime(json!({"handlers":[h]}), dir.path());
    assert!(hooks
        .run("turn.start.before", &context(dir.path()), json!({}), None)
        .await
        .unwrap_err()
        .contains("deadline"));
    let pids = std::fs::read_to_string(dir.path().join("pids"))
        .unwrap()
        .split_whitespace()
        .map(|s| s.parse::<u32>().unwrap())
        .collect::<Vec<_>>();
    assert!(!Path::new(&format!("/proc/{}", pids[0])).exists());
    stopped(pids[1]).await;
}
#[cfg(target_os = "linux")]
async fn stopped(pid: u32) {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
                Err(_) => return,
                Ok(s) if s.rsplit_once(") ").unwrap().1.starts_with('Z') => return,
                _ => tokio::time::sleep(Duration::from_millis(10)).await,
            }
        }
    })
    .await
    .unwrap();
}
#[tokio::test]
#[cfg(unix)]
async fn cancellation_and_active_invocation_identity_are_scoped() {
    let dir = tempfile::tempdir().unwrap();
    let hooks = runtime(
        json!({"handlers":[handler("cancel","turn.start.before","echo $OCMINI_HOOK_INVOCATION_ID > invocation; sleep 30")]}),
        dir.path(),
    );
    let cancel = Arc::new(AtomicBool::new(false));
    let h = hooks.clone();
    let flag = cancel.clone();
    let ctx = context(dir.path());
    let task = tokio::spawn(async move {
        h.run("turn.start.before", &ctx, json!({}), Some(&flag))
            .await
    });
    tokio::time::timeout(Duration::from_secs(2), async {
        while !dir.path().join("invocation").exists() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    let id = std::fs::read_to_string(dir.path().join("invocation")).unwrap();
    let id = id.trim();
    assert!(hooks.is_active_invocation(id, "ses_test"));
    assert!(!hooks.is_active_invocation(id, "ses_other"));
    cancel.store(true, Ordering::Release);
    assert!(tokio::time::timeout(Duration::from_secs(1), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err()
        .contains("cancelled"));
    assert!(!hooks.is_active_invocation(id, "ses_test"));
}
#[tokio::test]
#[cfg(unix)]
async fn dropped_invocation_kills_and_reaps_process() {
    let dir = tempfile::tempdir().unwrap();
    let hooks = runtime(
        json!({"handlers":[handler("drop","turn.start.before","echo $$ > pid; sleep 30")]}),
        dir.path(),
    );
    let ctx = context(dir.path());
    let task =
        tokio::spawn(async move { hooks.run("turn.start.before", &ctx, json!({}), None).await });
    tokio::time::timeout(Duration::from_secs(2), async {
        while !dir.path().join("pid").exists() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    let pid = std::fs::read_to_string(dir.path().join("pid"))
        .unwrap()
        .trim()
        .parse::<u32>()
        .unwrap();
    task.abort();
    let _ = task.await;
    #[cfg(target_os = "linux")]
    stopped(pid).await;
}
