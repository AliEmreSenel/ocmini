use crate::events as event;
use tokio_util::sync::CancellationToken;
pub(crate) mod hooks;
use crate::engine::{App, EngineApi};
use crate::types::{OperationError, ToolOutput};
pub(crate) use changes::target as canonical_target;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

mod changes;
mod files;
mod output;
pub mod patch;

pub(crate) use files::media_type;
pub use process::{cancel_background_jobs, stop_background_jobs};
mod process;
mod question;
mod search;
mod skills;
mod task;
mod todo;
mod web;

#[derive(Clone)]
pub struct ToolCtx {
    pub dir: String,
    pub app: App,
    pub session_id: String,
    pub message_id: String,
    pub call_id: String,
    pub part_id: String,
    pub engine: Arc<dyn EngineApi>,
    pub cancel: CancellationToken,
    pub hook_context: Value,
}

impl ToolCtx {
    pub fn update_metadata(&self, metadata: Value) -> Result<(), OperationError> {
        let message = self
            .app
            .store
            .message(&self.session_id, &self.message_id)?
            .ok_or_else(|| failure("Tool message no longer exists"))?;
        let mut part = message
            .parts
            .into_iter()
            .find(|part| part["id"].as_str() == Some(self.part_id.as_str()))
            .ok_or_else(|| failure("Tool part no longer exists"))?;
        part["state"]["metadata"] = metadata;
        crate::messages::update_part(&self.app, &part)?;
        Ok(())
    }
}

/// The built-in catalog and its closed Rust identity share one spelling table.
macro_rules! builtin_tools {
    ($($variant:ident => $name:literal, $description:literal, $parameters:ty;)+) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub(crate) enum Builtin { $($variant),+ }
        impl Builtin {
            pub fn from_name(name: &str) -> Option<Self> {
                match name {
                    $($name => Some(Self::$variant),)+
                    "patch" => Some(Self::ApplyPatch),
                    _ => None,
                }
            }
            pub fn is_execution(self) -> bool {
                matches!(self, Self::Bash | Self::Write | Self::Edit | Self::ApplyPatch | Self::Task)
            }
        }
        pub fn tool_defs() -> Vec<Value> {
            static DEFINITIONS: std::sync::OnceLock<Vec<Value>> = std::sync::OnceLock::new();
            DEFINITIONS.get_or_init(|| vec![$(definition::<$parameters>($name, $description)),+]).clone()
        }
    };
}
builtin_tools! {
    Bash => "bash", "Run a shell command. workdir is relative to the session directory; timeout is milliseconds (default 120000). Commands are cancelled with the turn and their process group is stopped when the command exits, so pass background:true for work that must outlive the call: it returns a pid and log path at once and keeps running across tool calls, ending when its session is aborted or deleted, or when ocmini exits. A timed-out command completes with the output captured before the limit and a shell_metadata timeout notice.", process::Input;
    Read => "read", "Read a UTF-8 file with line numbers, an image/PDF attachment, or a directory listing. Relative paths use the session directory; offset starts at 1.", files::ReadInput;
    Write => "write", "Atomically write a UTF-8 file, creating parent directories and preserving existing file permissions.", changes::WriteInput;
    Edit => "edit", "Replace an exact, nonempty string in a file. Match must be unique unless replaceAll is true. Preserves CRLF files.", changes::EditInput;
    ApplyPatch => "apply_patch", "Apply a patch using *** Begin Patch, *** Add File:, *** Update File:, *** Delete File:, optional *** Move to:, @@ context hunks, and *** End Patch. All files are validated before any are changed.", changes::PatchInput;
    Glob => "glob", "Find files using ripgrep and glob patterns (including ** and {a,b}), honoring gitignore. Returns up to 100 files. Relative path uses the session directory.", search::Input;
    Grep => "grep", "Search UTF-8 files by regex, honoring gitignore. Returns up to 100 matching lines with paths and line numbers; include filters file names or relative paths by glob.", search::GrepInput;
    WebFetch => "webfetch", "Fetch an HTTP(S) text resource up to 5 MiB. HTML is converted to readable Markdown-style text or plain text according to format. timeout is seconds (default 30, maximum 120).", web::Input;
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

fn failure(message: impl Into<String>) -> OperationError {
    OperationError::new(crate::types::FailureKind::Error, message)
}

/// Decode once at a tool or hook boundary; execution consumes ordinary Rust fields.
fn decode<T: serde::de::DeserializeOwned>(input: &Value) -> Result<T, OperationError> {
    T::deserialize(input).map_err(|e| failure(format!("Invalid tool input: {e}")))
}

/// Compatibility aliases use the canonical string when both spellings are supplied.
fn alias<'a>(input: &'a Value, key: &str, fallback: &str) -> std::borrow::Cow<'a, Value> {
    if !input[key].is_string() && input[fallback].is_string() {
        let mut input = input.clone();
        input[key] = input[fallback].clone();
        std::borrow::Cow::Owned(input)
    } else {
        std::borrow::Cow::Borrowed(input)
    }
}

pub fn resolve_path(directory: &str, path: &str) -> PathBuf {
    let absolute = crate::config::absolute_path(Path::new(path), Path::new(directory));
    let absolute = if absolute.is_absolute() {
        absolute
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(absolute)
    };
    crate::config::normalize_path(&absolute)
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

pub(crate) fn hook_context(ctx: &ToolCtx, tool: &str) -> Value {
    let mut context = ctx.hook_context.clone();
    if !context.is_object() {
        context = json!({});
    }
    context["directory"] = json!(ctx.dir);
    context["sessionID"] = json!(ctx.session_id);
    context["messageID"] = json!(ctx.message_id);
    context["callID"] = json!(ctx.call_id);
    context["tool"] = json!(tool);
    context["requestKind"] = json!("primary");
    context
}

/// Public arguments and prepared effects travel separately, without private JSON fields.
pub struct PreparedTool {
    args: Value,
    tool: Tool,
}

enum Tool {
    Bash(process::Input),
    Read(files::ReadInput),
    File(Arc<changes::Plan>),
    Task(Box<task::Plan>),
    Glob(search::Input),
    Grep(search::GrepInput),
    WebFetch(web::Input),
    TodoWrite(todo::Input),
    Question(question::Input),
    Skill(skills::Input),
    Mcp { server: String, tool: String },
}
impl PreparedTool {
    pub fn args(&self) -> &Value {
        &self.args
    }
    pub fn file_paths(&self) -> Option<impl Iterator<Item = &Path>> {
        match &self.tool {
            Tool::File(plan) => Some(plan.paths()),
            _ => None,
        }
    }
    pub fn paths(&self, directory: &str) -> Vec<PathBuf> {
        let path = match &self.tool {
            Tool::File(plan) => return plan.paths().map(Path::to_path_buf).collect(),
            Tool::Read(input) => Some(input.file_path.as_str()),
            Tool::Glob(input) => input.path.as_deref(),
            Tool::Grep(input) => input.search.path.as_deref(),
            Tool::Bash(input) => input.workdir.as_deref(),
            Tool::Task(_)
            | Tool::WebFetch(_)
            | Tool::TodoWrite(_)
            | Tool::Question(_)
            | Tool::Skill(_)
            | Tool::Mcp { .. } => return vec![],
        };
        vec![resolve_path(directory, path.unwrap_or("."))]
    }
    pub fn permission<'a>(&self, name: &'a str, paths: &[PathBuf]) -> (&'a str, Vec<String>) {
        let permission = if matches!(self.tool, Tool::File(_)) {
            "edit"
        } else {
            name
        };
        let patterns = match &self.tool {
            Tool::File(_) | Tool::Read(_) => paths
                .iter()
                .map(|p| p.to_string_lossy().into_owned())
                .collect(),
            Tool::Bash(input) => vec![input.command.clone()],
            Tool::Glob(input) => vec![input.pattern.clone()],
            Tool::Grep(input) => vec![input.search.pattern.clone()],
            Tool::WebFetch(input) => vec![input.url.clone()],
            Tool::Task(plan) => vec![plan.input.subagent_type.clone()],
            Tool::Skill(input) => vec![input.name.clone()],
            Tool::TodoWrite(_) | Tool::Question(_) | Tool::Mcp { .. } => vec!["*".into()],
        };
        (permission, patterns)
    }
}

pub async fn prepare_input(
    ctx: &ToolCtx,
    name: &str,
    input: Value,
) -> Result<PreparedTool, OperationError> {
    if !input.is_object() {
        return Err(failure("Tool input must be an object"));
    }
    if ctx.cancel.is_cancelled() {
        return Err(failure("Tool cancelled"));
    }
    let mut args = input;
    let tool = match Builtin::from_name(name) {
        Some(Builtin::Write | Builtin::Edit | Builtin::ApplyPatch) => {
            let (effective, plan) = changes::prepare(ctx, name, args).await?;
            args = effective;
            Tool::File(Arc::new(plan))
        }
        Some(Builtin::Task) => {
            let (effective, plan) = task::prepare(ctx, args).await?;
            args = effective;
            Tool::Task(Box::new(plan))
        }
        Some(Builtin::Bash) => Tool::Bash(decode(&alias(&args, "workdir", "cwd"))?),
        Some(Builtin::Read) => Tool::Read(decode(&args)?),
        Some(Builtin::Glob) => Tool::Glob(decode(&args)?),
        Some(Builtin::Grep) => Tool::Grep(decode(&args)?),
        Some(Builtin::WebFetch) => Tool::WebFetch(decode(&args)?),
        Some(Builtin::TodoWrite) => Tool::TodoWrite(todo::parse(&args)?),
        Some(Builtin::Question) => Tool::Question(question::parse(&args)?),
        Some(Builtin::Skill) => Tool::Skill(decode(&alias(&args, "name", "id"))?),
        None => {
            let (server, tool) = ctx
                .app
                .mcp()
                .route(name)
                .ok_or_else(|| failure(format!("Unknown tool {name:?}")))?;
            Tool::Mcp { server, tool }
        }
    };
    Ok(PreparedTool { args, tool })
}

/// Rebuild derived spill metadata after a successful output transform.
pub async fn limit_output(out: ToolOutput) -> Result<ToolOutput, OperationError> {
    output::relimit(out, output::directory()).await
}

pub async fn run_tool(
    ctx: &ToolCtx,
    name: &str,
    prepared: &PreparedTool,
) -> Result<ToolOutput, OperationError> {
    if ctx.cancel.is_cancelled() {
        return Err(failure("Tool cancelled"));
    }
    let result = match &prepared.tool {
        Tool::File(plan) => {
            let (plan, cancel) = (plan.clone(), ctx.cancel.clone());
            tokio::task::spawn_blocking(move || plan.commit(&cancel))
                .await
                .map_err(|e| failure(e.to_string()))?
        }
        Tool::Task(plan) => task::run(ctx, plan).await,
        Tool::Bash(input) => process::run(ctx, input).await,
        Tool::Glob(input) => {
            search::run(&ctx.dir, search::Mode::Glob, input, ctx.cancel.clone()).await
        }
        Tool::Grep(input) => {
            search::run(
                &ctx.dir,
                search::Mode::Grep(input.include.as_deref()),
                &input.search,
                ctx.cancel.clone(),
            )
            .await
        }
        Tool::Read(input) => {
            let (directory, input, cancel) = (ctx.dir.clone(), input.clone(), ctx.cancel.clone());
            tokio::task::spawn_blocking(move || files::read(&directory, &input, &cancel))
                .await
                .map_err(|e| failure(e.to_string()))?
        }
        Tool::WebFetch(input) => web::fetch(input, ctx.cancel.clone()).await,
        Tool::TodoWrite(input) => todo::run(ctx, input),
        Tool::Question(input) => question::run(ctx, input, &prepared.args).await,
        Tool::Skill(input) => skills::run(ctx, input).await,
        Tool::Mcp { server, tool } => {
            let result = ctx
                .app
                .mcp()
                .call_result_async(server, tool, prepared.args.clone(), ctx.cancel.clone())
                .await?;
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
    if let Some(paths) = prepared.file_paths() {
        let context = hook_context(ctx, name);
        for path in paths {
            ctx.app.hooks.emit(
                event::File::Edited {
                    file: path,
                    tool: name,
                },
                &context,
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

pub use skills::list as skill_list_with_paths;

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
