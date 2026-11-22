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
pub struct Input {
    pub pattern: String,
    pub path: Option<String>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct GrepInput {
    #[serde(flatten)]
    pub search: Input,
    pub include: Option<String>,
    /// Number of lines of context before and after each match.
    pub context: Option<usize>,
    /// Return only paths containing matches.
    #[serde(rename = "filesOnly")]
    pub files_only: Option<bool>,
    /// Treat the pattern as literal text instead of a regular expression.
    pub literal: Option<bool>,
    /// Whether matching is case sensitive (defaults to true).
    #[serde(rename = "caseSensitive")]
    pub case_sensitive: Option<bool>,
}

#[derive(Clone, Copy)]
pub enum Mode<'a> {
    Glob,
    Grep(&'a GrepInput),
}

pub async fn run(
    directory: &str,
    mode: Mode<'_>,
    input: &Input,
    cancel: CancellationToken,
) -> Result<ToolOutput, OperationError> {
    let base = resolve_path(directory, input.path.as_deref().unwrap_or("."));
    let metadata = tokio::fs::metadata(&base)
        .await
        .map_err(|error| failure(format!("Cannot search {}: {error}", base.display())))?;
    let pattern = &input.pattern;
    let glob = matches!(mode, Mode::Glob);
    let paths_only =
        glob || matches!(mode, Mode::Grep(options) if options.files_only == Some(true));
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
        command.args(["--hidden", "--no-messages"]);
        if paths_only {
            command.args(["--files-with-matches", "--null"]);
        } else {
            command.arg("--json");
        }
        if let Mode::Grep(options) = mode {
            if let Some(include) = &options.include {
                command.arg(format!("--glob={include}"));
            }
            if options.literal == Some(true) {
                command.arg("--fixed-strings");
            }
            if options.case_sensitive == Some(false) {
                command.arg("--ignore-case");
            }
            if options.files_only != Some(true) {
                if let Some(context) = options.context {
                    command.arg(format!("--context={context}"));
                }
            }
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
            let mut lines: Vec<(String, bool)> = Vec::new();
            let mut truncated = false;
            let mut stopped = false;
            loop {
                record.clear();
                let bytes = (&mut reader)
                    .take(MAX_RECORD as u64 + 1)
                    .read_until(if paths_only { 0 } else { b'\n' }, &mut record)
                    .await
                    .map_err(|error| failure(format!("Cannot read ripgrep output: {error}")))?;
                if bytes == 0 {
                    break;
                }
                if record.len() > MAX_RECORD {
                    return Err(failure(format!("Ripgrep record exceeds {MAX_RECORD} bytes; narrow the pattern or search path")));
                }
                let (line, is_match) = if paths_only {
                    record.pop();
                    let path = std::str::from_utf8(&record)
                        .map_err(|_| failure("ripgrep returned a non-UTF-8 path"))?;
                    (
                        cwd.join(path.trim_start_matches("./"))
                            .display()
                            .to_string(),
                        true,
                    )
                } else {
                    let event: Value = serde_json::from_slice(&record)
                        .map_err(|error| failure(format!("Invalid ripgrep output: {error}")))?;
                    let is_match = event["type"] == "match";
                    if !is_match && event["type"] != "context" {
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
                    (
                        format!(
                            "{}:{}:{preview}",
                            cwd.join(path.trim_start_matches("./")).display(),
                            data["line_number"]
                        ),
                        is_match,
                    )
                };
                if lines.len() == MAX_RESULTS {
                    truncated = true;
                    if is_match {
                        if let Some(index) = lines.iter().position(|(_, matched)| !matched) {
                            lines.remove(index);
                        } else {
                            stopped = true;
                            let _ = child.start_kill();
                            break;
                        }
                    } else {
                        continue;
                    }
                }
                lines.push((line, is_match));
            }
            let count = lines.len();
            let mut output = String::new();
            for (line, _) in lines {
                output.push_str(&line);
                output.push('\n');
            }
            Ok((output, count, truncated, stopped))
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
    let ((mut output, count, truncated, stopped), errors) = result?;
    if !stopped
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
                super::run(directory, Mode::Grep(&input), &input.search, cancel).await
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
        assert!(included.output.contains(
            Path::new("node_modules")
                .join("explicit.rs")
                .to_str()
                .unwrap()
        ));
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
    async fn ripgrep_context_files_and_matching_options() {
        let directory = Directory::new();
        std::fs::create_dir(directory.0.path().join(".git")).unwrap();
        std::fs::write(directory.0.path().join(".gitignore"), "ignored.txt\n").unwrap();
        std::fs::write(directory.0.path().join("ignored.txt"), "A.B\n").unwrap();
        std::fs::write(
            directory.0.path().join("visible.txt"),
            "one\na.b\nthree\nA.B\nfive\n",
        )
        .unwrap();
        let context = run(
            directory.dir(),
            "grep",
            &json!({"pattern":"a.b", "literal":true, "caseSensitive":false, "context":1}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        let lines: Vec<_> = context.output.lines().collect();
        assert_eq!(lines.len(), 5, "{}", context.output);
        for (index, text) in ["one", "a.b", "three", "A.B", "five"].iter().enumerate() {
            assert!(lines[index].ends_with(&format!(":{}:{text}", index + 1)));
        }
        let files = run(directory.dir(), "grep", &json!({"pattern":"a.b", "literal":true, "caseSensitive":false, "filesOnly":true, "context":1}), CancellationToken::new()).await.unwrap();
        assert_eq!(files.metadata["count"], 1);
        assert!(files.output.trim_end().ends_with("visible.txt"));
        assert!(!files.output.contains("ignored.txt"));
        let sensitive = run(
            directory.dir(),
            "grep",
            &json!({"pattern":"A.B", "literal":true}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(sensitive.metadata["count"], 1);
        assert!(sensitive.output.contains(":4:A.B"));
        let regex = run(
            directory.dir(),
            "grep",
            &json!({"pattern":"a.b"}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(regex.metadata["count"], 1);
    }

    #[tokio::test]
    async fn context_bound_preserves_late_matches_and_reports_omissions() {
        let directory = Directory::new();
        let mut content = "before\n".repeat(200);
        content.push_str("needle\nafter\n");
        std::fs::write(directory.0.path().join("late.txt"), content).unwrap();
        let result = run(
            directory.dir(),
            "grep",
            &json!({"pattern":"needle", "context":200}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(result.metadata["count"], 100);
        assert_eq!(result.metadata["truncated"], true);
        assert!(result.output.contains(":201:needle"));
        assert!(result.output.contains("Results truncated at 100"));
    }

    #[tokio::test]
    async fn files_only_stops_after_first_match_in_each_file() {
        let directory = Directory::new();
        std::fs::write(
            directory.0.path().join("first.txt"),
            format!("needle\nneedle{}\n", "x".repeat(MAX_RECORD)),
        )
        .unwrap();
        std::fs::write(directory.0.path().join("second.txt"), "needle\nneedle\n").unwrap();
        let result = run(
            directory.dir(),
            "grep",
            &json!({"pattern":"needle", "filesOnly":true}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(result.metadata["count"], 2);
        assert!(result.output.contains("first.txt"));
        assert!(result.output.contains("second.txt"));
        assert_eq!(result.metadata["truncated"], false);
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
