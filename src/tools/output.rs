use super::{cap_out, failure};
use crate::types::{ToolError, ToolOutput};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

pub(super) const MAX_OUTPUT: usize = 50 * 1024;
const RETENTION: Duration = Duration::from_secs(7 * 24 * 60 * 60);

pub(super) fn directory() -> PathBuf {
    Path::new(&crate::config::data_home())
        .join("ocmini")
        .join("tool-output")
}

pub(super) fn create(directory: &Path) -> std::io::Result<tempfile::NamedTempFile> {
    std::fs::create_dir_all(directory)?;
    tempfile::Builder::new()
        .prefix("tool_")
        .suffix(".txt")
        .tempfile_in(directory)
}

pub(super) fn with_file(mut output: ToolOutput, path: &Path) -> ToolOutput {
    output.output = cap_out(&output.output, MAX_OUTPUT);
    output.output.push_str(&format!(
        "\n\n[Full tool output saved to {}. Read or search this file for omitted content.]",
        path.display()
    ));
    output.metadata["truncated"] = true.into();
    output.metadata["outputPath"] = path.to_string_lossy().into_owned().into();
    output
}

pub(super) async fn limit(
    mut output: ToolOutput,
    directory: PathBuf,
) -> Result<ToolOutput, ToolError> {
    if output.metadata.get("truncated").is_none() {
        output.metadata["truncated"] = false.into();
    }
    if output.output.len() <= MAX_OUTPUT || output.metadata["outputPath"].is_string() {
        return Ok(output);
    }
    tokio::task::spawn_blocking(move || {
        let save = || -> std::io::Result<PathBuf> {
            let mut file = create(&directory)?;
            file.write_all(output.output.as_bytes())?;
            file.into_temp_path().keep().map_err(std::io::Error::from)
        };
        let path = save().map_err(|e| failure(format!("Cannot save tool output: {e}")))?;
        Ok(with_file(output, &path))
    })
    .await
    .map_err(|e| failure(format!("Cannot save tool output: {e}")))?
}

pub(super) fn cleanup(directory: &Path) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with("tool_") || !name.ends_with(".txt") {
            continue;
        }
        let expired = entry.metadata().is_ok_and(|metadata| {
            metadata.is_file()
                && metadata
                    .modified()
                    .ok()
                    .and_then(|time| SystemTime::now().duration_since(time).ok())
                    .is_some_and(|age| age > RETENTION)
        });
        if expired {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn large_unicode_output_is_recoverable_and_small_output_creates_nothing() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("output");
        let small = ToolOutput {
            output: "x".repeat(MAX_OUTPUT),
            title: "small".into(),
            metadata: json!({"attachments":[{"type":"image"}]}),
        };
        let small = limit(small, directory.clone()).await.unwrap();
        assert_eq!(small.output.len(), MAX_OUTPUT);
        assert!(!directory.exists());
        let text = format!(
            "BEGIN\n{}\nMIDDLE_MARKER\n{}\nEND",
            "🙂".repeat(20_000),
            "é".repeat(40_000)
        );
        let large = ToolOutput {
            output: text.clone(),
            title: "large".into(),
            metadata: small.metadata,
        };
        let large = limit(large, directory.clone()).await.unwrap();
        assert!(large.output.starts_with("BEGIN"));
        assert!(large.output.contains("END"));
        assert!(!large.output.contains("MIDDLE_MARKER"));
        assert!(large.output.len() < MAX_OUTPUT + 1024);
        assert_eq!(large.metadata["truncated"], true);
        assert_eq!(large.metadata["attachments"][0]["type"], "image");
        assert_eq!(
            std::fs::read_to_string(large.metadata["outputPath"].as_str().unwrap()).unwrap(),
            text
        );
        let again = limit(large, directory.clone()).await.unwrap();
        assert!(again.output.contains("Full tool output saved"));
        assert_eq!(std::fs::read_dir(directory).unwrap().count(), 1);
    }

    #[test]
    fn cleanup_removes_only_expired_output_files() {
        let root = tempfile::tempdir().unwrap();
        for name in ["tool_old.txt", "tool_new.txt", "unrelated.txt"] {
            let file = std::fs::File::create(root.path().join(name)).unwrap();
            if name != "tool_new.txt" {
                file.set_modified(SystemTime::now() - RETENTION - Duration::from_secs(1))
                    .unwrap();
            }
        }
        cleanup(root.path());
        assert!(!root.path().join("tool_old.txt").exists());
        assert!(root.path().join("tool_new.txt").exists());
        assert!(root.path().join("unrelated.txt").exists());
    }

    #[tokio::test]
    async fn a_failed_save_is_reported_instead_of_discarding_output() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("file-not-directory");
        std::fs::write(&path, "keep").unwrap();
        let result = limit(
            ToolOutput {
                output: "x".repeat(MAX_OUTPUT + 1),
                title: "large".into(),
                metadata: json!({}),
            },
            path.clone(),
        )
        .await;
        assert!(result
            .unwrap_err()
            .message
            .contains("Cannot save tool output"));
        assert_eq!(std::fs::read_to_string(path).unwrap(), "keep");
    }
}
