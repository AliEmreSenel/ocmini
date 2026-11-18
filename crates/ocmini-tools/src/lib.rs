//! Tool implementations shared by the main process and the isolated runner.
pub mod changes;
pub mod files;
pub mod http;
pub mod output;
pub mod patch;
pub mod process_group;
pub mod search;
pub mod shell;
pub mod types;
pub mod web;
pub use process_group as process;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use types::{OperationError, ToolOutput};

pub fn failure(message: impl Into<String>) -> OperationError {
    message.into().into()
}
pub fn decode<T: serde::de::DeserializeOwned>(input: &Value) -> Result<T, OperationError> {
    T::deserialize(input).map_err(|e| failure(format!("Invalid tool input: {e}")))
}
pub fn alias<'a>(input: &'a Value, key: &str, fallback: &str) -> std::borrow::Cow<'a, Value> {
    if !input[key].is_string() && input[fallback].is_string() {
        let mut input = input.clone();
        input[key] = input[fallback].clone();
        std::borrow::Cow::Owned(input)
    } else {
        std::borrow::Cow::Borrowed(input)
    }
}
pub fn resolve_path(directory: &str, path: &str) -> PathBuf {
    config::normalize_path(&Path::new(directory).join(path))
}
pub fn cap_out(value: &str, max: usize) -> String {
    if value.len() <= max {
        return value.into();
    }
    let mut head = max / 2;
    while !value.is_char_boundary(head) {
        head -= 1;
    }
    let mut tail = value.len() - max / 2;
    while !value.is_char_boundary(tail) {
        tail += 1;
    }
    format!(
        "{}\n\n[... {} bytes truncated ...]\n{}",
        &value[..head],
        tail - head,
        &value[tail..]
    )
}
pub mod config {
    pub use crate::frontmatter::markdown_frontmatter;
    use std::path::{Component, Path, PathBuf};
    pub fn data_home() -> String {
        std::env::var("XDG_DATA_HOME").unwrap_or_else(|_| {
            format!(
                "{}/.local/share",
                std::env::var("HOME").unwrap_or_else(|_| ".".into())
            )
        })
    }
    pub fn normalize_path(path: &Path) -> PathBuf {
        let mut out = PathBuf::new();
        for part in path.components() {
            match part {
                Component::CurDir => {}
                Component::ParentDir => {
                    out.pop();
                }
                part => out.push(part),
            }
        }
        out
    }
}
#[cfg(test)]
fn test_tempdir() -> std::io::Result<tempfile::TempDir> {
    tempfile::tempdir_in(std::env::temp_dir().canonicalize()?)
}
#[cfg(test)]
pub mod tools {
    pub use crate::{decode, files};
}

/// The same typed implementations serve both modes. Hooks and approvals belong
/// to the caller; these functions perform effects under the caller's OS domain.
pub async fn run_effect(
    directory: &str,
    session: &str,
    name: &str,
    args: &Value,
    cancel: CancellationToken,
) -> Result<ToolOutput, OperationError> {
    let result = match name {
        "background_read" => shell::read_background(session, &decode(args)?),
        "read" => {
            let input = decode(args)?;
            let dir = directory.to_owned();
            let token = cancel.clone();
            tokio::task::spawn_blocking(move || files::read(&dir, &input, &token))
                .await
                .map_err(|e| failure(e.to_string()))?
        }
        "glob" => {
            search::run(
                directory,
                search::Mode::Glob,
                &decode(args)?,
                cancel.clone(),
            )
            .await
        }
        "grep" => {
            let input: search::GrepInput = decode(args)?;
            search::run(
                directory,
                search::Mode::Grep(input.include.as_deref()),
                &input.search,
                cancel.clone(),
            )
            .await
        }
        "webfetch" => web::fetch(&decode(args)?, cancel.clone()).await,
        "bash" | "shell" => {
            let command = args["command"]
                .as_str()
                .ok_or_else(|| failure("command is required"))?;
            let path = resolve_path(
                directory,
                args["workdir"]
                    .as_str()
                    .or(args["cwd"].as_str())
                    .unwrap_or("."),
            );
            let env = args
                .get("env")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            if args["background"] == true {
                shell::shell_background_with_env(
                    command,
                    &path,
                    session,
                    &output::directory(),
                    &env,
                )
                .await
            } else {
                let timeout = args["timeout"].as_u64().unwrap_or(120_000);
                if timeout == 0 {
                    return Err(failure(
                        "timeout must be a positive integer in milliseconds",
                    ));
                }
                shell::shell_with_env(
                    command,
                    &path,
                    std::time::Duration::from_millis(timeout),
                    cancel.clone(),
                    &output::directory(),
                    &env,
                )
                .await
            }
        }
        "write" | "edit" | "apply_patch" | "patch" => {
            prepare_file(directory, name, args, cancel.clone())
                .await?
                .run(cancel.clone())
                .await
        }
        _ => Err(failure(format!("Unknown tool {name:?}"))),
    }?;
    output::limit(result, output::directory()).await
}
pub async fn prepare_file(
    directory: &str,
    name: &str,
    args: &Value,
    cancel: CancellationToken,
) -> Result<Arc<changes::Plan>, OperationError> {
    changes::prepare_plain(directory, name, args, cancel)
        .await
        .map(Arc::new)
}

pub mod catalog;
pub mod frontmatter;
pub mod hook_types;
pub mod hooks;
pub mod host;
pub mod skills;
pub use catalog::*;
pub use changes::target as canonical_target;
pub use files::media_type;
pub use shell::{cancel_background_jobs, stop_background_jobs};
pub use skills::list as skill_list_with_paths;
pub mod effects;
pub mod question;
pub mod task;
pub mod todo;
