use crate::engine::{bus, App, EngineApi};
use crate::types::{hex_id, ToolError, ToolOutput};
use serde_json::{json, Value};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

pub struct ToolCtx {
    pub dir: String,
    pub app: App,
    pub session_id: String,
    pub engine: Arc<dyn EngineApi>,
    pub cancel: Arc<AtomicBool>,
}

pub fn tool_defs() -> Vec<Value> {
    vec![
        fn_def("bash", "Run a shell command in the working directory", json!({"type":"object","properties":{"command":{"type":"string"},"timeout":{"type":"number"}},"required":["command"]})),
        fn_def("read", "Read a file. Returns lines prefixed with line numbers.", json!({"type":"object","properties":{"filePath":{"type":"string"},"offset":{"type":"number"},"limit":{"type":"number"}},"required":["filePath"]})),
        fn_def("write", "Write a file, creating parent directories.", json!({"type":"object","properties":{"filePath":{"type":"string"},"content":{"type":"string"}},"required":["filePath","content"]})),
        fn_def("edit", "Exact string replacement in a file.", json!({"type":"object","properties":{"filePath":{"type":"string"},"oldString":{"type":"string"},"newString":{"type":"string"},"replaceAll":{"type":"boolean"}},"required":["filePath","oldString","newString"]})),
        fn_def("glob", "Find files by glob pattern, sorted by mtime.", json!({"type":"object","properties":{"pattern":{"type":"string"},"path":{"type":"string"}},"required":["pattern"]})),
        fn_def("grep", "Search file contents by regex.", json!({"type":"object","properties":{"pattern":{"type":"string"},"path":{"type":"string"},"include":{"type":"string"}},"required":["pattern"]})),
        fn_def("webfetch", "Fetch a URL and return its text content.", json!({"type":"object","properties":{"url":{"type":"string"},"format":{"type":"string","enum":["text","markdown","html"]}},"required":["url"]})),
        fn_def("todowrite", "Replace the todo list.", json!({"type":"object","properties":{"todos":{"type":"array","items":{"type":"object","properties":{"content":{"type":"string"},"status":{"type":"string","enum":["pending","in_progress","completed","cancelled"]},"priority":{"type":"string","enum":["high","medium","low"]}},"required":["content","status","priority"]}}},"required":["todos"]})),
        fn_def("question", "Ask the user a question and wait for the answer.", json!({"type":"object","properties":{"questions":{"type":"array","items":{"type":"object"}}},"required":["questions"]})),
        fn_def("task", "Delegate a task to a subagent session and return its result.", json!({"type":"object","properties":{"description":{"type":"string"},"prompt":{"type":"string"},"subagent_type":{"type":"string"}},"required":["description","prompt","subagent_type"]})),
    ]
}

fn fn_def(name: &str, desc: &str, params: Value) -> Value {
    json!({"type": "function", "function": {"name": name, "description": desc, "parameters": params}})
}

fn cap_out(s: &str, max: usize) -> String {
    let bytes = s.as_bytes();
    if bytes.len() <= max {
        return s.to_string();
    }
    let half = max / 2;
    let mut head = half;
    while head > 0 && !s.is_char_boundary(head) {
        head -= 1;
    }
    let mut tail = bytes.len() - half;
    while tail < bytes.len() && !s.is_char_boundary(tail) {
        tail += 1;
    }
    format!("{}\n\n[... {} bytes truncated ...]\n{}", &s[..head], tail - head, &s[tail..])
}

pub async fn run_tool(ctx: &ToolCtx, name: &str, input: &Value) -> Result<ToolOutput, ToolError> {
    let err = |m: String| ToolError { message: m, denied: false };
    match name {
        "bash" => {
            let cmd = input["command"].as_str().ok_or_else(|| err("no command".into()))?;
            let timeout = input["timeout"].as_u64().unwrap_or(120);
            let out = tokio::time::timeout(
                std::time::Duration::from_secs(timeout),
                tokio::process::Command::new("bash")
                    .arg("-c")
                    .arg(cmd)
                    .current_dir(&ctx.dir)
                    .output(),
            )
            .await
            .map_err(|_| err(format!("command timed out after {timeout}s")))?
            .map_err(|e| err(format!("spawn failed: {e}")))?;
            let mut text = String::from_utf8_lossy(&out.stdout).to_string();
            let se = String::from_utf8_lossy(&out.stderr).to_string();
            if !se.is_empty() {
                text.push_str(&format!("\n[stderr]\n{se}"));
            }
            if !out.status.success() {
                text.push_str(&format!("\n[exit code: {}]", out.status.code().unwrap_or(-1)));
            }
            Ok(ToolOutput { output: cap_out(&text, 50 * 1024), title: cmd.chars().take(60).collect() })
        }
        "read" => {
            let path = input["filePath"].as_str().ok_or_else(|| err("no filePath".into()))?;
            let content = std::fs::read_to_string(path).map_err(|e| err(format!("read failed: {e}")))?;
            let offset = input["offset"].as_u64().unwrap_or(1).max(1) as usize;
            let limit = input["limit"].as_u64().unwrap_or(2000) as usize;
            let lines: Vec<&str> = content.lines().collect();
            let mut text = lines
                .iter()
                .copied()
                .enumerate()
                .filter(|(i, _)| *i + 1 >= offset && *i + 1 < offset + limit)
                .map(|(i, l)| format!("{}: {}", i + 1, l))
                .collect::<Vec<_>>()
                .join("\n");
            let remaining = lines.len().saturating_sub(offset + limit - 1);
            if remaining > 0 {
                text.push_str(&format!("\n[truncated: {remaining} more lines]"));
            }
            Ok(ToolOutput { output: cap_out(&text, 200 * 1024), title: path.to_string() })
        }
        "write" => {
            let path = input["filePath"].as_str().ok_or_else(|| err("no filePath".into()))?;
            let content = input["content"].as_str().ok_or_else(|| err("no content".into()))?;
            if let Some(p) = std::path::Path::new(path).parent() {
                std::fs::create_dir_all(p).ok();
            }
            std::fs::write(path, content).map_err(|e| err(format!("write failed: {e}")))?;
            Ok(ToolOutput { output: format!("wrote {path}"), title: path.to_string() })
        }
        "edit" => {
            let path = input["filePath"].as_str().ok_or_else(|| err("no filePath".into()))?;
            let old = input["oldString"].as_str().ok_or_else(|| err("no oldString".into()))?;
            let new = input["newString"].as_str().ok_or_else(|| err("no newString".into()))?;
            let all = input["replaceAll"].as_bool().unwrap_or(false);
            let raw = std::fs::read_to_string(path).map_err(|e| err(format!("read failed: {e}")))?;
            let crlf = raw.contains("\r\n");
            let content = if crlf { raw.replace("\r\n", "\n") } else { raw.clone() };
            let old_n = old.replace("\r\n", "\n");
            let new_n = new.replace("\r\n", "\n");
            let count = content.matches(old_n.as_str()).count();
            if count == 0 {
                return Err(err("oldString not found in file".into()));
            }
            if count > 1 && !all {
                return Err(err(format!("found {count} matches for oldString, not unique")));
            }
            let updated = if all {
                content.replace(old_n.as_str(), new_n.as_str())
            } else {
                content.replacen(old_n.as_str(), new_n.as_str(), 1)
            };
            let out = if crlf { updated.replace("\n", "\r\n") } else { updated };
            std::fs::write(path, out).map_err(|e| err(format!("write failed: {e}")))?;
            Ok(ToolOutput { output: format!("edited {path}"), title: path.to_string() })
        }
        "glob" => {
            let pattern = input["pattern"].as_str().ok_or_else(|| err("no pattern".into()))?;
            let base = input["path"].as_str().unwrap_or(&ctx.dir);
            let mut files: Vec<(std::time::SystemTime, String)> = Vec::new();
            for entry in glob::glob(&format!("{base}/{pattern}")).map_err(|e| err(e.to_string()))? {
                if let Ok(p) = entry {
                    if !p.is_file() {
                        continue;
                    }
                    if p.components().any(|c| c.as_os_str().to_string_lossy().starts_with('.')) {
                        continue;
                    }
                    let mt = p.metadata().and_then(|m| m.modified()).unwrap_or(std::time::SystemTime::UNIX_EPOCH);
                    files.push((mt, p.to_string_lossy().to_string()));
                }
            }
            files.sort_by(|a, b| b.0.cmp(&a.0));
            let out = files.into_iter().take(100).map(|(_, p)| p).collect::<Vec<_>>().join("\n");
            Ok(ToolOutput { output: out, title: pattern.to_string() })
        }
        "grep" => {
            const SKIP_DIRS: [&str; 3] = [".git", "node_modules", "target"];
            let pattern = input["pattern"].as_str().ok_or_else(|| err("no pattern".into()))?;
            let base = input["path"].as_str().unwrap_or(&ctx.dir).to_string();
            let include = input["include"].as_str().unwrap_or("").to_string();
            let re = regex::Regex::new(pattern).map_err(|e| err(format!("bad regex: {e}")))?;
            let mut out = String::new();
            let mut hits = 0usize;
            for entry in walkdir::WalkDir::new(&base).into_iter().filter_map(|e| e.ok()) {
                if hits >= 100 {
                    break;
                }
                let p = entry.path();
                if !p.is_file() {
                    continue;
                }
                if p.components().any(|c| SKIP_DIRS.contains(&c.as_os_str().to_string_lossy().as_ref())) {
                    continue;
                }
                if !include.is_empty()
                    && !p.file_name().map(|n| glob::Pattern::new(&include).map(|g| g.matches(&n.to_string_lossy())).unwrap_or(false)).unwrap_or(false)
                {
                    continue;
                }
                let raw = match std::fs::read(p) {
                    Ok(r) => r,
                    Err(_) => continue,
                };
                if raw.contains(&0) {
                    continue;
                }
                let content = String::from_utf8_lossy(&raw);
                for (i, line) in content.lines().enumerate() {
                    if re.is_match(line) {
                        out.push_str(&format!("{}:{}:{}\n", p.display(), i + 1, line));
                        hits += 1;
                        if hits >= 100 {
                            break;
                        }
                    }
                }
            }
            Ok(ToolOutput { output: out, title: pattern.to_string() })
        }
        "webfetch" => {
            let url = input["url"].as_str().ok_or_else(|| err("no url".into()))?;
            let client = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .map_err(|e| err(format!("client failed: {e}")))?;
            let resp = client.get(url).send().await.map_err(|e| err(format!("fetch failed: {e}")))?;
            let body = resp.text().await.map_err(|e| err(format!("read failed: {e}")))?;
            Ok(ToolOutput { output: cap_out(&body, 200 * 1024), title: url.to_string() })
        }
        "todowrite" => {
            let todos = input["todos"].clone();
            let out = todos
                .as_array()
                .map(|a| a.iter().map(|t| format!("[{}] {}", t["status"], t["content"])).collect::<Vec<_>>().join("\n"))
                .unwrap_or_default();
            Ok(ToolOutput { output: out, title: "todos".into() })
        }
        "question" => {
            let qs = input["questions"].as_array().cloned().unwrap_or_default();
            let first = qs.first().cloned().unwrap_or(json!([]));
            let (req, rx) = ctx.app.questions.ask(&ctx.session_id, first);
            bus().emit("question.asked", json!({"id": req.id, "sessionID": req.session_id, "questions": req.questions}));
            let answers = rx.await.map_err(|_| ToolError { message: "question cancelled".into(), denied: false })?;
            let text = answers
                .map(|a| a.iter().map(|v| v.join(", ")).collect::<Vec<_>>().join("\n"))
                .unwrap_or_else(|| "user dismissed".into());
            Ok(ToolOutput { output: text, title: "question".into() })
        }
        "task" => {
            let prompt = input["prompt"].as_str().ok_or_else(|| err("no prompt".into()))?;
            let desc = input["description"].as_str().unwrap_or("task");
            let parent = ctx
                .app
                .store
                .get_session(&ctx.session_id)
                .ok_or_else(|| err("no session".into()))?;
            let (provider, model) = ctx
                .app
                .store
                .messages(&ctx.session_id)
                .into_iter()
                .rev()
                .find_map(|m| {
                    if m.info.role == "user" {
                        Some((m.info.provider_id?, m.info.model_id?))
                    } else {
                        None
                    }
                })
                .unwrap_or_default();
            let child = crate::types::Session {
                id: hex_id("ses_"),
                slug: crate::types::slug(),
                project_id: parent.project_id.clone(),
                directory: parent.directory.clone(),
                path: parent.path.clone(),
                cost: 0.0,
                tokens: crate::types::Tokens::default(),
                title: format!("[{desc}]"),
                version: parent.version.clone(),
                time: crate::types::Time { created: crate::types::now_ms(), updated: None },
                parent_id: Some(parent.id.clone()),
                permission: None,
            };
            ctx.app.store.create_session(&child);
            bus().emit("session.created", json!({"info": child}));
            let res = ctx
                .engine
                .run_sync(ctx.app.clone(), child.id.clone(), model, provider, prompt.to_string(), Some("general".into()))
                .await
                .map_err(|e| ToolError { message: e, denied: false })?;
            let text = res["parts"]
                .as_array()
                .map(|ps| ps.iter().filter(|p| p["type"] == "text").map(|p| p["text"].as_str().unwrap_or_default()).collect::<Vec<_>>().join(""))
                .unwrap_or_else(|| res.to_string());
            Ok(ToolOutput { output: text, title: desc.to_string() })
        }
        _ => {
            if let Some((server, tool)) = ctx.app.mcp.route(name) {
                return ctx
                    .app
                    .mcp
                    .call(&server, &tool, input.clone())
                    .map(|output| ToolOutput { output, title: format!("{server}/{tool}") })
                    .map_err(|e| ToolError { message: e, denied: false });
            }
            Err(err(format!("unknown tool: {name}")))
        }
    }
}

pub fn skill_list() -> Vec<Value> {
    skill_list_in(None)
}

pub fn skill_list_in(dir: Option<&str>) -> Vec<Value> {
    let home = std::env::var("HOME").unwrap_or_default();
    let mut roots = vec![
        format!("{home}/.config/opencode/skill"),
        format!("{home}/.config/opencode/skills"),
    ];
    if let Some(d) = dir {
        roots.push(format!("{d}/.opencode/skill"));
        roots.push(format!("{d}/.opencode/skills"));
    }
    let mut out: Vec<Value> = Vec::new();
    for root in roots {
        let Ok(rd) = std::fs::read_dir(&root) else {
            continue;
        };
        for entry in rd.filter_map(|e| e.ok()) {
            let dir_name = entry.file_name().to_string_lossy().to_string();
            let skill_md = entry.path().join("SKILL.md");
            let Ok(content) = std::fs::read_to_string(&skill_md) else {
                continue;
            };
            let mut name = dir_name.clone();
            let mut description = String::new();
            if let Some(rest) = content.strip_prefix("---") {
                if let Some(end) = rest.find("\n---") {
                    for line in rest[..end].lines() {
                        if let Some(v) = line.strip_prefix("name:") {
                            name = v.trim().trim_matches('"').to_string();
                        }
                        if let Some(v) = line.strip_prefix("description:") {
                            description = v.trim().trim_matches('"').to_string();
                        }
                    }
                }
            }
            if out.iter().any(|s| s["name"].as_str() == Some(name.as_str())) {
                continue;
            }
            out.push(json!({"name": name, "description": description, "location": skill_md.to_string_lossy()}));
        }
    }
    out
}
