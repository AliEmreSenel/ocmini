use super::*;

#[path = "hooks_engine.rs"]
mod engine;

fn registration(name: &str, code: &str) -> Value {
    json!({"id":name.replace('.',"-"),"on":name,"command":["python3","-c",code]})
}
fn hook_config(handlers: Vec<Value>) -> Value {
    json!({"ocmini":{"hooks":{"version":1,"handlers":handlers}}})
}
async fn wait_file(path: &Path) {
    tokio::time::timeout(Duration::from_secs(8), async {
        while !path.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("hook did not reach its boundary");
}
fn answer(value: &Value) -> String {
    value["parts"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|p| p["type"] == "text")
        .filter_map(|p| p["text"].as_str())
        .collect::<Vec<_>>()
        .join("")
}

#[tokio::test]
async fn post_tool_hooks_preserve_unchanged_spills_and_bound_replacement_output() {
    let original = format!(
        "BEGIN\n{}\nMIDDLE_SENTINEL\n{}\nEND\n",
        "x".repeat(35_000),
        "y".repeat(35_000)
    );
    let replacement = format!(
        "REPLACED\n{}\nNEW_MIDDLE\n{}\nDONE\n",
        "a".repeat(35_000),
        "b".repeat(35_000)
    );
    let code = r#"import json,sys
r=json.load(sys.stdin)
mode=sys.argv[1]
if mode=='empty': sys.exit(0)
changes={}
if mode=='title': changes['title']='Hook title'
if mode=='same-output': changes['output']=r['input']['output']
if mode=='small': changes['output']='replacement'
if mode=='large': changes['output']='REPLACED\n'+'a'*35000+'\nNEW_MIDDLE\n'+'b'*35000+'\nDONE\n'
print(json.dumps({'set':changes}))"#;
    for mode in ["empty", "allow", "title", "same-output", "small", "large"] {
        let mut provider = Provider::start(vec![
            tool_script(
                "bash",
                json!({"command":"cat input.txt"}),
                "call_hook_spill",
            ),
            text_script("Done", ""),
        ])
        .await;
        let mut handler = registration("tool.execute.after", code);
        handler["command"].as_array_mut().unwrap().push(json!(mode));
        let server = TestServer::start(&provider, hook_config(vec![handler])).await;
        std::fs::write(server.directory.join("input.txt"), &original).unwrap();
        let sid = server.session(allow()).await;
        server
            .post(
                &format!("/session/{sid}/message"),
                &prompt("msg_hook_spill", "Run the tool"),
            )
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
        provider.request().await;
        let continuation = provider.request().await;
        let sent = continuation["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|message| message["role"] == "tool")
            .unwrap()["content"]
            .as_str()
            .unwrap();
        let messages = server.messages(&sid).await;
        let tool = messages
            .iter()
            .flat_map(|message| message["parts"].as_array().unwrap())
            .find(|part| part["callID"] == "call_hook_spill")
            .unwrap();
        assert_eq!(tool["state"]["output"], sent, "{mode}");
        assert!(sent.len() < 52 * 1024, "{mode}");
        if mode == "small" {
            assert_eq!(sent, "replacement");
            assert_eq!(tool["state"]["metadata"]["truncated"], false);
            assert!(tool["state"]["metadata"].get("outputPath").is_none());
        } else {
            let path = tool["state"]["metadata"]["outputPath"].as_str().unwrap();
            assert_eq!(tool["state"]["metadata"]["truncated"], true);
            assert_eq!(
                std::fs::read_to_string(path).unwrap(),
                if mode == "large" {
                    replacement.as_str()
                } else {
                    original.as_str()
                },
                "{mode}"
            );
            assert!(!sent.contains("MIDDLE"), "{mode}");
            assert!(sent.contains(path), "{mode}");
        }
        if mode == "title" {
            assert_eq!(tool["state"]["title"], "Hook title");
        }
        let files = server.root.path().join(".local/share/ocmini/tool-output");
        assert_eq!(
            std::fs::read_dir(files).unwrap().count(),
            if mode == "large" { 2 } else { 1 },
            "{mode}"
        );
    }
}

#[tokio::test]
async fn observers_match_session_ancestry_for_messages_and_parts_after_restart() {
    let mut provider = Provider::start(vec![
        text_script("Root response", ""),
        text_script("Child response", ""),
        text_script("Resumed child response", ""),
    ])
    .await;
    let code = r#"import json,sys,pathlib
r=json.load(sys.stdin)
kind=sys.argv[1]
for e in r['events']:
 p=e['properties']
 sid=p['sessionID']
 with pathlib.Path(kind+'-'+sid+'.jsonl').open('a') as f: f.write(json.dumps(e)+'\n')
 if e['type']=='ocmini.turn.ended': pathlib.Path(kind+'-'+p['turnID']+'-done').touch()"#;
    let observers = ["root", "child"]
        .into_iter()
        .map(|kind| {
            json!({
                "id":format!("{kind}-messages"),
                "on":["message.updated","message.part.updated","ocmini.turn.ended"],
                "match":{"session":kind},
                "command":["python3","-c",code,kind]
            })
        })
        .collect::<Vec<_>>();
    let mut server = TestServer::start(
        &provider,
        json!({"ocmini":{"hooks":{"observers":observers}}}),
    )
    .await;
    let root = server.session(allow()).await;
    let child = server
        .post("/session", &json!({"parentID":root,"permission":allow()}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json::<Value>()
        .await
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    for (index, sid, kind, other) in [
        (0, &root, "root", "child"),
        (1, &child, "child", "root"),
        (2, &child, "child", "root"),
    ] {
        if index == 2 {
            server.restart().await;
        }
        let mid = format!("msg_observer_scope_{index}");
        server
            .post(&format!("/session/{sid}/message"), &prompt(&mid, "Respond"))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
        provider.request().await;
        wait_file(&server.directory.join(format!("{kind}-{mid}-done"))).await;
        let recorded =
            std::fs::read_to_string(server.directory.join(format!("{kind}-{sid}.jsonl"))).unwrap();
        let events = recorded
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert!(
            events.iter().any(|event| event["type"] == "message.updated"
                && event["properties"]["info"]["id"] == mid),
            "missing user update for {mid}"
        );
        let assistant = events
            .iter()
            .find(|event| {
                event["type"] == "message.updated"
                    && event["properties"]["info"]["role"] == "assistant"
                    && event["properties"]["info"]["parentID"] == mid
            })
            .unwrap_or_else(|| panic!("missing assistant update for {mid}"));
        let assistant_id = &assistant["properties"]["info"]["id"];
        assert!(
            events
                .iter()
                .any(|event| event["type"] == "message.part.updated"
                    && event["properties"]["part"]["messageID"] == *assistant_id
                    && event["properties"]["part"]["type"] == "text"),
            "missing assistant part for {mid}"
        );
        assert!(
            !server
                .directory
                .join(format!("{other}-{sid}.jsonl"))
                .exists(),
            "events for {sid} reached the {other} observer"
        );
    }
}

#[tokio::test]
async fn file_payload_hook_transforms_large_text_without_large_control_json() {
    let original = "café 🦀\n".repeat(40_000);
    let mut provider = Provider::start(vec![text_script(&original, "")]).await;
    let code = r#"import json,sys,os
wrapper=json.load(sys.stdin)
assert wrapper['version']==2
with open(wrapper['payload']['path'],encoding='utf-8') as f: request=json.load(f)
assert len(request['input']['text'])>200000
response={'version':1,'set':{'text':'TRANSFORMED '+request['input']['text']}}
path=os.environ['OCMINI_HOOK_RESPONSE_PATH']
with open(path,'x',encoding='utf-8') as f: json.dump(response,f,ensure_ascii=False)
json.dump({'version':2,'responseFile':path},sys.stdout)"#;
    let mut h = registration("message.text.complete", code);
    h["payloadMode"] = json!("file");
    h["maxInputBytes"] = json!(1024);
    h["maxOutputBytes"] = json!(512);
    let server = TestServer::start(&provider, hook_config(vec![h])).await;
    let sid = server.session(allow()).await;
    let value = server
        .post(
            &format!("/session/{sid}/message"),
            &prompt("msg_large_hook_file", "work"),
        )
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(answer(&value), format!("TRANSFORMED {original}"));
    let _ = provider.request().await;
}

#[tokio::test]
async fn durable_terminal_observer_retries_after_restart_with_stable_event_id() {
    let mut provider = Provider::start(vec![text_script("finished", "")]).await;
    let observer = r#"import json,sys,pathlib
r=json.load(sys.stdin)
event=r['events'][0]
p=pathlib.Path('first_delivery')
if not p.exists():
    p.write_text(event['id'])
    sys.exit(1)
assert p.read_text()==event['id']
pathlib.Path('delivered').write_text(event['id'])
print('{"ack":true}')"#;
    let config = json!({"ocmini":{"hooks":{"observers":[{"id":"durable-terminal","on":"ocmini.turn.ended","delivery":"durable","retryAttempts":2,"retryDelayMs":1500,"command":["python3","-c",observer]}]}}});
    let mut server = TestServer::start(&provider, config).await;
    let sid = server.session(allow()).await;
    let value = server
        .post(
            &format!("/session/{sid}/message"),
            &prompt("msg_durable_hook", "work"),
        )
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(answer(&value), "finished");
    let _ = provider.request().await;
    wait_file(&server.directory.join("first_delivery")).await;
    let db_path = server.root.path().join(".local/share/ocmini/ocmini.db");
    tokio::time::timeout(Duration::from_secs(5),async {
        loop {
            let conn=rusqlite::Connection::open(&db_path).unwrap();
            let pending:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM hook_delivery WHERE state='pending' AND attempts=1)",[],|r|r.get(0)).unwrap();
            if pending {break;}
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }).await.unwrap();
    server.restart().await;
    wait_file(&server.directory.join("delivered")).await;
    assert_eq!(
        std::fs::read_to_string(server.directory.join("first_delivery")).unwrap(),
        "evt_msg_durable_hook_ocmini_turn_ended"
    );
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            let count: i64 = conn
                .query_row("SELECT COUNT(*) FROM hook_delivery", [], |r| r.get(0))
                .unwrap();
            if count == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn durable_completion_survives_a_crash_during_finalizer() {
    let mut provider = Provider::start(vec![text_script("settled", "")]).await;
    let after = r#"import json,sys,pathlib,time
json.load(sys.stdin)
pathlib.Path('finalizer_started').write_text('yes')
time.sleep(1.5)"#;
    let observer = r#"import json,sys,pathlib
r=json.load(sys.stdin)
pathlib.Path('recovered_event').write_text(json.dumps(r['events'][0]))"#;
    let mut config = hook_config(vec![registration("turn.end.after", after)]);
    config["ocmini"]["hooks"]["observers"] = json!([{"id":"recover-terminal","on":"ocmini.turn.ended","delivery":"durable","command":["python3","-c",observer]}]);
    let mut server = TestServer::start(&provider, config).await;
    let sid = server.session(allow()).await;
    let _response = server
        .post(
            &format!("/session/{sid}/prompt_async"),
            &prompt("msg_finalizer_crash", "work"),
        )
        .send()
        .await
        .unwrap();
    wait_file(&server.directory.join("finalizer_started")).await;
    // A crashed owner's lease must expire before a sibling can recover it.
    let db_path = server.root.path().join(".local/share/ocmini/ocmini.db");
    {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute(
            "UPDATE owner SET lease_expires=0 WHERE session_id=?1",
            [&sid],
        )
        .unwrap();
    }
    server.restart().await;
    wait_file(&server.directory.join("recovered_event")).await;
    let event: Value = serde_json::from_str(
        &std::fs::read_to_string(server.directory.join("recovered_event")).unwrap(),
    )
    .unwrap();
    assert_eq!(event["id"], "evt_msg_finalizer_crash_ocmini_turn_ended");
    assert_eq!(event["properties"]["reason"], "completed");
    assert_eq!(event["properties"]["finalizerError"], "interrupted");
    let _ = provider.request().await;
}

#[tokio::test]
async fn hooks_continue_same_turn_transform_context_and_finalize_before_idle() {
    let mut provider =
        Provider::start(vec![text_script("first", ""), text_script("second", "")]).await;
    let end = r#"import json,sys
r=json.load(sys.stdin)
print(json.dumps({'decision':'continue','feedback':'HOOK_FEEDBACK'} if r['context']['endAttempt']==1 else {}))"#;
    let after = r#"import json,sys,pathlib
r=json.load(sys.stdin)
pathlib.Path('finalized').write_text(r['input']['reason'])"#;
    let context = r#"import json,sys
r=json.load(sys.stdin)
print(json.dumps({'set':{'system':r['input']['system']+'\nHOOK_SYSTEM'}}))"#;
    let text = r#"import json,sys
r=json.load(sys.stdin)
print(json.dumps({'set':{'text':'EDITED '+r['input']['text']}}))"#;
    let server = TestServer::start(
        &provider,
        hook_config(vec![
            registration("turn.end.before", end),
            registration("turn.end.after", after),
            registration("model.context.transform", context),
            registration("message.text.complete", text),
        ]),
    )
    .await;
    let sid = server.session(allow()).await;
    let mut events = server.events().await;
    let response = server
        .post(
            &format!("/session/{sid}/message"),
            &prompt("msg_hook_continue", "work"),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let value = response.json::<Value>().await.unwrap();
    assert_eq!(answer(&value), "EDITED second");
    assert_eq!(
        std::fs::read_to_string(server.directory.join("finalized")).unwrap(),
        "completed"
    );
    let first = provider.request().await;
    let second = provider.request().await;
    assert!(first.to_string().contains("HOOK_SYSTEM"));
    assert!(second.to_string().contains("HOOK_FEEDBACK"));
    assert!(second.to_string().contains("EDITED first"));
    let all = events.through("session.idle").await;
    assert!(
        all.iter()
            .position(|e| e["type"] == "ocmini.turn.ended")
            .unwrap()
            < all
                .iter()
                .position(|e| e["type"] == "session.idle")
                .unwrap()
    );
    let messages = server.messages(&sid).await;
    let assistants = messages
        .iter()
        .filter(|m| m["info"]["role"] == "assistant")
        .collect::<Vec<_>>();
    assert_eq!(assistants.len(), 2);
    assert!(assistants
        .iter()
        .all(|m| m["info"]["parentID"] == "msg_hook_continue"));
    let duplicate = server
        .post(
            &format!("/session/{sid}/message"),
            &prompt("msg_hook_continue", "ignored retry"),
        )
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(duplicate["info"]["id"], value["info"]["id"]);
    assert!(provider.requests.try_recv().is_err());
}

#[tokio::test]
async fn end_gate_continuations_are_bounded() {
    let mut provider =
        Provider::start(vec![text_script("first", ""), text_script("second", "")]).await;
    let mut config = hook_config(vec![registration(
        "turn.end.before",
        r#"import json,sys
json.load(sys.stdin)
print('{"decision":"continue","feedback":"keep checking"}')"#,
    )]);
    config["ocmini"]["hooks"]["maxEndContinuations"] = json!(1);
    let server = TestServer::start(&provider, config).await;
    let sid = server.session(allow()).await;
    let response = server
        .post(
            &format!("/session/{sid}/message"),
            &prompt("msg_hook_limit", "work"),
        )
        .send()
        .await
        .unwrap();
    assert!(!response.status().is_success());
    assert!(response
        .text()
        .await
        .unwrap()
        .contains("continuation limit"));
    provider.request().await;
    provider.request().await;
    assert!(provider.requests.try_recv().is_err());
}

#[tokio::test]
async fn admission_abort_cannot_commit_an_idle_reservation() {
    let mut provider = Provider::start(vec![]).await;
    let script = r#"import json,sys,pathlib,time
json.load(sys.stdin)
pathlib.Path('admission-entered').touch()
time.sleep(60)"#;
    let server = TestServer::start(
        &provider,
        hook_config(vec![registration("prompt.accept.before", script)]),
    )
    .await;
    let sid = server.session(allow()).await;
    let pending = tokio::spawn(
        server
            .post(
                &format!("/session/{sid}/prompt_async"),
                &prompt("msg_hook_abort_admission", "work"),
            )
            .send(),
    );
    wait_file(&server.directory.join("admission-entered")).await;
    assert!(server
        .post(&format!("/session/{sid}/abort"), &json!({}))
        .send()
        .await
        .unwrap()
        .status()
        .is_success());
    let response = tokio::time::timeout(Duration::from_secs(4), pending)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(!response.status().is_success());
    assert!(server.messages(&sid).await.is_empty());
    assert!(provider.requests.try_recv().is_err());
}

#[tokio::test]
async fn input_hook_is_idempotent_and_no_reply_skips_turn_hooks() {
    let mut provider = Provider::start(vec![text_script("done", "")]).await;
    let script = r#"import json,sys,pathlib
r=json.load(sys.stdin)
p=pathlib.Path('admission-count')
p.write_text(str(int(p.read_text())+1) if p.exists() else '1')
print(json.dumps({'set':{'parts':[{'type':'text','text':'TRANSFORMED_INPUT'}]}}))"#;
    let server = TestServer::start(
        &provider,
        hook_config(vec![registration("prompt.accept.before", script)]),
    )
    .await;
    let sid = server.session(allow()).await;
    let request = prompt("msg_hook_input", "original");
    let (one, two) = tokio::join!(
        server
            .post(&format!("/session/{sid}/message"), &request)
            .send(),
        server
            .post(&format!("/session/{sid}/message"), &request)
            .send()
    );
    assert!(one.unwrap().status().is_success());
    assert!(two.unwrap().status().is_success());
    assert_eq!(
        std::fs::read_to_string(server.directory.join("admission-count")).unwrap(),
        "1"
    );
    assert!(provider
        .request()
        .await
        .to_string()
        .contains("TRANSFORMED_INPUT"));
    assert!(provider.requests.try_recv().is_err());
    let mut no_reply = prompt("msg_hook_context", "context");
    no_reply["noReply"] = json!(true);
    let response = server
        .post(&format!("/session/{sid}/message"), &no_reply)
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success());
    assert!(provider.requests.try_recv().is_err());
}

#[tokio::test]
async fn steering_is_admitted_while_completion_gate_waits() {
    let mut provider = Provider::start(vec![text_script("old", ""), text_script("new", "")]).await;
    let script = r#"import json,sys,pathlib,time
r=json.load(sys.stdin)
if r['context']['turnID']=='msg_hook_steer_old':
 pathlib.Path('gate-entered').touch()
 while not pathlib.Path('release-gate').exists(): time.sleep(.01)
 print('{"decision":"continue","feedback":"must lose to user steering"}')"#;
    let server = TestServer::start(
        &provider,
        hook_config(vec![registration("turn.end.before", script)]),
    )
    .await;
    let sid = server.session(allow()).await;
    assert_eq!(
        server
            .post(
                &format!("/session/{sid}/prompt_async"),
                &prompt("msg_hook_steer_old", "old")
            )
            .send()
            .await
            .unwrap()
            .status(),
        204
    );
    wait_file(&server.directory.join("gate-entered")).await;
    assert_eq!(
        server
            .post(
                &format!("/session/{sid}/prompt_async"),
                &prompt("msg_hook_steer_new", "new")
            )
            .send()
            .await
            .unwrap()
            .status(),
        204
    );
    std::fs::write(server.directory.join("release-gate"), "").unwrap();
    provider.request().await;
    let next = provider.request().await;
    assert!(!next.to_string().contains("must lose to user steering"));
    let mut events = server.events().await;
    // A completed duplicate waits for the exact second response even if events already drained.
    let value = server
        .post(
            &format!("/session/{sid}/message"),
            &prompt("msg_hook_steer_new", "retry"),
        )
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(answer(&value), "new");
    let _ = &mut events;
    assert!(provider.requests.try_recv().is_err());
}

#[tokio::test]
async fn file_hook_rewrites_are_authorized_and_persist_effective_arguments() {
    let mut provider = Provider::start(vec![
        tool_script(
            "write",
            json!({"filePath":"original.txt","content":"old"}),
            "call_hook_write",
        ),
        text_script("done", ""),
    ])
    .await;
    let script = r#"import json,sys,pathlib
r=json.load(sys.stdin)
c=r['input']['changes'];c[0]['path']=str(pathlib.Path(r['context']['directory'])/'rewritten.txt');c[0]['after']='HOOK_CONTENT'
print(json.dumps({'set':{'changes':c}}))"#;
    let server = TestServer::start(
        &provider,
        hook_config(vec![registration("file.change.before", script)]),
    )
    .await;
    let sid = server.session(allow()).await;
    let response = server
        .post(
            &format!("/session/{sid}/message"),
            &prompt("msg_hook_file", "write"),
        )
        .send()
        .await
        .unwrap();
    assert!(
        response.status().is_success(),
        "{}",
        response.text().await.unwrap()
    );
    provider.request().await;
    let final_request = provider.request().await;
    assert!(!server.directory.join("original.txt").exists());
    assert_eq!(
        std::fs::read_to_string(server.directory.join("rewritten.txt")).unwrap(),
        "HOOK_CONTENT"
    );
    assert!(final_request.to_string().contains("rewritten.txt"));
    let messages = server.messages(&sid).await;
    let part = messages
        .iter()
        .flat_map(|m| m["parts"].as_array().unwrap())
        .find(|p| p["type"] == "tool")
        .unwrap();
    assert!(part["state"]["input"].get("_ocminiFileChanges").is_none());
}

#[tokio::test]
async fn rewritten_tool_arguments_cannot_reuse_original_allow_rule() {
    let mut provider = Provider::start(vec![
        tool_script(
            "write",
            json!({"filePath":"original.txt","content":"old"}),
            "call_hook_deny",
        ),
        text_script("denied", ""),
    ])
    .await;
    let code = r#"import json,sys
r=json.load(sys.stdin);a=r['input']['args'];a['filePath']='private.txt';print(json.dumps({'set':{'args':a}}))"#;
    let server = TestServer::start(
        &provider,
        hook_config(vec![registration("tool.execute.before", code)]),
    )
    .await;
    let sid=server.session(json!([{"permission":"*","pattern":"*","action":"allow"},{"permission":"edit","pattern":"*private.txt","action":"deny"}])).await;
    assert!(server
        .post(
            &format!("/session/{sid}/message"),
            &prompt("msg_hook_deny", "write")
        )
        .send()
        .await
        .unwrap()
        .status()
        .is_success());
    provider.request().await;
    let next = provider.request().await;
    assert!(next.to_string().contains("denied"));
    assert!(!server.directory.join("private.txt").exists());
    assert!(!server.directory.join("original.txt").exists());
}

#[tokio::test]
async fn manual_compaction_has_dedicated_hooks_without_turn_end_gate() {
    let mut provider =
        Provider::start(vec![text_script("answer", ""), text_script("summary", "")]).await;
    let before = r#"import json,sys
json.load(sys.stdin);print('{"set":{"context":["HOOK_SUMMARIZER"]}}')"#;
    let after = r#"import json,sys
json.load(sys.stdin);print('{"set":{"summary":"HOOK_SUMMARY"}}')"#;
    let gate = r#"import json,sys
json.load(sys.stdin);print('{}')"#;
    let server = TestServer::start(
        &provider,
        hook_config(vec![
            registration("compaction.before", before),
            registration("compaction.after", after),
            registration("turn.end.before", gate),
        ]),
    )
    .await;
    let sid = server.session(allow()).await;
    assert!(server
        .post(
            &format!("/session/{sid}/message"),
            &prompt("msg_hook_summary", "work")
        )
        .send()
        .await
        .unwrap()
        .status()
        .is_success());
    provider.request().await;
    let result = server
        .post(&format!("/session/{sid}/summarize"), &json!({}))
        .send()
        .await
        .unwrap();
    assert!(
        result.status().is_success(),
        "{}",
        result.text().await.unwrap()
    );
    assert!(provider
        .request()
        .await
        .to_string()
        .contains("HOOK_SUMMARIZER"));
    let messages = server.messages(&sid).await;
    assert!(messages
        .iter()
        .any(|m| m["info"]["summary"] == true && answer(m) == "HOOK_SUMMARY"));
}

#[tokio::test]
async fn duplicate_waits_for_one_post_end_finalizer() {
    let mut provider = Provider::start(vec![text_script("done", "")]).await;
    let script = r#"import json,sys,pathlib,time
json.load(sys.stdin)
p=pathlib.Path('finalizer-count');p.write_text(str(int(p.read_text())+1) if p.exists() else '1')
pathlib.Path('post-entered').touch()
while not pathlib.Path('release-post').exists(): time.sleep(.01)"#;
    let server = TestServer::start(
        &provider,
        hook_config(vec![registration("turn.end.after", script)]),
    )
    .await;
    let sid = server.session(allow()).await;
    let request = prompt("msg_hook_finalize", "work");
    let pending = tokio::spawn(
        server
            .post(&format!("/session/{sid}/message"), &request)
            .send(),
    );
    wait_file(&server.directory.join("post-entered")).await;
    let duplicate = tokio::spawn(
        server
            .post(&format!("/session/{sid}/message"), &request)
            .send(),
    );
    tokio::time::sleep(Duration::from_millis(40)).await;
    assert!(!pending.is_finished());
    assert!(!duplicate.is_finished());
    let status = server
        .get("/session/status")
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(status[&sid]["type"], "busy");
    std::fs::write(server.directory.join("release-post"), "").unwrap();
    let one = pending
        .await
        .unwrap()
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    let two = duplicate
        .await
        .unwrap()
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(one["info"]["id"], two["info"]["id"]);
    assert_eq!(
        std::fs::read_to_string(server.directory.join("finalizer-count")).unwrap(),
        "1"
    );
    provider.request().await;
    assert!(provider.requests.try_recv().is_err());
}

#[tokio::test]
async fn observer_receives_one_terminal_event_and_sse_filter_is_exact() {
    let provider = Provider::start(vec![text_script("done", "")]).await;
    let observer = r#"import json,sys,pathlib
r=json.load(sys.stdin)
with pathlib.Path('observed.jsonl').open('a') as f:
 for e in r['events']: f.write(json.dumps(e)+'\n')"#;
    let mut config = hook_config(vec![registration(
        "turn.start.before",
        r#"import json,sys
json.load(sys.stdin);print('{}')"#,
    )]);
    config["ocmini"]["hooks"]["observers"] = json!([{"id":"observe-completion","on":["ocmini.turn.ended"],"command":["python3","-c",observer]}]);
    let server = TestServer::start(&provider, config).await;
    let sid = server.session(allow()).await;
    let mut filtered = server
        .client
        .get(format!("{}/event?types=ocmini.turn.ended", server.url))
        .send()
        .await
        .unwrap()
        .bytes_stream();
    assert!(server
        .post(
            &format!("/session/{sid}/message"),
            &prompt("msg_hook_observe", "work")
        )
        .send()
        .await
        .unwrap()
        .status()
        .is_success());
    wait_file(&server.directory.join("observed.jsonl")).await;
    let text = std::fs::read_to_string(server.directory.join("observed.jsonl")).unwrap();
    assert_eq!(
        text.lines().count(),
        1,
        "snapshot registered duplicate observers: {text}"
    );
    assert_eq!(
        serde_json::from_str::<Value>(text.lines().next().unwrap()).unwrap()["type"],
        "ocmini.turn.ended"
    );
    let bytes = tokio::time::timeout(Duration::from_secs(4), async {
        let mut text = String::new();
        while let Some(chunk) = filtered.next().await {
            text.push_str(&String::from_utf8_lossy(&chunk.unwrap()));
            if text.contains("ocmini.turn.ended") {
                break;
            }
        }
        text
    })
    .await
    .unwrap();
    assert!(!bytes.contains("message.part.delta"));
    assert!(!bytes.contains("session.idle"));
}

#[tokio::test]
async fn permission_hook_can_tighten_an_allow_without_executing_the_tool() {
    let mut provider = Provider::start(vec![
        tool_script(
            "write",
            json!({"filePath":"denied.txt","content":"bad"}),
            "call_hook_permission",
        ),
        text_script("denied", ""),
    ])
    .await;
    let code = r#"import json,sys
json.load(sys.stdin);print('{"set":{"decision":"deny"}}')"#;
    let mut handler = registration("permission.decide", code);
    handler["match"] = json!({"agent":["build"],"session":"root","model":["mock/test-model"]});
    let server = TestServer::start(&provider, hook_config(vec![handler])).await;
    let sid = server.session(allow()).await;
    assert!(server
        .post(
            &format!("/session/{sid}/message"),
            &prompt("msg_hook_permission", "write")
        )
        .send()
        .await
        .unwrap()
        .status()
        .is_success());
    provider.request().await;
    assert!(provider.request().await.to_string().contains("denied"));
    assert!(!server.directory.join("denied.txt").exists());
}

#[tokio::test]
async fn command_hooks_prepare_before_admission_and_settle_once() {
    let mut provider = Provider::start(vec![text_script("done", "")]).await;
    let before = r#"import json,sys
json.load(sys.stdin);print('{"set":{"arguments":"HOOK_ARGUMENT"}}')"#;
    let expanded = r#"import json,sys
r=json.load(sys.stdin);r['input']['parts'][0]['text']+=' EXPANDED_HOOK';print(json.dumps({'set':{'parts':r['input']['parts']}}))"#;
    let server = TestServer::start(
        &provider,
        hook_config(vec![
            registration("command.execute.before", before),
            registration("command.expand.after", expanded),
        ]),
    )
    .await;
    let sid = server.session(allow()).await;
    let mut events = server.events().await;
    let request = json!({"messageID":"msg_hook_command","command":"review","arguments":"original"});
    assert!(server
        .post(&format!("/session/{sid}/command"), &request)
        .send()
        .await
        .unwrap()
        .status()
        .is_success());
    let body = provider.request().await.to_string();
    assert!(body.contains("HOOK_ARGUMENT"));
    assert!(body.contains("EXPANDED_HOOK"));
    let all = events.through("session.idle").await;
    assert_eq!(
        all.iter()
            .filter(|e| e["type"] == "command.executed")
            .count(),
        1
    );
    assert!(server
        .post(&format!("/session/{sid}/command"), &request)
        .send()
        .await
        .unwrap()
        .status()
        .is_success());
    assert!(provider.requests.try_recv().is_err());
}

#[tokio::test]
async fn settled_turn_survives_restart_without_replaying_a_finalizer() {
    let mut provider = Provider::start(vec![text_script("saved", "")]).await;
    let script = r#"import json,sys,pathlib,time
json.load(sys.stdin)
p=pathlib.Path('restart-finalizer-count');p.write_text(str(int(p.read_text())+1) if p.exists() else '1')
pathlib.Path('restart-post-entered').touch()
end=time.monotonic()+5
while not pathlib.Path('restart-release').exists() and time.monotonic()<end: time.sleep(.01)"#;
    let mut server = TestServer::start(
        &provider,
        hook_config(vec![registration("turn.end.after", script)]),
    )
    .await;
    let sid = server.session(allow()).await;
    let request = prompt("msg_hook_restart", "work");
    let pending = tokio::spawn(
        server
            .post(&format!("/session/{sid}/message"), &request)
            .send(),
    );
    wait_file(&server.directory.join("restart-post-entered")).await;
    provider.request().await;
    server.child.kill().await.unwrap();
    server.child.wait().await.unwrap();
    let _ = pending.await;
    let db = rusqlite::Connection::open(server.root.path().join(".local/share/ocmini/ocmini.db"))
        .unwrap();
    db.execute("UPDATE owner SET lease_expires=0", []).unwrap();
    drop(db);
    let (child, url) = launch(server.root.path(), &server.directory).await;
    server.child = child;
    server.url = url;
    let response = server
        .post(&format!("/session/{sid}/message"), &request)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(answer(&response.json::<Value>().await.unwrap()), "saved");
    std::fs::write(server.directory.join("restart-release"), "").unwrap();
    assert_eq!(
        std::fs::read_to_string(server.directory.join("restart-finalizer-count")).unwrap(),
        "1"
    );
    assert!(provider.requests.try_recv().is_err());
}

#[tokio::test]
async fn no_reply_and_invalid_input_do_not_run_execution_hooks() {
    let mut provider = Provider::start(vec![]).await;
    let admission = r#"import json,sys,pathlib
json.load(sys.stdin);pathlib.Path('validated-admission').touch();print('{}')"#;
    let execution = r#"import json,sys,pathlib
json.load(sys.stdin);pathlib.Path('unexpected-execution').touch();print('{}')"#;
    let server = TestServer::start(
        &provider,
        hook_config(vec![
            registration("prompt.accept.before", admission),
            registration("turn.start.before", execution),
            registration("turn.end.before", execution),
            registration("turn.end.after", execution),
        ]),
    )
    .await;
    let sid = server.session(allow()).await;
    let invalid = json!({"messageID":"msg_hook_invalid","parts":[{"type":"text","text":7}]});
    assert!(!server
        .post(&format!("/session/{sid}/message"), &invalid)
        .send()
        .await
        .unwrap()
        .status()
        .is_success());
    assert!(!server.directory.join("validated-admission").exists());
    let mut no_reply = prompt("msg_hook_only_context", "context");
    no_reply["noReply"] = json!(true);
    assert!(server
        .post(&format!("/session/{sid}/message"), &no_reply)
        .send()
        .await
        .unwrap()
        .status()
        .is_success());
    assert!(server.directory.join("validated-admission").exists());
    assert!(!server.directory.join("unexpected-execution").exists());
    assert!(provider.requests.try_recv().is_err());
}

#[tokio::test]
async fn a_failed_text_hook_records_measured_usage_and_never_executes_tools() {
    let mut provider=Provider::start(vec![Script::ok(vec![
        json!({"choices":[{"index":0,"delta":{"content":"checking","tool_calls":[{"index":0,"id":"call_text_fail","type":"function","function":{"name":"write","arguments":"{\"filePath\":\"must-not-exist\",\"content\":\"bad\"}"}}]},"finish_reason":null}]}),
        json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}],"usage":usage()})
    ])]).await;
    let server = TestServer::start(
        &provider,
        hook_config(vec![registration(
            "message.text.complete",
            "import json,sys;json.load(sys.stdin);raise SystemExit(3)",
        )]),
    )
    .await;
    let sid = server.session(allow()).await;
    let response = server
        .post(
            &format!("/session/{sid}/message"),
            &prompt("msg_hook_text_fail", "work"),
        )
        .send()
        .await
        .unwrap();
    assert!(!response.status().is_success());
    provider.request().await;
    assert!(provider.requests.try_recv().is_err());
    assert!(!server.directory.join("must-not-exist").exists());
    let messages = server.messages(&sid).await;
    let assistant = messages
        .iter()
        .find(|m| m["info"]["role"] == "assistant")
        .unwrap();
    assert!(assistant["info"]["time"]["completed"].is_number());
    assert!(assistant["info"]["error"].is_object());
    assert!(assistant["parts"]
        .as_array()
        .unwrap()
        .iter()
        .any(|p| p["type"] == "step-finish" && p["tokens"]["output"].as_i64().unwrap_or(0) > 0));
    assert!(assistant["parts"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|p| p["type"] == "tool")
        .all(|p| p["state"]["status"] == "error"));
}
