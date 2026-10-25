use crate::engine::{bus, App, EngineApi};
use crate::types::{hex_id, ModelRef, PromptRequest, ToolError, ToolOutput};
use serde_json::{json, Value};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

mod changes;
mod files;
mod output;
pub mod patch;

pub(crate) use files::media_type;
pub use process::{cancel_background_jobs, stop_background_jobs};
mod process;
mod search;
mod skills;
mod web;

pub struct ToolCtx {
    pub dir: String,
    pub app: App,
    pub session_id: String,
    pub message_id: String,
    pub call_id: String,
    pub part_id: String,
    pub engine: Arc<dyn EngineApi>,
    pub cancel: Arc<AtomicBool>,
    pub hook_context: Value,
}

impl ToolCtx {
    pub fn update_metadata(&self, metadata: Value) -> Result<(), ToolError> {
        let message = self
            .app
            .store
            .message(&self.session_id, &self.message_id)
            .map_err(failure)?
            .ok_or_else(|| failure("Tool message no longer exists"))?;
        let mut part = message
            .parts
            .into_iter()
            .find(|part| part["id"].as_str() == Some(self.part_id.as_str()))
            .ok_or_else(|| failure("Tool part no longer exists"))?;
        part["state"]["metadata"] = metadata;
        self.app
            .store
            .update_part(&self.session_id, &part)
            .map_err(failure)?;
        bus().emit("message.part.updated", json!({"part":part}));
        Ok(())
    }
}

pub fn tool_defs() -> Vec<Value> {
    vec![
        fn_def("bash", "Run a shell command. workdir is relative to the session directory; timeout is milliseconds (default 120000). Commands are cancelled with the turn and their process group is stopped when the command exits, so pass background:true for work that must outlive the call: it returns a pid and log path at once and keeps running across tool calls, ending when its session is aborted or deleted, or when ocmini exits. A timed-out command completes with the output captured before the limit and a shell_metadata timeout notice.", json!({"type":"object","properties":{"command":{"type":"string"},"workdir":{"type":"string"},"timeout":{"type":"integer","minimum":1},"description":{"type":"string"},"background":{"type":"boolean"}},"required":["command"]})),
        fn_def("read", "Read a UTF-8 file with line numbers, an image/PDF attachment, or a directory listing. Relative paths use the session directory; offset starts at 1.", json!({"type":"object","properties":{"filePath":{"type":"string"},"offset":{"type":"integer","minimum":1},"limit":{"type":"integer","minimum":1}},"required":["filePath"]})),
        fn_def("write", "Atomically write a UTF-8 file, creating parent directories and preserving existing file permissions.", json!({"type":"object","properties":{"filePath":{"type":"string"},"content":{"type":"string"}},"required":["filePath","content"]})),
        fn_def("edit", "Replace an exact, nonempty string in a file. Match must be unique unless replaceAll is true. Preserves CRLF files.", json!({"type":"object","properties":{"filePath":{"type":"string"},"oldString":{"type":"string","minLength":1},"newString":{"type":"string"},"replaceAll":{"type":"boolean"}},"required":["filePath","oldString","newString"]})),
        fn_def("apply_patch", "Apply a patch using *** Begin Patch, *** Add File:, *** Update File:, *** Delete File:, optional *** Move to:, @@ context hunks, and *** End Patch. All files are validated before any are changed.", json!({"type":"object","properties":{"patchText":{"type":"string"}},"required":["patchText"]})),
        fn_def("glob", "Find files using ripgrep and glob patterns (including ** and {a,b}), honoring gitignore. Returns up to 100 files. Relative path uses the session directory.", json!({"type":"object","properties":{"pattern":{"type":"string"},"path":{"type":"string"}},"required":["pattern"]})),
        fn_def("grep", "Search UTF-8 files by regex, honoring gitignore. Returns up to 100 matching lines with paths and line numbers; include filters file names or relative paths by glob.", json!({"type":"object","properties":{"pattern":{"type":"string"},"path":{"type":"string"},"include":{"type":"string"}},"required":["pattern"]})),
        fn_def("webfetch", "Fetch an HTTP(S) text resource up to 5 MiB. HTML is converted to readable Markdown-style text or plain text according to format. timeout is seconds (default 30, maximum 120).", json!({"type":"object","properties":{"url":{"type":"string"},"format":{"type":"string","enum":["text","markdown","html"]},"timeout":{"type":"integer","minimum":1,"maximum":120}},"required":["url"]})),
        fn_def("todowrite", "Replace and persist the session's todo list.", json!({"type":"object","properties":{"todos":{"type":"array","items":{"type":"object","properties":{"id":{"type":"string"},"content":{"type":"string"},"status":{"type":"string","enum":["pending","in_progress","completed","cancelled"]},"priority":{"type":"string","enum":["high","medium","low"]}},"required":["content","status","priority"]}}},"required":["todos"]})),
        fn_def("question", "Ask one or more questions, with options, and wait for the user's answers.", json!({"type":"object","properties":{"questions":{"type":"array","minItems":1,"items":{"type":"object","properties":{"question":{"type":"string"},"header":{"type":"string"},"multiple":{"type":"boolean"},"custom":{"type":"boolean"},"options":{"type":"array","items":{"type":"object","properties":{"label":{"type":"string"},"description":{"type":"string"}},"required":["label","description"]}}},"required":["question","header","options"]}}},"required":["questions"]})),
        fn_def("task", "Delegate to a subagent session. subagent_type selects an available subagent; task_id resumes a child from this session. Returns the child's session ID and result.", json!({"type":"object","properties":{"description":{"type":"string"},"prompt":{"type":"string"},"subagent_type":{"type":"string"},"task_id":{"type":"string"}},"required":["description","prompt","subagent_type"]})),
        fn_def("skill", "Load the complete instructions and resource directory of an available skill by name.", json!({"type":"object","properties":{"name":{"type":"string"}},"required":["name"]})),
    ]
}

fn fn_def(name: &str, description: &str, parameters: Value) -> Value {
    json!({"type":"function","function":{"name":name,"description":description,"parameters":parameters}})
}

fn failure(message: impl Into<String>) -> ToolError {
    ToolError {
        message: message.into(),
    }
}

fn required<'a>(input: &'a Value, key: &str) -> Result<&'a str, ToolError> {
    input
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| failure(format!("{key} must be a string")))
}

pub fn resolve_path(directory: &str, path: &str) -> PathBuf {
    let expanded = if path == "~" || path.starts_with("~/") {
        std::env::var("HOME")
            .map(|home| PathBuf::from(home).join(path.strip_prefix("~/").unwrap_or("")))
            .unwrap_or_else(|_| PathBuf::from(path))
    } else {
        PathBuf::from(path)
    };
    let absolute = if expanded.is_absolute() {
        expanded
    } else {
        Path::new(directory).join(expanded)
    };
    let absolute = if absolute.is_absolute() {
        absolute
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(absolute)
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

fn cap_out(value: &str, max: usize) -> String {
    if value.len() <= max {
        return value.into();
    }
    let mut head = max / 2;
    while head > 0 && !value.is_char_boundary(head) {
        head -= 1;
    }
    let mut tail = value.len() - max / 2;
    while tail < value.len() && !value.is_char_boundary(tail) {
        tail += 1;
    }
    format!(
        "{}\n\n[... {} bytes truncated ...]\n{}",
        &value[..head],
        tail - head,
        &value[tail..]
    )
}

async fn cancelled(flag: Arc<AtomicBool>) {
    while !flag.load(Ordering::Acquire) {
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

pub(super) fn hook_context(ctx: &ToolCtx, tool: &str) -> Value {
    let mut context = ctx.hook_context.clone();
    if !context.is_object() {
        context = json!({});
    }
    context["directory"] = json!(ctx.dir);
    context["sessionID"] = json!(ctx.session_id);
    context["messageID"] = json!(ctx.message_id);
    context["callID"] = json!(ctx.call_id);
    context["tool"] = json!(tool);
    context
}

/// Resource transforms run before the engine authorizes and records effective input.
pub async fn prepare_input(
    ctx: &ToolCtx,
    name: &str,
    mut input: Value,
) -> Result<Value, ToolError> {
    let object = input
        .as_object_mut()
        .ok_or_else(|| failure("Tool input must be an object"))?;
    object.remove(changes::KEY);
    object.remove("_ocminiTaskModel");
    if ctx.cancel.load(Ordering::Acquire) {
        return Err(failure("Tool cancelled"));
    }
    match name {
        "write" | "edit" | "apply_patch" | "patch"
            if ctx
                .app
                .hooks
                .matches("file.change.before", &hook_context(ctx, name)) =>
        {
            changes::prepare(ctx, name, input).await
        }
        "task" => prepare_task(ctx, input).await,
        _ => Ok(input),
    }
}

pub fn prepared_file_paths(input: &Value) -> Option<Vec<PathBuf>> {
    changes::paths(input)
}

/// Rebuild derived spill metadata after a successful output transform.
pub async fn limit_output(_directory: &str, out: ToolOutput) -> Result<ToolOutput, ToolError> {
    output::relimit(out, output::directory()).await
}

async fn shell_environment(
    ctx: &ToolCtx,
    input: &Value,
    directory: &Path,
) -> Result<serde_json::Map<String, Value>, ToolError> {
    if !ctx.app.hooks.has("shell.env") {
        return Ok(Default::default());
    }
    let mut context = hook_context(ctx, "bash");
    context["workdir"] = json!(directory);
    let effective = ctx.app.hooks.run("shell.env", &context,
        json!({"env":{},"command":input["command"],"background":input["background"].as_bool().unwrap_or(false)}), Some(&ctx.cancel)).await.map_err(failure)?.input;
    let env = effective["env"]
        .as_object()
        .ok_or_else(|| failure("shell.env env must be an object"))?;
    for (key, value) in env {
        if key.is_empty()
            || key.contains(['=', '\0'])
            || key.starts_with("OCMINI_HOOK_")
            || (!value.is_null() && value.as_str().is_none_or(|value| value.contains('\0')))
        {
            return Err(failure(format!("Invalid shell environment entry {key:?}")));
        }
    }
    Ok(env.clone())
}

pub async fn run_tool(ctx: &ToolCtx, name: &str, input: &Value) -> Result<ToolOutput, ToolError> {
    if !input.is_object() {
        return Err(failure("Tool input must be an object"));
    }
    if ctx.cancel.load(Ordering::Acquire) {
        return Err(failure("Tool cancelled"));
    }
    let result = match name {
        "bash" => {
            let command = required(input, "command")?;
            let directory = resolve_path(
                &ctx.dir,
                input["workdir"]
                    .as_str()
                    .or_else(|| input["cwd"].as_str())
                    .unwrap_or("."),
            );
            let env = shell_environment(ctx, input, &directory).await?;
            if ctx.cancel.load(Ordering::Acquire) {
                return Err(failure("Tool cancelled"));
            }
            if input["background"].as_bool().unwrap_or(false) {
                return process::shell_background_with_env(
                    command,
                    &directory,
                    &ctx.session_id,
                    &output::directory(),
                    &env,
                )
                .await;
            }
            let timeout = match input.get("timeout") {
                None | Some(Value::Null) => 120_000,
                Some(value) => value
                    .as_u64()
                    .filter(|v| *v > 0)
                    .ok_or_else(|| failure("timeout must be a positive integer in milliseconds"))?,
            };
            process::shell_with_env(
                command,
                &directory,
                Duration::from_millis(timeout),
                ctx.cancel.clone(),
                &output::directory(),
                &env,
            )
            .await
        }
        "glob" | "grep" => search::run(&ctx.dir, name, input, ctx.cancel.clone()).await,
        "write" | "edit" | "apply_patch" | "patch" if input.get(changes::KEY).is_some() => {
            let input = input.clone();
            let cancel = ctx.cancel.clone();
            tokio::task::spawn_blocking(move || changes::commit(&input, &cancel))
                .await
                .map_err(|e| failure(e.to_string()))?
        }
        "read" | "write" | "edit" => {
            let directory = ctx.dir.clone();
            let name = name.to_owned();
            let input = input.clone();
            let cancel = ctx.cancel.clone();
            tokio::task::spawn_blocking(move || files::run(&directory, &name, &input, &cancel))
                .await
                .map_err(|e| failure(format!("File operation failed: {e}")))?
        }
        "apply_patch" | "patch" => {
            let text = input["patchText"]
                .as_str()
                .or_else(|| input["patch"].as_str())
                .ok_or_else(|| failure("patchText must be a string"))?
                .to_owned();
            let directory = ctx.dir.clone();
            let cancel = ctx.cancel.clone();
            tokio::task::spawn_blocking(move || patch::apply(&directory, &text, &cancel))
                .await
                .map_err(|e| failure(format!("Patch operation failed: {e}")))?
        }
        "webfetch" => web::fetch(input, ctx.cancel.clone()).await,
        "todowrite" => {
            let todos = input["todos"]
                .as_array()
                .ok_or_else(|| failure("todos must be an array"))?;
            for todo in todos {
                if todo["content"]
                    .as_str()
                    .is_none_or(|text| text.trim().is_empty())
                    || !matches!(
                        todo["status"].as_str(),
                        Some("pending" | "in_progress" | "completed" | "cancelled")
                    )
                    || !matches!(todo["priority"].as_str(), Some("high" | "medium" | "low"))
                {
                    return Err(failure("Each todo needs nonempty content, a valid status, and high/medium/low priority"));
                }
            }
            ctx.app
                .store
                .set_todos(&ctx.session_id, &input["todos"])
                .map_err(failure)?;
            bus().emit(
                "todo.updated",
                json!({"sessionID":ctx.session_id,"todos":todos}),
            );
            Ok(ToolOutput {
                output: serde_json::to_string_pretty(todos).map_err(|e| failure(e.to_string()))?,
                title: format!("{} todos", todos.len()),
                metadata: json!({"todos":todos}),
            })
        }
        "question" => question(ctx, input).await,
        "task" => task(ctx, input).await,
        "skill" => {
            let name = input["name"]
                .as_str()
                .or_else(|| input["id"].as_str())
                .ok_or_else(|| failure("name must be a string"))?;
            let paths = ctx.app.config.lock().unwrap().skill_paths();
            let mut loaded = skills::load_content(&ctx.dir, &paths, name)?;
            let mut context = hook_context(ctx, "skill");
            context["name"] = json!(name);
            context["path"] = loaded.metadata["location"].clone();
            context["sourceFingerprint"] = loaded.metadata["sourceFingerprint"].clone();
            if ctx.app.hooks.has("skill.load.after") {
                let effective = ctx
                    .app
                    .hooks
                    .run(
                        "skill.load.after",
                        &context,
                        json!({"content":loaded.output}),
                        Some(&ctx.cancel),
                    )
                    .await
                    .map_err(failure)?
                    .input;
                loaded.output = effective["content"]
                    .as_str()
                    .ok_or_else(|| failure("Skill content must be a string"))?
                    .into();
            }
            skills::render_content(&mut loaded);
            ctx.app.hooks.emit(
                "ocmini.skill.loaded",
                &context,
                json!({"name":name,"path":loaded.metadata["location"],"sourceFingerprint":loaded.metadata["sourceFingerprint"]}),
            );
            Ok(loaded)
        }
        _ => {
            let (server, tool) = ctx
                .app
                .mcp
                .route(name)
                .ok_or_else(|| failure(format!("Unknown tool {name:?}")))?;
            let result = ctx
                .app
                .mcp
                .call_result_async(&server, &tool, input.clone(), ctx.cancel.clone())
                .await
                .map_err(failure)?;
            let output = crate::mcp::render_result(&result);
            let attachments: Vec<Value> = result["content"]
                .as_array()
                .map(|items| {
                    items
                        .iter()
                        .filter(|item| {
                            matches!(item["type"].as_str(), Some("image" | "audio"))
                                || item["type"] == "resource"
                                    && item["resource"]["blob"].is_string()
                        })
                        .cloned()
                        .collect()
                })
                .unwrap_or_default();
            Ok(ToolOutput {
                output,
                title: format!("{server}/{tool}"),
                metadata: json!({"server":server,"tool":tool,"attachments":attachments}),
            })
        }
    }?;
    if matches!(name, "write" | "edit" | "apply_patch" | "patch") {
        let mut paths = prepared_file_paths(input).unwrap_or_default();
        if paths.is_empty() {
            if let Some(path) = result.metadata["filepath"].as_str() {
                paths.push(resolve_path(&ctx.dir, path));
            }
            if let Some(files) = result.metadata["files"].as_array() {
                for file in files {
                    for key in ["path", "movePath"] {
                        if let Some(path) = file[key].as_str() {
                            paths.push(resolve_path(&ctx.dir, path));
                        }
                    }
                }
            }
        }
        for path in paths {
            ctx.app.hooks.emit(
                "file.edited",
                &hook_context(ctx, name),
                json!({"file":path,"tool":name}),
            );
        }
    }
    output::limit(result, output::directory()).await
}

pub fn cleanup_outputs() -> Result<(), String> {
    let Some(value) = std::env::var_os("OCMINI_TOOL_OUTPUT_RETENTION_DAYS") else {
        return Ok(());
    };
    let days = value
        .to_str()
        .and_then(|value| value.parse::<u64>().ok())
        .ok_or("OCMINI_TOOL_OUTPUT_RETENTION_DAYS must be a nonnegative integer")?;
    if days > 0 {
        output::cleanup(
            &output::directory(),
            Duration::from_secs(days.saturating_mul(86_400)),
        );
    }
    Ok(())
}

struct QuestionGuard {
    state: crate::engine::question::QuestionState,
    id: String,
}
impl Drop for QuestionGuard {
    fn drop(&mut self) {
        self.state.reject(&self.id);
    }
}

fn validate_questions(input: &Value) -> Result<(), ToolError> {
    let questions = input["questions"]
        .as_array()
        .filter(|questions| !questions.is_empty())
        .ok_or_else(|| failure("questions must be a nonempty array"))?;
    for question in questions {
        if ["multiple", "custom"]
            .iter()
            .any(|key| question.get(key).is_some_and(|value| !value.is_boolean()))
            || !question["question"].is_string()
            || !question["header"].is_string()
            || !question["options"].as_array().is_some_and(|options| {
                options
                    .iter()
                    .all(|option| option["label"].is_string() && option["description"].is_string())
            })
        {
            return Err(failure("Every question needs question/header strings and an options array containing label/description strings"));
        }
    }
    Ok(())
}

async fn question(ctx: &ToolCtx, input: &Value) -> Result<ToolOutput, ToolError> {
    validate_questions(input)?;
    let effective;
    let input = if ctx.app.hooks.has("question.ask.before") {
        effective = ctx
            .app
            .hooks
            .run(
                "question.ask.before",
                &hook_context(ctx, "question"),
                input.clone(),
                Some(&ctx.cancel),
            )
            .await
            .map_err(failure)?
            .input;
        validate_questions(&effective)?;
        &effective
    } else {
        input
    };
    if ctx.cancel.load(Ordering::Acquire) {
        return Err(failure("Question cancelled"));
    }
    let questions = input["questions"]
        .as_array()
        .filter(|questions| !questions.is_empty())
        .ok_or_else(|| failure("questions must be a nonempty array"))?;
    let (request, receiver) = ctx.app.questions.ask_for_tool(
        &ctx.session_id,
        Value::Array(questions.clone()),
        Some(json!({"messageID":ctx.message_id,"callID":ctx.call_id})),
    );
    let _guard = QuestionGuard {
        state: ctx.app.questions.clone(),
        id: request.id.clone(),
    };
    bus().emit(
        "question.asked",
        serde_json::to_value(&request).map_err(|e| failure(e.to_string()))?,
    );
    let answers = tokio::select! {
        answers = receiver => answers.map_err(|_| failure("Question cancelled"))?,
        _ = cancelled(ctx.cancel.clone()) => return Err(failure("Question cancelled")),
    }
    .ok_or_else(|| ToolError {
        message: "User dismissed the question".into(),
    })?;
    let text = questions
        .iter()
        .zip(&answers)
        .map(|(question, answer)| {
            format!(
                "{}: {}",
                question["question"].as_str().unwrap_or_default(),
                answer.join(", ")
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    Ok(ToolOutput {
        output: text,
        title: "User answered".into(),
        metadata: json!({"questions":questions,"answers":answers}),
    })
}

struct ChildGuard {
    engine: Arc<dyn EngineApi>,
    session_id: String,
    armed: bool,
}
impl Drop for ChildGuard {
    fn drop(&mut self) {
        if self.armed {
            self.engine.abort(&self.session_id);
        }
    }
}

fn task_options(
    ctx: &ToolCtx,
    input: &Value,
) -> Result<(crate::types::Session, Option<ModelRef>), ToolError> {
    let prompt = required(input, "prompt")?;
    required(input, "description")?;
    let agent = required(input, "subagent_type")?;
    if prompt.trim().is_empty() {
        return Err(failure("prompt must not be empty"));
    }
    {
        let config = ctx.app.config.lock().unwrap();
        if !matches!(agent, "general" | "explore") && !config.agent.contains_key(agent) {
            return Err(failure(format!("Unknown subagent {agent:?}")));
        }
        if let Some(config) = config.agent.get(agent) {
            if config["disable"] == true || config["mode"] == "primary" {
                return Err(failure(format!(
                    "Agent {agent:?} is unavailable as a subagent"
                )));
            }
        }
    }
    let parent = ctx
        .app
        .store
        .get_session(&ctx.session_id)
        .map_err(failure)?
        .ok_or_else(|| failure("Parent session does not exist"))?;
    let mut ancestor = parent.parent_id.clone();
    let max_depth = ctx.app.config.lock().unwrap().subagent_depth.unwrap_or(8);
    let mut depth = 0u64;
    let mut ancestors = std::collections::HashSet::from([parent.id.clone()]);
    while let Some(id) = ancestor {
        if !ancestors.insert(id.clone()) {
            return Err(failure("Subagent session ancestry contains a cycle"));
        }
        depth += 1;
        ancestor = ctx
            .app
            .store
            .get_session(&id)
            .map_err(failure)?
            .ok_or_else(|| failure(format!("Ancestor session {id} does not exist")))?
            .parent_id;
    }
    if depth >= max_depth {
        return Err(failure(format!("Subagent nesting limit ({max_depth}) reached. Increase subagent_depth to allow more levels.")));
    }
    let mut model = ctx.app.store.last_model(&ctx.session_id).map_err(failure)?;
    if let Some(selected) = ctx
        .app
        .config
        .lock()
        .unwrap()
        .agent
        .get(agent)
        .and_then(|value| value["model"].as_str())
    {
        let (provider, selected) = selected
            .split_once('/')
            .ok_or_else(|| failure("Subagent model must use provider/model syntax"))?;
        if provider.is_empty() || selected.is_empty() {
            return Err(failure("Subagent model must use provider/model syntax"));
        }
        model = Some(ModelRef {
            provider_id: provider.into(),
            model_id: selected.into(),
        });
    }
    if let Some(selected) = input.get("_ocminiTaskModel") {
        model = Some(
            serde_json::from_value(selected.clone())
                .map_err(|e| failure(format!("Invalid task model: {e}")))?,
        );
    }
    if let Some(selected) = &model {
        if selected.provider_id.is_empty() || selected.model_id.is_empty() {
            return Err(failure(
                "Task model requires nonempty providerID and modelID",
            ));
        }
        if !crate::engine_impl::model_available(&ctx.app.config.lock().unwrap(), selected) {
            return Err(failure("Subagent provider or model is disabled"));
        }
    }
    if let Some(id) = input.get("task_id") {
        let id = id
            .as_str()
            .ok_or_else(|| failure("task_id must be a string"))?;
        let child = ctx
            .app
            .store
            .get_session(id)
            .map_err(failure)?
            .ok_or_else(|| failure("Task session not found"))?;
        if child.parent_id.as_deref() != Some(ctx.session_id.as_str()) {
            return Err(failure(
                "task_id must identify a child of the current session",
            ));
        }
    }
    Ok((parent, model))
}

async fn prepare_task(ctx: &ToolCtx, mut input: Value) -> Result<Value, ToolError> {
    let (_, model) = task_options(ctx, &input)?;
    if !ctx.app.hooks.has("task.spawn.before") {
        return Ok(input);
    }
    let original_agent = required(&input, "subagent_type")?.to_owned();
    let mut payload = input.clone();
    payload["model"] = json!(model);
    let result = ctx
        .app
        .hooks
        .run(
            "task.spawn.before",
            &hook_context(ctx, "task"),
            payload,
            Some(&ctx.cancel),
        )
        .await
        .map_err(failure)?
        .input;
    for key in ["prompt", "description", "subagent_type"] {
        input[key] = result[key].clone();
    }
    let effective_model = result["model"].clone();
    if !effective_model.is_null()
        && (input["subagent_type"] == original_agent || effective_model != json!(model))
    {
        input["_ocminiTaskModel"] = effective_model;
    }
    task_options(ctx, &input)?;
    Ok(input)
}

async fn task(ctx: &ToolCtx, input: &Value) -> Result<ToolOutput, ToolError> {
    let prompt = required(input, "prompt")?;
    let description = required(input, "description")?;
    let agent = required(input, "subagent_type")?;
    let (parent, model) = task_options(ctx, input)?;
    let child = if let Some(id) = input["task_id"].as_str() {
        let child = ctx
            .app
            .store
            .get_session(id)
            .map_err(failure)?
            .ok_or_else(|| failure(format!("Task session {id:?} does not exist")))?;
        if child.parent_id.as_deref() != Some(ctx.session_id.as_str()) {
            return Err(failure(
                "task_id must identify a child of the current session",
            ));
        }
        child
    } else {
        let mut child = parent.clone();
        child.id = hex_id("ses_");
        child.slug = crate::types::slug();
        child.title = description.chars().take(160).collect();
        child.parent_id = Some(parent.id.clone());
        child.cost = 0.0;
        child.tokens = Default::default();
        child.time = crate::types::Time {
            created: crate::types::now_ms(),
            updated: Some(crate::types::now_ms()),
            ..Default::default()
        };
        child.extra.clear();
        crate::sessions::create(&ctx.app, child)
            .await
            .map_err(failure)?
    };
    {
        let mut grants = ctx.app.always.lock().unwrap();
        if let Some(inherited) = grants.get(&ctx.session_id).cloned() {
            grants.insert(child.id.clone(), inherited);
        }
    }
    ctx.update_metadata(json!({"sessionId":child.id,"model":model,"agent":agent}))?;
    let request = PromptRequest {
        model: model.clone(),
        parts: vec![json!({"type":"text","text":prompt})],
        agent: Some(agent.into()),
        ..Default::default()
    };
    let mut guard = ChildGuard {
        engine: ctx.engine.clone(),
        session_id: child.id.clone(),
        armed: true,
    };
    let mut context = hook_context(ctx, "task");
    context["childSessionID"] = json!(child.id);
    let done = tokio::select! {
        result = ctx.engine.admit(ctx.app.clone(), child.id.clone(), request) => result.map_err(failure)?,
        _ = cancelled(ctx.cancel.clone()) => return Err(failure("Subagent cancelled")),
    };
    ctx.app.hooks.emit("ocmini.task.started", &context, json!({"parentSessionID":ctx.session_id,"childSessionID":child.id,"agent":agent,"model":model}));
    let result = tokio::select! {
        result = done => result.map_err(|_| "Subagent ended without a result".to_owned()),
        _ = cancelled(ctx.cancel.clone()) => Err("Subagent cancelled".into()),
    };
    let child_error = result
        .as_ref()
        .ok()
        .and_then(|value| {
            value
                .get("error")
                .filter(|error| !error.is_null())
                .or_else(|| value["info"].get("error").filter(|error| !error.is_null()))
        })
        .map(|error| format!("Subagent failed: {error}"));
    let status = match &result {
        Ok(_) if child_error.is_none() => "completed",
        Ok(_) => "failed",
        Err(_) if ctx.cancel.load(Ordering::Acquire) => "cancelled",
        Err(_) => "failed",
    };
    ctx.app.hooks.emit("ocmini.task.ended", &context, json!({"parentSessionID":ctx.session_id,"childSessionID":child.id,"status":status,"messageID":result.as_ref().ok().and_then(|value| value["info"]["id"].as_str())}));
    let result = result.map_err(failure)?;
    guard.armed = false;
    if let Some(error) = child_error {
        return Err(failure(error));
    }
    let text = result["parts"]
        .as_array()
        .map(|parts| {
            parts
                .iter()
                .filter(|part| part["type"] == "text")
                .filter_map(|part| part["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default();
    Ok(ToolOutput {
        output: format!("Task session: {}\n\n{text}", child.id),
        title: description.into(),
        metadata: json!({"sessionId":child.id,"model":model,"agent":agent}),
    })
}

pub fn skill_list_with_paths(directory: Option<&str>, paths: &[String]) -> Vec<Value> {
    skills::list(directory, paths)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn truncation_preserves_unicode_boundaries() {
        let text = "🙂".repeat(50_000);
        let output = cap_out(&text, 101);
        assert!(output.contains("truncated"));
        assert!(output.starts_with('🙂'));
        assert!(output.ends_with('🙂'));
    }
    #[test]
    fn paths_resolve_against_session() {
        assert_eq!(
            resolve_path("/tmp/project", "src/../test"),
            PathBuf::from("/tmp/project/test")
        );
        assert_eq!(
            resolve_path("/tmp/project", "/elsewhere/file"),
            PathBuf::from("/elsewhere/file")
        );
    }
    #[test]
    fn all_advertised_tools_have_schemas() {
        let definitions = tool_defs();
        for name in [
            "apply_patch",
            "skill",
            "bash",
            "read",
            "write",
            "edit",
            "glob",
            "grep",
            "webfetch",
            "todowrite",
            "question",
            "task",
        ] {
            assert!(definitions
                .iter()
                .any(|tool| tool["function"]["name"] == name
                    && tool["function"]["parameters"]["type"] == "object"));
        }
    }
}
