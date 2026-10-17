use rand::Rng;
use serde_json::{json, Value};
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::{Child, Command};

struct Server {
    child: Child,
    url: String,
}

async fn spawn_server(bin: &str, home: &str, port: u16) -> Server {
    std::fs::create_dir_all(format!("{home}/.config/opencode")).unwrap();
    std::fs::write(format!("{home}/.config/opencode/opencode.json"), "{}").unwrap();
    std::fs::create_dir_all(format!("{home}/.local/share")).unwrap();
    let mut child = Command::new(bin)
        .args(["serve", &format!("--hostname=127.0.0.1"), &format!("--port={port}")])
        .env("OPENCODE_SERVER_PASSWORD", "fuzzpw")
        .env("HOME", home)
        .env("XDG_DATA_HOME", format!("{home}/.local/share"))
        .env("XDG_CONFIG_HOME", format!("{home}/.config"))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap_or_else(|e| panic!("spawn {bin}: {e}"));
    let stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    let url = loop {
        line.clear();
        if std::time::Instant::now() > deadline {
            panic!("no banner from {bin}");
        }
        let n = reader.read_line(&mut line).await.unwrap();
        if n == 0 {
            continue;
        }
        if let Some(i) = line.find("http://") {
            let u = line[i..].trim().trim_end_matches([')', ']', '.']).to_string();
            break u;
        }
    };
    Server { child, url }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .unwrap()
}

fn shape(v: &Value) -> Value {
    match v {
        Value::Object(m) => {
            let mut o = serde_json::Map::new();
            for (k, val) in m {
                o.insert(k.clone(), shape(val));
            }
            Value::Object(o)
        }
        Value::Array(a) => match a.first() {
            Some(f) => Value::Array(vec![shape(f)]),
            None => Value::Array(vec![]),
        },
        Value::String(_) => json!("s"),
        Value::Number(n) => {
            if n.is_i64() || n.is_u64() {
                json!(0)
            } else {
                json!(0.0)
            }
        }
        other => other.clone(),
    }
}


fn diff(a: &Value, b: &Value, path: &str, out: &mut Vec<String>) {
    match (a, b) {
        (Value::Object(x), Value::Object(y)) => {
            for k in x.keys() {
                if !y.contains_key(k) {
                    out.push(format!("{path}.{k}: missing in ocmini"));
                }
            }
            for k in y.keys() {
                if !x.contains_key(k) {
                    out.push(format!("{path}.{k}: extra in ocmini"));
                }
            }
            for k in x.keys() {
                if let (Some(xv), Some(yv)) = (x.get(k), y.get(k)) {
                    diff(xv, yv, &format!("{path}.{k}"), out);
                }
            }
        }
        (Value::Array(x), Value::Array(y)) => {
            if let (Some(xf), Some(yf)) = (x.first(), y.first()) {
                diff(xf, yf, &format!("{path}[0]"), out);
            }
        }
        (Value::Number(_), Value::Number(_)) => {}
        (x, y) => {
            if std::mem::discriminant(x) != std::mem::discriminant(y) {
                out.push(format!("{path}: type {:?} vs {:?}", x, y));
            }
        }
    }
}

async fn get(c: &reqwest::Client, url: &str, path: &str, dir: &str) -> (u16, Value) {
    let r = c
        .get(format!("{url}{path}"))
        .basic_auth("opencode", Some("fuzzpw"))
        .header("x-opencode-directory", dir)
        .send()
        .await
        .unwrap();
    let st = r.status().as_u16();
    let body = r.text().await.unwrap_or_default();
    (st, serde_json::from_str(&body).unwrap_or(Value::String(body)))
}

async fn post(c: &reqwest::Client, url: &str, path: &str, dir: &str, body: Value) -> (u16, Value) {
    let r = c
        .post(format!("{url}{path}"))
        .basic_auth("opencode", Some("fuzzpw"))
        .header("x-opencode-directory", dir)
        .json(&body)
        .send()
        .await
        .unwrap();
    let st = r.status().as_u16();
    let text = r.text().await.unwrap_or_default();
    (st, serde_json::from_str(&text).unwrap_or(Value::String(text)))
}

#[tokio::main]
async fn main() {
    let port_a: u16 = rand::rng().random_range(21000..29000);
    let port_b: u16 = rand::rng().random_range(31000..39000);
    let mut oracle = spawn_server("/usr/bin/opencode", &format!("/tmp/fuzzhome-o{port_a}"), port_a).await;
    let mut shim = spawn_server("/mnt/data/cargo-target/debug/ocmini", &format!("/tmp/fuzzhome-s{port_b}"), port_b).await;
    let c = client();
    let dir = "/tmp";
    let mut fails = 0u32;

    // 1 health
    let (sa, ba) = get(&c, &oracle.url, "/global/health", dir).await;
    let (sb, bb) = get(&c, &shim.url, "/global/health", dir).await;
    check(&mut fails, "GET /global/health", sa, sb, &ba, &bb);

    // 2 api/info
    let (sa, ba) = get(&c, &oracle.url, "/api/info", dir).await;
    let (sb, bb) = get(&c, &shim.url, "/api/info", dir).await;
    check(&mut fails, "GET /api/info", sa, sb, &ba, &bb);

    // 3 create session
    let (sa, ba) = post(&c, &oracle.url, "/session", dir, json!({"title": "fuzz"})).await;
    let (sb, bb) = post(&c, &shim.url, "/session", dir, json!({"title": "fuzz"})).await;
    check(&mut fails, "POST /session", sa, sb, &ba, &bb);
    let sid_a = ba["id"].as_str().unwrap_or_default().to_string();
    let sid_b = bb["id"].as_str().unwrap_or_default().to_string();

    for path in [
        format!("/session/{sid_a}"),
        "/session".to_string(),
        format!("/session/{sid_a}/message"),
        format!("/session/{sid_a}/children"),
    ] {
        let path_b = path.replace(&sid_a, &sid_b);
        let (sa, ba) = get(&c, &oracle.url, &path, dir).await;
        let (sb, bb) = get(&c, &shim.url, &path_b, dir).await;
        check(&mut fails, &format!("GET {path}"), sa, sb, &ba, &bb);
    }

    // 4 abort
    let (sa, ba) = post(&c, &oracle.url, &format!("/session/{sid_a}/abort"), dir, json!({})).await;
    let (sb, bb) = post(&c, &shim.url, &format!("/session/{sid_b}/abort"), dir, json!({})).await;
    check(&mut fails, "POST /session/:id/abort", sa, sb, &ba, &bb);

    // 5 catalog endpoints
    for path in ["/agent", "/skill", "/command", "/mcp", "/permission", "/question"] {
        let (sa, ba) = get(&c, &oracle.url, path, dir).await;
        let (sb, bb) = get(&c, &shim.url, path, dir).await;
        check(&mut fails, &format!("GET {path}"), sa, sb, &ba, &bb);
    }

    // 6 provider: status + top-level keys + item key sets (catalog depth intentionally reduced)
    let (sa, ba) = get(&c, &oracle.url, "/provider", dir).await;
    let (sb, bb) = get(&c, &shim.url, "/provider", dir).await;
    let mut out = Vec::new();
    if sa != sb {
        out.push(format!("status {sa} vs {sb}"));
    }
    for k in ["all", "connected", "default"] {
        let (x, y) = (ba.get(k).is_some(), bb.get(k).is_some());
        if x != y {
            out.push(format!("{k}: presence {x} vs {y}"));
        }
    }
    let item_keys = |v: &Value| -> Vec<String> {
        v["all"]
            .as_array()
            .and_then(|a| a.first().cloned())
            .map(|f| f.as_object().map(|o| o.keys().cloned().collect()).unwrap_or_default())
            .unwrap_or_default()
    };
    let (ka, kb) = (item_keys(&ba), item_keys(&bb));
    if !ka.is_empty() && !kb.is_empty() {
        let mut d = Vec::new();
        diff(&json!({"k": ka}), &json!({"k": kb}), "", &mut d);
        out.extend(d);
    }
    if !out.is_empty() {
        fails += 1;
        println!("FAIL GET /provider (top-level): {}", out.join("; "));
    } else {
        println!("PASS GET /provider (top-level)");
    }

    // 6 SSE first event
    use futures::StreamExt;
    for (label, srv) in [("oracle", &oracle), ("shim", &shim)] {
        let r = c
            .get(format!("{}{}", srv.url, "/event"))
            .basic_auth("opencode", Some("fuzzpw"))
            .header("x-opencode-directory", dir)
            .send()
            .await
            .unwrap();
        let mut stream = r.bytes_stream();
        let mut acc: Vec<u8> = Vec::new();
        let got = tokio::time::timeout(std::time::Duration::from_secs(12), async {
            while let Some(chunk) = stream.next().await {
                match chunk {
                    Ok(b) => acc.extend_from_slice(&b),
                    Err(_) => break,
                }
                if String::from_utf8_lossy(&acc).contains("\n\n") {
                    break;
                }
            }
        })
        .await;
        if got.is_err() {
            println!("FAIL {} SSE /event: timeout", label);
            fails += 1;
            continue;
        }
        let text = String::from_utf8_lossy(&acc).to_string();
        let first = text
            .lines()
            .find(|l| l.starts_with("data:"))
            .map(|l| l[5..].trim().to_string())
            .unwrap_or_default();
        let v: Value = serde_json::from_str(&first).unwrap_or(Value::Null);
        let ok = v["type"] == "server.connected" && v.get("id").is_some() && v.get("properties").is_some();
        println!("{} SSE /event: {}", if ok { "PASS" } else { "FAIL" }, label);
        if !ok {
            fails += 1;
            println!("  body: {first}");
        }
    }

    println!("\n{} failures", fails);
    std::process::exit(if fails == 0 { 0 } else { 1 });
}

fn check(fails: &mut u32, label: &str, sa: u16, sb: u16, ba: &Value, bb: &Value) {
    let mut out = Vec::new();
    if sa != sb {
        let ok = label == "GET /api/info" && sa == 200 && sb == 404;
        if !ok {
            out.push(format!("status {sa} vs {sb}"));
        }
    }
    let (ma, mb) = if label.starts_with("GET /provider") {
        let mut x = ba.clone();
        let mut y = bb.clone();
        x.as_object_mut().map(|m| m.remove("default"));
        y.as_object_mut().map(|m| m.remove("default"));
        (x, y)
    } else {
        (ba.clone(), bb.clone())
    };
    diff(&shape(&ma), &shape(&mb), "", &mut out);
    if out.is_empty() {
        println!("PASS {label}");
    } else {
        *fails += 1;
        println!("FAIL {label}: {}", out.join("; "));
        println!("  oracle: {}", ba.to_string().chars().take(300).collect::<String>());
        println!("  shim:   {}", bb.to_string().chars().take(300).collect::<String>());
    }
}
