use super::{failure, resolve_path};
use crate::types::{OperationError, ToolOutput};
use serde_json::{json, Value};
use std::path::Path;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio_util::sync::CancellationToken;

const MAX_RESULTS: usize = 100;
const MAX_RECORD: usize = 64 * 1024;
const MAX_ERROR: usize = 8 * 1024;

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub(super) struct Input {
    pub pattern: String,
    pub path: Option<String>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub(super) struct GrepInput {
    #[serde(flatten)]
    pub search: Input,
    pub include: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Mode<'a> {
    Glob,
    Grep(Option<&'a str>),
}

pub(super) async fn run(
    directory: &str,
    mode: Mode<'_>,
    input: &Input,
    cancel: CancellationToken,
) -> Result<ToolOutput, OperationError> {
    let base = resolve_path(directory, input.path.as_deref().unwrap_or("."));
    let metadata = std::fs::metadata(&base)
        .map_err(|error| failure(format!("Cannot search {}: {error}", base.display())))?;
    let pattern = &input.pattern;
    let glob = matches!(mode, Mode::Glob);
    if !glob && pattern.is_empty() {
        return Err(failure("pattern is required"));
    }
    if glob && !metadata.is_dir() {
        return Err(failure("glob path must be a directory"));
    }
    let cwd = if metadata.is_dir() {
        base.as_path()
    } else {
        base.parent()
            .ok_or_else(|| failure("Search path has no parent"))?
    };
    let mut command = tokio::process::Command::new("rg");
    command.arg("--no-config");
    if glob {
        command
            .args(["--files", "--null"])
            .arg(format!("--glob={pattern}"));
    } else {
        command.args(["--json", "--hidden", "--no-messages"]);
        if let Mode::Grep(Some(include)) = mode {
            command.arg(format!("--glob={include}"));
        }
    }
    command.arg("--glob=!**/.git/**").arg("--");
    if !glob {
        command.arg(pattern);
    }
    command.arg(if metadata.is_dir() {
        Path::new(".")
    } else {
        Path::new(
            base.file_name()
                .ok_or_else(|| failure("Search path has no filename"))?,
        )
    });
    command
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    let mut child = command.spawn().map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            failure("Search requires ripgrep (rg) on PATH. Install ripgrep to use grep and glob.")
        } else {
            failure(format!("Cannot start ripgrep: {error}"))
        }
    })?;
    let group = crate::process::Group::new(
        child
            .id()
            .ok_or_else(|| failure("ripgrep has no process id"))?,
    );
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| failure("ripgrep stdout unavailable"))?;
    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| failure("ripgrep stderr unavailable"))?;
    let operation = async {
        let output = async {
            let mut reader = BufReader::new(stdout);
            let mut record = Vec::new();
            let mut output = String::new();
            let mut count = 0;
            let mut truncated = false;
            loop {
                record.clear();
                let bytes = (&mut reader)
                    .take(MAX_RECORD as u64 + 1)
                    .read_until(if glob { 0 } else { b'\n' }, &mut record)
                    .await
                    .map_err(|error| failure(format!("Cannot read ripgrep output: {error}")))?;
                if bytes == 0 {
                    break;
                }
                if record.len() > MAX_RECORD {
                    return Err(failure(format!("Ripgrep record exceeds {MAX_RECORD} bytes; narrow the pattern or search path")));
                }
                let line = if glob {
                    record.pop();
                    let path = String::from_utf8(record.clone())
                        .map_err(|_| failure("ripgrep returned a non-UTF-8 path"))?;
                    cwd.join(path.trim_start_matches("./"))
                        .display()
                        .to_string()
                } else {
                    let event: Value = serde_json::from_slice(&record)
                        .map_err(|error| failure(format!("Invalid ripgrep output: {error}")))?;
                    if event["type"] != "match" {
                        continue;
                    }
                    let data = &event["data"];
                    let path = data["path"]["text"]
                        .as_str()
                        .ok_or_else(|| failure("ripgrep returned a non-UTF-8 path"))?;
                    let text = data["lines"]["text"]
                        .as_str()
                        .ok_or_else(|| failure("ripgrep returned non-UTF-8 matching text"))?;
                    let text = text.trim_end_matches(['\r', '\n']);
                    let mut preview: String = text.chars().take(2_000).collect();
                    if preview.len() < text.len() {
                        preview.push_str("...");
                    }
                    format!(
                        "{}:{}:{preview}",
                        cwd.join(path.trim_start_matches("./")).display(),
                        data["line_number"]
                    )
                };
                if count == MAX_RESULTS {
                    truncated = true;
                    let _ = child.start_kill();
                    break;
                }
                output.push_str(&line);
                output.push('\n');
                count += 1;
            }
            Ok((output, count, truncated))
        };
        let errors = async {
            let mut errors = Vec::new();
            let mut buffer = [0; 4096];
            loop {
                let count = stderr
                    .read(&mut buffer)
                    .await
                    .map_err(|error| failure(format!("Cannot read ripgrep errors: {error}")))?;
                if count == 0 {
                    break;
                }
                errors.extend_from_slice(
                    &buffer[..count.min(MAX_ERROR.saturating_sub(errors.len()))],
                );
            }
            Ok::<_, OperationError>(String::from_utf8_lossy(&errors).trim().to_string())
        };
        tokio::try_join!(output, errors)
    };
    let result = tokio::select! {
        result = operation => result,
        _ = cancel.cancelled() => Err(failure("Search cancelled")),
    };
    drop(group);
    let status = child
        .wait()
        .await
        .map_err(|error| failure(format!("Cannot wait for ripgrep: {error}")))?;
    let ((mut output, count, truncated), errors) = result?;
    if !truncated
        && !matches!(status.code(), Some(0 | 1))
        && (count == 0
            || status.code() != Some(2)
            || errors.contains("regex parse error")
            || errors.contains("error parsing glob"))
    {
        return Err(failure(if errors.is_empty() {
            format!("ripgrep exited with {status}")
        } else {
            errors
        }));
    }
    if truncated {
        output.push_str("\n[Results truncated at 100; narrow the path or pattern]\n");
    }
    if !errors.is_empty() {
        output.push_str(&format!("\n[ripgrep: {errors}]\n"));
    }
    Ok(ToolOutput {
        title: format!("{count} matches"),
        output,
        metadata: json!({"matches":count,"count":count,"truncated":truncated,"path":base,"partial":!errors.is_empty()}),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fmt::Write as _;
    async fn run(
        directory: &str,
        name: &str,
        input: &Value,
        cancel: CancellationToken,
    ) -> Result<ToolOutput, OperationError> {
        match name {
            "glob" => {
                super::run(directory, Mode::Glob, &crate::tools::decode(input)?, cancel).await
            }
            "grep" => {
                let input: GrepInput = crate::tools::decode(input)?;
                super::run(
                    directory,
                    Mode::Grep(input.include.as_deref()),
                    &input.search,
                    cancel,
                )
                .await
            }
            _ => panic!("unknown search tool"),
        }
    }

    struct Directory(tempfile::TempDir);
    impl Directory {
        fn new() -> Self {
            Self(tempfile::tempdir().unwrap())
        }
        fn dir(&self) -> &str {
            self.0.path().to_str().unwrap()
        }
    }

    #[tokio::test]
    async fn ripgrep_streams_and_truncates_results_and_reports_bad_patterns() {
        let directory = Directory::new();
        let mut content = String::new();
        for index in 0..150 {
            writeln!(&mut content, "needle {index}").unwrap();
        }
        std::fs::write(directory.0.path().join("matches.txt"), content).unwrap();
        let result = run(
            directory.dir(),
            "grep",
            &json!({"pattern":"needle"}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(result.metadata["matches"], 100);
        assert_eq!(result.metadata["truncated"], true);
        assert!(result.output.contains("Results truncated at 100"));
        for (name, input) in [
            ("grep", json!({"pattern":"["})),
            ("glob", json!({"pattern":"["})),
        ] {
            assert!(run(directory.dir(), name, &input, CancellationToken::new())
                .await
                .is_err());
        }
        assert!(run(directory.dir(), "grep", &json!({"pattern":"needle"}), {
            let cancel = CancellationToken::new();
            cancel.cancel();
            cancel
        })
        .await
        .unwrap_err()
        .message
        .contains("cancelled"));
    }

    #[tokio::test]
    async fn ripgrep_matches_full_lines_and_applies_native_globs() {
        let directory = Directory::new();
        std::fs::create_dir(directory.0.path().join(".git")).unwrap();
        std::fs::write(directory.0.path().join(".gitignore"), "ignored.rs\n").unwrap();
        std::fs::write(directory.0.path().join("ignored.rs"), "needle").unwrap();
        std::fs::create_dir(directory.0.path().join("node_modules")).unwrap();
        std::fs::write(
            directory.0.path().join("node_modules/explicit.rs"),
            "needle",
        )
        .unwrap();
        std::fs::write(
            directory.0.path().join("long.txt"),
            format!("START{}END", "é".repeat(10_000)),
        )
        .unwrap();
        let found = run(
            directory.dir(),
            "grep",
            &json!({"pattern":"^STARTé+END$","include":"long.txt"}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(found.metadata["matches"], 1);
        assert!(found.output.len() < 5 * 1024);
        let included = run(
            directory.dir(),
            "grep",
            &json!({"pattern":"needle","include":"*.rs"}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(included.output.contains("node_modules/explicit.rs"));
        assert!(included.output.contains("ignored.rs"));
    }
    #[tokio::test]
    async fn search_respects_gitignore_but_includes_hidden_files() {
        let directory = Directory::new();
        std::fs::create_dir(directory.0.path().join(".git")).unwrap();
        std::fs::write(directory.0.path().join(".gitignore"), "ignored.txt\n").unwrap();
        std::fs::write(directory.0.path().join("ignored.txt"), "needle").unwrap();
        std::fs::write(directory.0.path().join(".visible.txt"), "needle").unwrap();
        let found = run(
            directory.dir(),
            "grep",
            &json!({"pattern":"needle"}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(found.output.contains(".visible.txt:1:needle"));
        assert!(!found.output.contains("ignored.txt"));
    }
    #[tokio::test]
    async fn glob_search_does_not_drop_brace_alternatives() {
        let directory = Directory::new();
        std::fs::write(directory.0.path().join("file39.rs"), "needle").unwrap();
        let choices = (0..40)
            .map(|index| format!("file{index}"))
            .collect::<Vec<_>>()
            .join(",");
        let result = run(
            directory.dir(),
            "glob",
            &json!({"pattern": format!("{{{choices}}}.rs")}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(result.metadata["matches"], 1);
        assert!(result.output.contains("file39.rs"));
    }
}
