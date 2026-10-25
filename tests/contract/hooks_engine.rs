use super::*;

async fn late_admission_during_abort_finalizer(input_hook: bool) {
    let (_release, gate) = oneshot::channel();
    let mut stalled = text_script("started", "must not finish");
    stalled.gate = Some(gate);
    let mut provider = Provider::start(vec![stalled, text_script("resumed", "")]).await;
    let after = r#"import json,sys,pathlib,time
r=json.load(sys.stdin)
if not r['input']['started']:
 pathlib.Path('abort-queued-entered').write_text(r['context']['turnID'])
 while not pathlib.Path('release-abort-queued').exists(): time.sleep(.01)"#;
    let mut handlers = vec![registration("turn.end.after", after)];
    if input_hook {
        handlers.push(registration(
            "prompt.accept.before",
            r#"import json,sys,pathlib
r=json.load(sys.stdin)
pathlib.Path('input-'+r['context']['turnID']).touch()"#,
        ));
    }
    let server = TestServer::start(&provider, hook_config(handlers)).await;
    let sid = server.session(allow()).await;
    let mut events = server.events().await;
    server
        .post(
            &format!("/session/{sid}/prompt_async"),
            &prompt("msg_abort_active", "active"),
        )
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    provider.request().await;
    server
        .post(
            &format!("/session/{sid}/prompt_async"),
            &prompt("msg_abort_queued", "queued"),
        )
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    server
        .post(&format!("/session/{sid}/abort"), &json!({}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    // Wait until cancellation has captured the queue and paused in its finalizer.
    wait_file(&server.directory.join("abort-queued-entered")).await;
    let (mut sibling, sibling_url) = launch(server.root.path(), &server.directory).await;
    for (instance, url) in [("owner", &server.url), ("sibling", &sibling_url)] {
        for no_reply in [false, true] {
            let id = format!("msg_abort_late_{instance}_{no_reply}");
            let mut request = prompt(&id, "arrived during cancellation");
            request["noReply"] = json!(no_reply);
            let response = server
                .client
                .post(format!("{url}/session/{sid}/prompt_async"))
                .json(&request)
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), 409, "accepted late prompt {id}");
            if input_hook && instance == "owner" {
                assert!(
                    !server.directory.join(format!("input-{id}")).exists(),
                    "input hook ran after cancellation fenced admissions"
                );
            }
        }
    }
    assert!(!server
        .messages(&sid)
        .await
        .iter()
        .any(|message| message["info"]["id"]
            .as_str()
            .is_some_and(|id| id.starts_with("msg_abort_late_"))));
    std::fs::write(server.directory.join("release-abort-queued"), "").unwrap();
    let terminal_events = events.through("session.idle").await;
    for (mid, started) in [("msg_abort_active", true), ("msg_abort_queued", false)] {
        let ended: Vec<_> = terminal_events
            .iter()
            .filter(|event| {
                event["type"] == "ocmini.turn.ended" && event["properties"]["turnID"] == mid
            })
            .collect();
        assert_eq!(ended.len(), 1);
        assert_eq!(ended[0]["properties"]["reason"], "cancelled");
        assert_eq!(ended[0]["properties"]["started"], started);
    }
    {
        let conn =
            rusqlite::Connection::open(server.root.path().join(".local/share/ocmini/ocmini.db"))
                .unwrap();
        let pending: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM hook_turn WHERE session_id=?1 AND state<>'done'",
                [&sid],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(pending, 0, "cancellation left an orphaned hook turn");
    }
    assert!(provider.requests.try_recv().is_err());
    let resumed = server
        .post(
            &format!("/session/{sid}/message"),
            &prompt("msg_abort_resumed", "resume after idle"),
        )
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(answer(&resumed), "resumed");
    provider.request().await;
    assert!(provider.requests.try_recv().is_err());
    sibling.kill().await.unwrap();
}

#[tokio::test]
async fn abort_finalizer_fences_late_admission_on_owner_and_sibling() {
    late_admission_during_abort_finalizer(false).await;
}

#[tokio::test]
async fn abort_finalizer_fences_input_hooks_and_context_admission() {
    late_admission_during_abort_finalizer(true).await;
}

#[tokio::test]
async fn sibling_duplicate_waits_for_durable_finalizer_completion() {
    let mut provider = Provider::start(vec![text_script("done", "")]).await;
    let after = r#"import json,sys,pathlib,time
json.load(sys.stdin)
p=pathlib.Path('sibling-finalizer-count')
p.write_text(str(int(p.read_text())+1) if p.exists() else '1')
pathlib.Path('sibling-finalizer-entered').touch()
while not pathlib.Path('release-sibling-finalizer').exists(): time.sleep(.01)"#;
    let server = TestServer::start(
        &provider,
        hook_config(vec![registration("turn.end.after", after)]),
    )
    .await;
    let sid = server.session(allow()).await;
    let request = prompt("msg_sibling_finalizer", "work");
    let mut pending = tokio::spawn(
        server
            .post(&format!("/session/{sid}/message"), &request)
            .send(),
    );
    wait_file(&server.directory.join("sibling-finalizer-entered")).await;
    let (mut sibling, sibling_url) = launch(server.root.path(), &server.directory).await;
    let mut duplicate = tokio::spawn(
        server
            .client
            .post(format!("{sibling_url}/session/{sid}/message"))
            .json(&request)
            .send(),
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(150), &mut duplicate)
            .await
            .is_err(),
        "sibling returned a prepared response before its finalizer completed"
    );
    assert!(!pending.is_finished());
    std::fs::write(server.directory.join("release-sibling-finalizer"), "").unwrap();
    let one = tokio::time::timeout(Duration::from_secs(4), &mut pending)
        .await
        .unwrap()
        .unwrap()
        .unwrap()
        .error_for_status()
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    let two = tokio::time::timeout(Duration::from_secs(4), &mut duplicate)
        .await
        .unwrap()
        .unwrap()
        .unwrap()
        .error_for_status()
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(answer(&one), "done");
    assert_eq!(one["info"]["id"], two["info"]["id"]);
    assert_eq!(
        std::fs::read_to_string(server.directory.join("sibling-finalizer-count")).unwrap(),
        "1"
    );
    provider.request().await;
    assert!(provider.requests.try_recv().is_err());
    sibling.kill().await.unwrap();
}
