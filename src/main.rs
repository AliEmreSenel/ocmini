mod auth;
mod bus;
mod config;
mod engine;
mod engine_impl;
mod mcp;
mod permission;
mod provider;
mod server;
mod store;
mod tools;
mod tui;
mod types;
mod usage_db;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        tui::run();
        return;
    }
    let cmd = args.first().map(String::as_str).unwrap_or("--help");
    match cmd {
        "serve" => {
            let mut hostname = "127.0.0.1".to_string();
            let mut port: u16 = 4096;
            for a in args.iter().skip(1) {
                if let Some(v) = a.strip_prefix("--hostname=") {
                    hostname = v.to_string();
                } else if let Some(v) = a.strip_prefix("--port=") {
                    port = v.parse().expect("bad port");
                }
            }
            let t0 = std::time::Instant::now();
            let rt = tokio::runtime::Runtime::new().unwrap();
            if std::env::var("OC_DEBUG_STARTUP").is_ok() {
                eprintln!("[startup] runtime: {:?}", t0.elapsed());
            }
            rt.block_on(server::serve(hostname, port));
        }
        "--version" | "version" => println!("1.19.2"),
        "models" => {
            let cfg = config::Config::load();
            for m in cfg.catalog() {
                println!(
                    "{}/{}",
                    m["providerID"].as_str().unwrap_or_default(),
                    m["modelID"].as_str().unwrap_or_default()
                );
                let mut body = m.clone();
                if let Some(o) = body.as_object_mut() {
                    let pid = o.remove("providerID").unwrap_or(serde_json::json!(""));
                    let mid = o.remove("modelID").unwrap_or(serde_json::json!(""));
                    o.insert("id".into(), mid.clone());
                    o.insert("providerID".into(), pid);
                }
                println!("{}", serde_json::to_string_pretty(&body).unwrap());
            }
        }
        "agent" => {
            println!("build (primary)");
            println!(r#"[{{"permission":"*","pattern":"*","action":"allow"}},{{"permission":"doom_loop","pattern":"*","action":"ask"}},{{"permission":"external_directory","pattern":"*","action":"ask"}},{{"permission":"question","pattern":"*","action":"deny"}},{{"permission":"plan_enter","pattern":"*","action":"deny"}},{{"permission":"plan_exit","pattern":"*","action":"deny"}},{{"permission":"read","pattern":"*","action":"allow"}},{{"permission":"read","pattern":"*.env","action":"ask"}},{{"permission":"read","pattern":"*.env.*","action":"ask"}}]"#);
            println!("plan (primary)");
            println!(r#"[{{"permission":"*","pattern":"*","action":"allow"}},{{"permission":"doom_loop","pattern":"*","action":"ask"}},{{"permission":"external_directory","pattern":"*","action":"ask"}}]"#);
            println!("general (subagent)");
            println!(r#"[{{"permission":"*","pattern":"*","action":"allow"}},{{"permission":"doom_loop","pattern":"*","action":"ask"}}]"#);
            println!("subagent (subagent)");
            println!(r#"[{{"permission":"*","pattern":"*","action":"allow"}},{{"permission":"doom_loop","pattern":"*","action":"ask"}}]"#);
        }
        "debug" => {
            let skills: Vec<serde_json::Value> = server::skills_json()
                .as_array()
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .map(|s| {
                    serde_json::json!({
                        "name": s["name"],
                        "description": s["description"],
                        "location": s["location"],
                    })
                })
                .collect();
            println!("{}", serde_json::to_string(&skills).unwrap());
        }
        _ => eprintln!("ocmini: usage: serve --hostname=H --port=P | --version | models --verbose | agent list | debug skill"),
    }
}
