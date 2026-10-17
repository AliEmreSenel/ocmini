use serde_json::{json, Value};
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};

struct TestServer {
    child: Child,
    url: String,
}

impl Drop for TestServer {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}

async fn boot() -> TestServer {
    let port: u16 = 20000 + rand::random::<u16>() % 40000;
    let home = format!("/tmp/ocmini-test-{}-{}", std::process::id(), port);
    std::fs::create_dir_all(format!("{home}/.config/opencode")).unwrap();
    std::fs::write(format!("{home}/.config/opencode/opencode.json"), "{}").unwrap();
    std::fs::create_dir_all(format!("{home}/.local/share")).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_ocmini"))
        .args(["serve", "--hostname=127.0.0.1", &format!("--port={port}")])
        .env("OPENCODE_SERVER_PASSWORD", "testpw")
        .env("HOME", &home)
        .env("XDG_DATA_HOME", format!("{home}/.local/share"))
        .env("XDG_CONFIG_HOME", format!("{home}/.config"))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let mut lines = BufReader::new(stdout).lines();
    let url = tokio::time::timeout(std::time::Duration::from_secs(15), async {
        loop {
            let line = lines.next_line().await.unwrap().unwrap_or_default();
            if let Some(i) = line.find("http://") {
                break line[i..].trim().trim_end_matches([')', ']', '.']).to_string();
            }
        }
    })
    .await
    .expect("banner timeout");
    let _ = lines;
    TestServer { child, url }
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .unwrap()
}

#[tokio::test]
async fn health_and_banner() {
    let s = boot().await;
    assert!(s.url.starts_with("http://127.0.0.1:"));
    let r = client()
        .get(format!("{}/global/health", s.url))
        .basic_auth("opencode", Some("testpw"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["healthy"], true);
    assert_eq!(v["version"], "1.19.2");
}

#[tokio::test]
async fn api_info_is_404() {
    let s = boot().await;
    let r = client()
        .get(format!("{}/api/info", s.url))
        .basic_auth("opencode", Some("testpw"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
}

#[tokio::test]
async fn auth_required() {
    let s = boot().await;
    let r = client().get(format!("{}/global/health", s.url)).send().await.unwrap();
    assert_eq!(r.status(), 401);
}

#[tokio::test]
async fn session_lifecycle() {
    let s = boot().await;
    let c = client();
    let r = c
        .post(format!("{}/session", s.url))
        .basic_auth("opencode", Some("testpw"))
        .header("x-opencode-directory", "/tmp")
        .json(&json!({"title": "t"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let v: Value = r.json().await.unwrap();
    let id = v["id"].as_str().unwrap().to_string();
    assert!(id.starts_with("ses_"));

    let r = c
        .get(format!("{}/session/{}", s.url, id))
        .basic_auth("opencode", Some("testpw"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);

    let r = c
        .get(format!("{}/session", s.url))
        .basic_auth("opencode", Some("testpw"))
        .header("x-opencode-directory", "/tmp")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let v: Value = r.json().await.unwrap();
    assert!(v.as_array().unwrap().iter().any(|s| s["id"] == json!(id)));

    let r = c
        .delete(format!("{}/session/{}", s.url, id))
        .basic_auth("opencode", Some("testpw"))
        .send()
        .await
        .unwrap();
    assert!(r.status() == 200 || r.status() == 204);

    let r = c
        .get(format!("{}/session/{}", s.url, id))
        .basic_auth("opencode", Some("testpw"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
}

#[tokio::test]
async fn prompt_async_requires_model() {
    let s = boot().await;
    let c = client();
    let r = c
        .post(format!("{}/session", s.url))
        .basic_auth("opencode", Some("testpw"))
        .header("x-opencode-directory", "/tmp")
        .json(&json!({"title": "t"}))
        .send()
        .await
        .unwrap();
    let v: Value = r.json().await.unwrap();
    let id = v["id"].as_str().unwrap().to_string();
    let r = c
        .post(format!("{}/session/{}/prompt_async", s.url, id))
        .basic_auth("opencode", Some("testpw"))
        .json(&json!({"parts": [{"type": "text", "text": "hi"}]}))
        .send()
        .await
        .unwrap();
    assert!(r.status().as_u16() >= 400 && r.status().as_u16() < 500);
}

#[tokio::test]
async fn sse_first_event() {
    let s = boot().await;
    use futures::StreamExt;
    let r = client()
        .get(format!("{}/event", s.url))
        .basic_auth("opencode", Some("testpw"))
        .send()
        .await
        .unwrap();
    let mut stream = r.bytes_stream();
    let mut acc: Vec<u8> = Vec::new();
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while let Some(chunk) = stream.next().await {
            acc.extend_from_slice(&chunk.unwrap());
            if String::from_utf8_lossy(&acc).contains("\n\n") {
                break;
            }
        }
    })
    .await
    .expect("no sse data");
    let text = String::from_utf8_lossy(&acc).to_string();
    let data = text.lines().find(|l| l.starts_with("data:")).unwrap();
    let v: Value = serde_json::from_str(data[5..].trim()).unwrap();
    assert_eq!(v["type"], "server.connected");
}

#[tokio::test]
async fn mcp_add_and_status() {
    let s = boot().await;
    let c = client();
    let r = c
        .get(format!("{}/mcp", s.url))
        .basic_auth("opencode", Some("testpw"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let r = c
        .post(format!("{}/mcp", s.url))
        .basic_auth("opencode", Some("testpw"))
        .json(&json!({"name": "x", "config": {"type": "remote", "url": "http://127.0.0.1:9/none"}}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let r = c
        .get(format!("{}/mcp", s.url))
        .basic_auth("opencode", Some("testpw"))
        .send()
        .await
        .unwrap();
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["x"]["status"], "connected");
}

#[tokio::test]
async fn catalog_endpoints_ok() {
    let s = boot().await;
    let c = client();
    for path in ["/provider", "/agent", "/skill", "/command", "/permission", "/question"] {
        let r = c
            .get(format!("{}{}", s.url, path))
            .basic_auth("opencode", Some("testpw"))
            .header("x-opencode-directory", "/tmp")
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200, "{path}");
    }
}

#[tokio::test]
async fn permission_routes() {
    let s = boot().await;
    let c = client();
    let sid = c
        .post(format!("{}/session", s.url))
        .basic_auth("opencode", Some("testpw"))
        .header("x-opencode-directory", "/tmp")
        .json(&json!({}))
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let rules = json!([{"permission": "bash", "pattern": "*", "action": "ask"}, {"permission": "read", "pattern": "*", "action": "allow"}]);
    let r = c
        .patch(format!("{}/session/{sid}", s.url))
        .basic_auth("opencode", Some("testpw"))
        .json(&json!({"permission": rules}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let got = c
        .get(format!("{}/session/{sid}", s.url))
        .basic_auth("opencode", Some("testpw"))
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(got["permission"], rules);
    let list = c
        .get(format!("{}/permission", s.url))
        .basic_auth("opencode", Some("testpw"))
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(list, json!([]));
    let r = c
        .post(format!("{}/permission/nope/reply", s.url))
        .basic_auth("opencode", Some("testpw"))
        .json(&json!({"reply": "once"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
    let r = c
        .post(format!("{}/session/{sid}/permissions/nope", s.url))
        .basic_auth("opencode", Some("testpw"))
        .json(&json!({"response": "reject"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
}
