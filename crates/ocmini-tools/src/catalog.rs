pub use crate::host::{context as hook_context, ToolCtx};
use crate::{
    alias, changes, decode, failure, files,
    host::Host,
    question, resolve_path, search, skills, task, todo,
    types::{OperationError, ToolOutput},
    web,
};
use serde_json::{json, Value};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::Duration;
#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct ShellInput {
    pub command: String,
    pub workdir: Option<String>,
    #[serde(rename = "description")]
    _description: Option<String>,
    #[schemars(range(min = 1))]
    pub timeout: Option<u64>,
    #[serde(default)]
    pub background: bool,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct DiscoverInput {
    pub query: String,
}

/// The built-in catalog and its closed Rust identity share one spelling table.
macro_rules! builtin_tools {
    ($($variant:ident => $name:literal, $description:literal, $parameters:ty;)+) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub enum Builtin { $($variant),+ }
        impl Builtin {
            pub fn from_name(name: &str) -> Option<Self> {
                match name {
                    $($name => Some(Self::$variant),)+
                    "patch" => Some(Self::ApplyPatch),
                    "shell" => Some(Self::Bash),
                    "subagent" => Some(Self::Task),
                    _ => None,
                }
            }
            pub fn is_execution(self) -> bool {
                matches!(
                    self,
                    Self::Bash
                        | Self::BackgroundRead
                        | Self::BackgroundList
                        | Self::BackgroundStop
                        | Self::Write
                        | Self::Edit
                        | Self::ApplyPatch
                        | Self::Task
                )
            }
        }
        pub fn tool_defs() -> Vec<Value> {
            static DEFINITIONS: std::sync::OnceLock<Vec<Value>> = std::sync::OnceLock::new();
            DEFINITIONS
                .get_or_init(|| vec![$(definition::<$parameters>($name, $description)),+])
                .iter()
                .filter(|def| {
                    cfg!(unix)
                        || !matches!(
                            def["function"]["name"].as_str(),
                            Some("background_read" | "background_list" | "background_stop")
                        )
                })
                .cloned()
                .collect()
        }
    };
}
builtin_tools! {
    DiscoverMcpTools => "discover_mcp_tools", "Find and activate available MCP tools for later steps by name or description.", DiscoverInput;
    Bash => "bash", "Run a shell command. workdir is relative to the session directory; timeout is milliseconds (default 120000). Commands are cancelled with the turn and their process group is stopped when the command exits, so pass background:true for work that must outlive the call: it returns a pid and log path at once and keeps running across tool calls, ending when its session is aborted or deleted, or when ocmini exits. A timed-out command completes with the output captured before the limit and a shell_metadata timeout notice.", ShellInput;
    BackgroundRead => "background_read", "Read incremental output from a background shell job owned by this session. Use job_id from the launch and nextOffset as the next byte offset; status and observed exit code are returned in metadata.", crate::shell::BackgroundReadInput;
    BackgroundList => "background_list", "List background shell jobs owned by this session, including pid, job_id, command/description, status, exit code, log path, and elapsed time.", crate::shell::BackgroundListInput;
    BackgroundStop => "background_stop", "Stop a running background shell job owned by this session by pid, terminating its entire process group.", crate::shell::BackgroundStopInput;
    Read => "read", "Read a UTF-8 file with line numbers, an image/PDF attachment, or a directory listing. Accepts filePath (or path alias). Relative paths use the session directory; offset starts at 1.", files::ReadInput;
    Write => "write", "Atomically write a UTF-8 file, creating parent directories and preserving existing file permissions.", changes::WriteInput;
    Edit => "edit", "Replace an exact, nonempty string in a file. Match must be unique unless replaceAll is true. Preserves CRLF files.", changes::EditInput;
    ApplyPatch => "apply_patch", "Apply a patch using *** Begin Patch, *** Add File:, *** Update File:, *** Delete File:, optional *** Move to:, @@ context hunks, and *** End Patch. All files are validated before any are changed.", changes::PatchInput;
    Glob => "glob", "Find files using ripgrep and glob patterns (including ** and {a,b}), including hidden files while honoring gitignore and skipping .git. Returns up to 100 files in deterministic sorted order. Relative path uses the session directory.", search::Input;
    Grep => "grep", "Search UTF-8 files by regex, honoring gitignore. Returns up to 100 matching lines in deterministic path order; include and exclude filter by glob, offset paginates results, multiline enables multi-line matching, context adds surrounding lines, filesOnly returns paths, literal disables regex, and caseSensitive controls case matching.", search::GrepInput;
    WebFetch => "webfetch", "Fetch an HTTP(S) text resource up to 5 MiB. HTML is converted to readable Markdown-style text or plain text according to format. timeout is seconds (default 30, maximum 120).", web::Input;
    TodoRead => "todoread", "Read the current session's persisted todo list.", todo::ReadInput;
    TodoWrite => "todowrite", "Replace and persist the session's todo list.", todo::Input;
    Question => "question", "Ask one or more questions, with options, and wait for the user's answers.", question::Input;
    Task => "task", "Delegate to a subagent session. subagent_type selects an available subagent; task_id resumes a child from this session. Returns the child's session ID and result.", task::Input;
    Skill => "skill", "Load the complete instructions and resource directory of an available skill by name.", skills::Input;
}

fn definition<T: schemars::JsonSchema>(name: &str, description: &str) -> Value {
    let mut parameters = schemars::generate::SchemaSettings::draft07()
        .with(|settings| {
            settings.inline_subschemas = true;
            settings.meta_schema = None;
        })
        .into_generator()
        .into_root_schema_for::<T>()
        .to_value();
    parameters.as_object_mut().unwrap().remove("title");
    json!({"type":"function","function":{"name":name,"description":description,"parameters":parameters}})
}

struct FileLease(Box<dyn Fn() + Send + Sync>);
impl Drop for FileLease {
    fn drop(&mut self) {
        (self.0)();
    }
}
pub struct PreparedTool {
    args: Value,
    tool: Tool,
}
enum Tool {
    Effect,
    File {
        handle: u64,
        paths: Vec<PathBuf>,
        _lease: FileLease,
    },
    Task(Box<task::Plan>),
    TodoRead,
    Todo(todo::Input),
    Question(question::Input),
    Skill(skills::Input),
    Mcp {
        server: String,
        tool: String,
    },
}
impl PreparedTool {
    pub fn args(&self) -> &Value {
        &self.args
    }
    pub fn file_paths(&self) -> Option<impl Iterator<Item = &Path>> {
        match &self.tool {
            Tool::File { paths, .. } => Some(paths.iter().map(PathBuf::as_path)),
            _ => None,
        }
    }
    pub fn paths(&self, directory: &str) -> Vec<PathBuf> {
        if let Some(paths) = self.file_paths() {
            return paths.map(Path::to_owned).collect();
        }
        let path = self.args["filePath"]
            .as_str()
            .or(self.args["path"].as_str())
            .or(self.args["workdir"].as_str())
            .or(self.args["cwd"].as_str());
        if let Some(path) = path {
            vec![resolve_path(directory, path)]
        } else {
            vec![]
        }
    }
    pub fn permission<'a>(&self, name: &'a str, paths: &[PathBuf]) -> (&'a str, Vec<String>) {
        match &self.tool {
            Tool::File { .. } => (
                "edit",
                paths
                    .iter()
                    .map(|p| p.to_string_lossy().into_owned())
                    .collect(),
            ),
            Tool::Task(plan) => ("task", vec![plan.input.subagent_type.clone()]),
            _ => match Builtin::from_name(name) {
                Some(Builtin::Bash) => (
                    "bash",
                    vec![self.args["command"].as_str().unwrap_or_default().into()],
                ),
                Some(Builtin::BackgroundRead) => (
                    "background_read",
                    vec![self.args["job_id"].as_str().unwrap_or_default().into()],
                ),
                Some(Builtin::BackgroundList) => ("background_list", vec!["*".into()]),
                Some(Builtin::BackgroundStop) => (
                    "background_stop",
                    vec![self.args["pid"]
                        .as_u64()
                        .map(|pid| pid.to_string())
                        .unwrap_or_default()],
                ),
                Some(Builtin::Read) => (
                    "read",
                    paths
                        .iter()
                        .map(|p| p.to_string_lossy().into_owned())
                        .collect(),
                ),
                Some(Builtin::Grep | Builtin::Glob) => (
                    name,
                    vec![self.args["pattern"].as_str().unwrap_or_default().into()],
                ),
                Some(Builtin::WebFetch) => (
                    name,
                    vec![self.args["url"]
                        .as_str()
                        .unwrap_or_default()
                        .split('#')
                        .next()
                        .unwrap_or_default()
                        .into()],
                ),
                Some(Builtin::Skill) => (
                    name,
                    vec![self.args["name"]
                        .as_str()
                        .or(self.args["id"].as_str())
                        .unwrap_or_default()
                        .into()],
                ),
                _ => (name, vec!["*".into()]),
            },
        }
    }
}

pub async fn prepare_input<A: Host, F>(
    ctx: &ToolCtx<A>,
    name: &str,
    input: Value,
    mut authorize: impl FnMut(PathBuf, PathBuf) -> F + Send,
) -> Result<PreparedTool, OperationError>
where
    F: Future<Output = Result<(), OperationError>> + Send,
{
    if !input.is_object() {
        return Err(failure("Tool input must be an object"));
    }
    if ctx.cancel.is_cancelled() {
        return Err(failure("Tool cancelled"));
    }
    let mut args = input;
    let tool = match Builtin::from_name(name) {
        Some(Builtin::Write | Builtin::Edit | Builtin::ApplyPatch) => {
            let paths = ctx
                .app
                .effect(ctx, "paths", json!({"name":name,"args":args}))
                .await?
                .json()?;
            let handle = paths["handle"].as_u64().ok_or("Missing file reservation")?;
            let app = ctx.app.clone();
            let session = ctx.session_id.clone();
            let lease = FileLease(Box::new(move || app.release_file(&session, handle)));
            for pair in paths["targets"].as_array().ok_or("Invalid tool paths")? {
                authorize(
                    PathBuf::from(pair[0].as_str().ok_or("Invalid path")?),
                    PathBuf::from(pair[1].as_str().ok_or("Invalid path")?),
                )
                .await?;
            }
            let mut draft = ctx
                .app
                .effect(
                    ctx,
                    "prepare",
                    json!({"name":name,"handle":handle,"includeChanges":ctx.app.matches_hook(crate::hooks::FILE_CHANGE_BEFORE,&hook_context(ctx,name))}),
                )
                .await?.json()?;
            if ctx
                .app
                .matches_hook(crate::hooks::FILE_CHANGE_BEFORE, &hook_context(ctx, name))
            {
                let baseline = draft["changes"].clone();
                let transformed = ctx
                    .app
                    .hook(
                        crate::hooks::FILE_CHANGE_BEFORE,
                        &hook_context(ctx, name),
                        json!({"changes":baseline}),
                        &ctx.cancel,
                        Some(Box::new(|_, set| {
                            if let Some(changes) = set.get("changes") {
                                let list = changes
                                    .as_array()
                                    .filter(|a| !a.is_empty())
                                    .ok_or("File change batch must not be empty")?;
                                for c in list {
                                    if !c["path"].is_string()
                                        || c["before"].as_str().is_some_and(|s| s.contains('\0'))
                                        || c["after"].as_str().is_some_and(|s| s.contains('\0'))
                                    {
                                        return Err("Invalid file changes".into());
                                    }
                                }
                            }
                            Ok(())
                        })),
                    )
                    .await;
                let transformed = match transformed {
                    Ok(t) => t,
                    Err(e) => {
                        let _ = ctx
                            .app
                            .effect(ctx, "discard", json!({"handle":handle}))
                            .await;
                        return Err(e);
                    }
                };
                if transformed["changes"] != baseline {
                    let normalized = ctx
                        .app
                        .effect(
                            ctx,
                            "transform_paths",
                            json!({"name":name,"handle":handle,"changes":transformed["changes"]}),
                        )
                        .await?
                        .json()?;
                    for pair in normalized["targets"].as_array().ok_or("Invalid paths")? {
                        authorize(
                            PathBuf::from(pair[0].as_str().ok_or("Invalid path")?),
                            PathBuf::from(pair[1].as_str().ok_or("Invalid path")?),
                        )
                        .await?;
                    }
                    draft = ctx
                        .app
                        .effect(
                            ctx,
                            "transform",
                            json!({"name":name,"handle":handle,"changes":normalized["changes"]}),
                        )
                        .await?
                        .json()?;
                    if matches!(
                        Builtin::from_name(name),
                        Some(Builtin::Write | Builtin::Edit)
                    ) {
                        let c = &draft["changes"][0];
                        args["filePath"] = c["path"].clone();
                        if name == "write" {
                            args["content"] = c["after"].clone();
                        } else {
                            args["oldString"] = c["before"].clone();
                            args["newString"] = c["after"].clone();
                            args["replaceAll"] = false.into();
                        }
                    }
                }
            }
            Tool::File {
                handle,
                _lease: lease,
                paths: serde_json::from_value(draft["paths"].clone())
                    .map_err(|e| failure(e.to_string()))?,
            }
        }
        Some(Builtin::Task) => {
            let (effective, plan) = task::prepare(ctx, args).await?;
            args = effective;
            Tool::Task(Box::new(plan))
        }
        Some(Builtin::TodoRead) => {
            let _: todo::ReadInput = decode(&args)?;
            Tool::TodoRead
        }
        Some(Builtin::TodoWrite) => Tool::Todo(todo::parse(&args)?),
        Some(Builtin::Question) => Tool::Question(question::parse(&args)?),
        Some(Builtin::Skill) => Tool::Skill(decode(&alias(&args, "name", "id"))?),
        Some(kind) => {
            match kind {
                Builtin::DiscoverMcpTools => {
                    let _: DiscoverInput = decode(&args)?;
                }
                Builtin::Bash => {
                    strip_caller_env(&mut args);
                    let input: ShellInput = decode(&alias(&args, "workdir", "cwd"))?;
                    if input.timeout == Some(0) {
                        return Err(failure(
                            "timeout must be a positive integer in milliseconds",
                        ));
                    }
                }
                Builtin::BackgroundRead => {
                    let _: crate::shell::BackgroundReadInput = decode(&args)?;
                }
                Builtin::BackgroundList => {
                    let _: crate::shell::BackgroundListInput = decode(&args)?;
                }
                Builtin::BackgroundStop => {
                    let input: crate::shell::BackgroundStopInput = decode(&args)?;
                    if input.pid == 0 {
                        return Err(failure("pid must be a positive integer"));
                    }
                }
                Builtin::Read => {
                    let input: files::ReadInput = decode(&alias(&args, "filePath", "path"))?;
                    files::validate_read(&input)?;
                }
                Builtin::Glob => {
                    let _: search::Input = decode(&args)?;
                }
                Builtin::Grep => {
                    let input: search::GrepInput = decode(&args)?;
                    if input.search.pattern.is_empty() {
                        return Err(failure("pattern is required"));
                    }
                }
                Builtin::WebFetch => {
                    let input: web::Input = decode(&args)?;
                    web::validate(&input)?;
                }
                _ => {}
            }
            Tool::Effect
        }
        None => {
            let (server, tool) = ctx
                .app
                .mcp_route(name)
                .ok_or_else(|| failure(format!("Unknown tool {name:?}")))?;
            Tool::Mcp { server, tool }
        }
    };
    Ok(PreparedTool { args, tool })
}
pub async fn run_tool<A: Host, F>(
    ctx: &ToolCtx<A>,
    name: &str,
    input: &PreparedTool,
    mut authorize_redirect: impl FnMut(String, Value) -> F + Send,
) -> Result<ToolOutput, OperationError>
where
    F: Future<Output = Result<(), OperationError>> + Send,
{
    if ctx.cancel.is_cancelled() {
        return Err(failure("Tool cancelled"));
    }
    let result = match &input.tool {
        Tool::Effect if Builtin::from_name(name) == Some(Builtin::WebFetch) => {
            let mut hop_args = input.args.clone();
            let timeout = web::request_timeout_from_args(&hop_args)?;
            let deadline = tokio::time::Instant::now() + timeout;
            let mut final_output = None;
            for step in 0..=web::MAX_REDIRECTS {
                ctx.app.ownership(&ctx.session_id)?;
                let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                if remaining.is_zero() {
                    return Err(failure("Fetch timed out"));
                }
                hop_args["timeoutMs"] = json!(remaining.as_millis().max(1) as u64);
                let value = ctx
                    .app
                    .effect(ctx, "webfetch_hop", hop_args.clone())
                    .await?
                    .json()?;
                if let Some(done) = value.get("done") {
                    final_output = Some(
                        serde_json::from_value::<ToolOutput>(done.clone())
                            .map_err(|error| failure(error.to_string()))?,
                    );
                    break;
                }
                let next = value["redirect"]
                    .as_str()
                    .ok_or_else(|| failure("Invalid webfetch redirect response"))?;
                if step == web::MAX_REDIRECTS {
                    return Err(failure(format!(
                        "Too many redirects (more than {})",
                        web::MAX_REDIRECTS
                    )));
                }
                hop_args["url"] = json!(next);
                authorize_redirect(next.to_owned(), hop_args.clone()).await?;
            }
            final_output.ok_or_else(|| {
                failure(format!(
                    "Too many redirects (more than {})",
                    web::MAX_REDIRECTS
                ))
            })
        }
        Tool::Effect => {
            let mut args = input.args.clone();
            if Builtin::from_name(name) == Some(Builtin::Bash) {
                strip_caller_env(&mut args);
                if ctx.app.has_hook(crate::hooks::SHELL_ENV) {
                    let mut context = hook_context(ctx, name);
                    context["workdir"] = json!(resolve_path(
                        &ctx.dir,
                        args["workdir"]
                            .as_str()
                            .or(args["cwd"].as_str())
                            .unwrap_or(".")
                    ));
                    let env=ctx.app.hook(crate::hooks::SHELL_ENV,&context,json!({"env":{},"command":args["command"],"background":args["background"]==true}),&ctx.cancel,None).await?;
                    args["env"] = env["env"].clone();
                }
            }
            ctx.app.ownership(&ctx.session_id)?;
            let value = ctx
                .app
                .effect(ctx, "run", json!({"name":name,"args":args}))
                .await?;
            value.output()
        }
        Tool::File { handle, .. } => {
            ctx.app.ownership(&ctx.session_id)?;
            let value = ctx
                .app
                .effect(ctx, "commit", json!({"handle":handle}))
                .await?;
            value.output()
        }
        Tool::Task(plan) => task::run(ctx, plan).await,
        Tool::TodoRead => {
            let todos = ctx.app.todos(&ctx.session_id)?;
            let count = todos.as_array().map_or(0, Vec::len);
            Ok(ToolOutput {
                output: serde_json::to_string_pretty(&todos).map_err(|e| failure(e.to_string()))?,
                title: format!("{count} todos"),
                metadata: json!({"todos":todos}),
            })
        }
        Tool::Todo(input) => {
            let todos = json!(input.todos);
            ctx.app.set_todos(&ctx.session_id, &todos)?;
            ctx.app.emit(
                "todo.updated",
                &ctx.app.session_context(&ctx.session_id),
                json!({"sessionID":ctx.session_id,"todos":todos}),
            );
            Ok(ToolOutput {
                output: serde_json::to_string_pretty(&todos).map_err(|e| failure(e.to_string()))?,
                title: format!("{} todos", input.todos.len()),
                metadata: json!({"todos":todos}),
            })
        }
        Tool::Question(question) => {
            let mut typed = None;
            let mut wire = input.args.clone();
            if ctx.app.has_hook(crate::hooks::QUESTION_ASK_BEFORE) {
                wire = ctx
                    .app
                    .hook(
                        crate::hooks::QUESTION_ASK_BEFORE,
                        &hook_context(ctx, name),
                        wire,
                        &ctx.cancel,
                        None,
                    )
                    .await?;
                typed = Some(question::parse(&wire)?);
            }
            let answers = ctx.app.ask(ctx, wire["questions"].clone()).await?;
            Ok(ToolOutput {
                output: question::render(typed.as_ref().unwrap_or(question), &answers),
                title: "User answered".into(),
                metadata: json!({"questions":wire["questions"],"answers":answers}),
            })
        }
        Tool::Skill(input) => {
            let mut loaded = if ctx.app.remote(&ctx.session_id) {
                ctx.app
                    .effect(
                        ctx,
                        "skill",
                        json!({"name":input.name,"paths":ctx.app.skill_paths()}),
                    )
                    .await?
                    .output()?
            } else {
                let dir = ctx.dir.clone();
                let paths = ctx.app.skill_paths();
                let skill_name = input.name.clone();
                tokio::task::spawn_blocking(move || skills::load_content(&dir, &paths, &skill_name))
                    .await
                    .map_err(|e| failure(e.to_string()))??
            };
            let mut context = hook_context(ctx, name);
            context["name"] = json!(input.name);
            context["path"] = loaded.metadata["location"].clone();
            context["sourceFingerprint"] = loaded.metadata["sourceFingerprint"].clone();
            if ctx.app.has_hook(crate::hooks::SKILL_LOAD_AFTER) {
                let effective = ctx
                    .app
                    .hook(
                        crate::hooks::SKILL_LOAD_AFTER,
                        &context,
                        json!({"content":loaded.output}),
                        &ctx.cancel,
                        None,
                    )
                    .await?;
                loaded.output = effective["content"]
                    .as_str()
                    .ok_or("Skill content must be a string")?
                    .into();
            }
            skills::render_content(&mut loaded);
            ctx.app.emit("skill.loaded",&context,json!({"name":input.name,"path":loaded.metadata["location"],"sourceFingerprint":loaded.metadata["sourceFingerprint"]}));
            Ok(loaded)
        }
        Tool::Mcp { server, tool } => {
            ctx.app
                .mcp_call(server, tool, input.args.clone(), ctx.cancel.clone())
                .await
        }
    }?;
    if let Some(paths) = input.file_paths() {
        for path in paths {
            ctx.app.emit(
                "file.edited",
                &hook_context(ctx, name),
                json!({"file":path,"tool":name}),
            );
        }
    }
    crate::output::limit(result, ctx.app.output_directory(&ctx.session_id)).await
}
pub async fn limit_output(
    out: ToolOutput,
    directory: PathBuf,
) -> Result<ToolOutput, OperationError> {
    crate::output::relimit(out, directory).await
}
pub fn cleanup_outputs() -> Result<(), String> {
    let Some(value) = std::env::var_os("OCMINI_TOOL_OUTPUT_RETENTION_DAYS") else {
        return Ok(());
    };
    let days = value
        .to_str()
        .and_then(|v| v.parse::<u64>().ok())
        .ok_or("OCMINI_TOOL_OUTPUT_RETENTION_DAYS must be a nonnegative integer")?;
    if days > 0 {
        crate::output::cleanup(
            &crate::output::directory(),
            Duration::from_secs(days.saturating_mul(86_400)),
        );
    }
    Ok(())
}

fn strip_caller_env(args: &mut Value) {
    if let Some(object) = args.as_object_mut() {
        object.remove("env");
    }
}

#[cfg(test)]
mod permission_tests {
    use super::*;

    #[test]
    fn webfetch_permission_ignores_fragment_but_keeps_query() {
        let tool = PreparedTool {
            args: json!({"url": "https://example.com/page?q=1#section"}),
            tool: Tool::Effect,
        };
        assert_eq!(
            tool.permission("webfetch", &[]),
            ("webfetch", vec!["https://example.com/page?q=1".into()])
        );
    }

    #[test]
    fn bash_strip_caller_env_removes_injected_variables() {
        let mut args = json!({
            "command": "echo hi",
            "env": {
                "LD_PRELOAD": "/tmp/evil.so",
                "BASH_ENV": "/tmp/evil.sh"
            }
        });
        strip_caller_env(&mut args);
        assert!(
            args.get("env").is_none(),
            "caller-supplied env must be stripped before shell execution"
        );
        assert_eq!(args["command"], "echo hi");
    }
}
