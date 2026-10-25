//! Prepare one complete file transaction before permission evaluation.
use super::{failure, files, patch, required, resolve_path, ToolCtx};
use crate::types::{ToolError, ToolOutput};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

pub(super) const KEY: &str = "_ocminiFileChanges";

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Change {
    pub path: PathBuf,
    pub before: Option<String>,
    pub after: Option<String>,
}

/// Resolve existing ancestor links as well as links to an existing target.
fn target(path: &Path) -> Result<PathBuf, ToolError> {
    let mut ancestor = path;
    let mut tail = Vec::new();
    loop {
        match ancestor.canonicalize() {
            Ok(mut root) => {
                for component in tail.iter().rev() {
                    root.push(component);
                }
                return Ok(root);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if ancestor.symlink_metadata().is_ok() {
                    return Err(failure(format!(
                        "Cannot resolve {}: {error}",
                        ancestor.display()
                    )));
                }
                tail.push(
                    ancestor
                        .file_name()
                        .ok_or_else(|| failure("Invalid file path"))?
                        .to_owned(),
                );
                ancestor = ancestor
                    .parent()
                    .ok_or_else(|| failure("Invalid file path"))?;
            }
            Err(error) => {
                return Err(failure(format!(
                    "Cannot resolve {}: {error}",
                    ancestor.display()
                )))
            }
        }
    }
}

fn current(path: &Path) -> Result<Option<String>, ToolError> {
    match path.symlink_metadata() {
        Ok(_) => files::text_file(path).map(Some),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(failure(format!(
            "Cannot inspect {}: {error}",
            path.display()
        ))),
    }
}

fn collect(
    directory: &str,
    name: &str,
    input: &Value,
    cancel: &AtomicBool,
) -> Result<Vec<Change>, ToolError> {
    match name {
        "write" => {
            let path = target(&resolve_path(directory, required(input, "filePath")?))?;
            Ok(vec![Change {
                before: current(&path)?,
                path,
                after: Some(required(input, "content")?.into()),
            }])
        }
        "edit" => {
            let (path, before, after, _) = files::prepare_edit(directory, input)?;
            Ok(vec![Change {
                path: target(&path)?,
                before: Some(before),
                after: Some(after),
            }])
        }
        _ => {
            let text = input["patchText"]
                .as_str()
                .or_else(|| input["patch"].as_str())
                .ok_or_else(|| failure("patchText must be a string"))?;
            let (changes, _) = patch::prepare(directory, text, cancel)?;
            changes
                .into_iter()
                .map(|change| {
                    let decode = |text: Option<Vec<u8>>| {
                        text.map(String::from_utf8)
                            .transpose()
                            .map_err(|_| failure("File change must contain UTF-8 text"))
                    };
                    Ok(Change {
                        path: target(&change.path)?,
                        before: decode(change.before)?,
                        after: decode(change.after)?,
                    })
                })
                .collect()
        }
    }
}

fn validate(changes: &[Change], cancel: &AtomicBool) -> Result<(), ToolError> {
    if changes.is_empty() {
        return Err(failure("File change batch must not be empty"));
    }
    let mut paths = HashSet::new();
    for change in changes {
        if cancel.load(Ordering::Acquire) {
            return Err(failure("File change cancelled before writing"));
        }
        if !change.path.is_absolute() || target(&change.path)? != change.path {
            return Err(failure(
                "Effective file path changed during preparation; retry the operation",
            ));
        }
        if !paths.insert(&change.path) {
            return Err(failure(format!(
                "File change modifies {} more than once",
                change.path.display()
            )));
        }
        if change.before.is_none() && change.after.is_none() {
            return Err(failure("File change needs before or after content"));
        }
        if change
            .before
            .as_ref()
            .is_some_and(|text| text.contains('\0'))
            || change
                .after
                .as_ref()
                .is_some_and(|text| text.contains('\0'))
        {
            return Err(failure("File change contains binary content"));
        }
        if current(&change.path)? != change.before {
            return Err(failure(format!(
                "File changed since preparation: {}",
                change.path.display()
            )));
        }
    }
    Ok(())
}

pub(super) async fn prepare(ctx: &ToolCtx, name: &str, input: Value) -> Result<Value, ToolError> {
    prepare_values(
        &ctx.dir,
        &ctx.app.hooks,
        super::hook_context(ctx, name),
        ctx.cancel.clone(),
        name,
        input,
    )
    .await
}

async fn prepare_values(
    directory: &str,
    hooks: &crate::hooks::Hooks,
    context: Value,
    cancel: std::sync::Arc<AtomicBool>,
    name: &str,
    mut input: Value,
) -> Result<Value, ToolError> {
    let work_directory = directory.to_owned();
    let tool = name.to_owned();
    let args = input.clone();
    let work_cancel = cancel.clone();
    let original =
        tokio::task::spawn_blocking(move || collect(&work_directory, &tool, &args, &work_cancel))
            .await
            .map_err(|e| failure(e.to_string()))??;
    let payload = json!({"changes":original});
    let effective = hooks
        .run("file.change.before", &context, payload, Some(&cancel))
        .await
        .map_err(failure)?
        .input;
    let mut changes: Vec<Change> = serde_json::from_value(effective["changes"].clone())
        .map_err(|e| failure(format!("Invalid file change batch: {e}")))?;
    for change in &mut changes {
        if change
            .path
            .to_str()
            .is_none_or(|path| path.is_empty() || path.contains(['\0', '\n', '\r']))
        {
            return Err(failure("File change path is invalid"));
        }
        change.path = target(&resolve_path(directory, change.path.to_str().unwrap()))?;
        if let Some(previous) = original
            .iter()
            .find(|original| original.path == change.path)
        {
            if change.before != previous.before {
                return Err(failure(
                    "A file hook cannot replace the original conflict baseline",
                ));
            }
        } else {
            // A redirected path receives its own baseline before permission evaluation.
            change.before = current(&change.path)?;
        }
    }
    validate(&changes, &cancel)?;
    match name {
        "write" | "edit" => {
            if changes.len() != 1 || changes[0].after.is_none() {
                return Err(failure(
                    "Write/edit hooks must return one file with after content",
                ));
            }
            input["filePath"] = json!(changes[0].path);
            if name == "write" {
                input["content"] = json!(changes[0].after);
            } else {
                if changes[0]
                    .before
                    .as_ref()
                    .is_none_or(|text| text.is_empty())
                {
                    return Err(failure("Edit target must be an existing nonempty file"));
                }
                input["oldString"] = json!(changes[0].before);
                input["newString"] = json!(changes[0].after);
                input["replaceAll"] = json!(false);
            }
        }
        _ => {
            input["patchText"] = json!(as_patch(&changes));
            input.as_object_mut().unwrap().remove("patch");
        }
    }
    input[KEY] = json!(changes);
    Ok(input)
}

fn as_patch(changes: &[Change]) -> String {
    let mut patch = String::from("*** Begin Patch\n");
    for change in changes {
        match (&change.before, &change.after) {
            (None, Some(after)) => {
                patch.push_str(&format!("*** Add File: {}\n", change.path.display()));
                for line in after.lines() {
                    patch.push_str(&format!("+{line}\n"));
                }
            }
            (Some(_), None) => {
                patch.push_str(&format!("*** Delete File: {}\n", change.path.display()))
            }
            (Some(before), Some(after)) => {
                patch.push_str(&format!("*** Update File: {}\n@@\n", change.path.display()));
                for line in before.lines() {
                    patch.push_str(&format!("-{line}\n"));
                }
                for line in after.lines() {
                    patch.push_str(&format!("+{line}\n"));
                }
                if before.is_empty() && after.is_empty() {
                    patch.push_str(" \n");
                }
            }
            _ => unreachable!(),
        }
    }
    patch.push_str("*** End Patch");
    patch
}

pub(super) fn paths(input: &Value) -> Option<Vec<PathBuf>> {
    serde_json::from_value::<Vec<Change>>(input.get(KEY)?.clone())
        .ok()
        .map(|changes| changes.into_iter().map(|change| change.path).collect())
}

pub(super) fn commit(input: &Value, cancel: &AtomicBool) -> Result<ToolOutput, ToolError> {
    let changes: Vec<Change> = serde_json::from_value(input[KEY].clone())
        .map_err(|e| failure(format!("Invalid prepared file change: {e}")))?;
    validate(&changes, cancel)?;
    let changes: Vec<patch::Change> = changes
        .into_iter()
        .map(|change| patch::Change {
            permissions: std::fs::metadata(&change.path)
                .ok()
                .map(|metadata| metadata.permissions()),
            path: change.path,
            before: change.before.map(String::into_bytes),
            after: change.after.map(String::into_bytes),
        })
        .collect();
    for (index, change) in changes.iter().enumerate() {
        if let Err(error) = patch::commit(change, false) {
            let mut rollback_errors = Vec::new();
            for previous in changes[..=index].iter().rev() {
                if let Err(error) = patch::commit(previous, true) {
                    rollback_errors.push(error);
                }
            }
            return Err(failure(format!(
                "File change failed: {error}; rollback {}",
                if rollback_errors.is_empty() {
                    "completed".into()
                } else {
                    rollback_errors.join("; ")
                }
            )));
        }
    }
    let files: Vec<Value> = changes.iter().map(|change| json!({"path":change.path,"type":if change.before.is_none(){"add"}else if change.after.is_none(){"delete"}else{"update"},"bytes":change.after.as_ref().map_or(0,Vec::len)})).collect();
    Ok(ToolOutput {
        title: format!("Updated {} file(s)", files.len()),
        output: files
            .iter()
            .map(|file| {
                format!(
                    "{} {}",
                    file["type"].as_str().unwrap(),
                    file["path"].as_str().unwrap()
                )
            })
            .collect::<Vec<_>>()
            .join("\n"),
        metadata: json!({"files":files}),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn conflict_in_later_file_prevents_all_writes() {
        let root = tempfile::tempdir().unwrap();
        let a = root.path().join("a");
        let b = root.path().join("b");
        std::fs::write(&a, "one").unwrap();
        std::fs::write(&b, "two").unwrap();
        let input = json!({(KEY):[{"path":a,"before":"one","after":"changed"},{"path":b,"before":"two","after":"changed"}]});
        std::fs::write(&b, "external edit").unwrap();
        assert!(commit(&input, &AtomicBool::new(false))
            .unwrap_err()
            .message
            .contains("changed since preparation"));
        assert_eq!(std::fs::read_to_string(a).unwrap(), "one");
        assert_eq!(std::fs::read_to_string(b).unwrap(), "external edit");
    }
    #[test]
    fn transaction_preserves_exact_content_and_rejects_aliases() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("file");
        let input = json!({(KEY):[{"path":path,"before":null,"after":"no final newline"}]});
        commit(&input, &AtomicBool::new(false)).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "no final newline");
        let duplicate = vec![
            Change {
                path: path.clone(),
                before: Some("no final newline".into()),
                after: Some("one".into()),
            },
            Change {
                path,
                before: Some("no final newline".into()),
                after: None,
            },
        ];
        assert!(validate(&duplicate, &AtomicBool::new(false))
            .unwrap_err()
            .message
            .contains("more than once"));
    }
    #[tokio::test]
    #[cfg(unix)]
    async fn hook_redirects_path_and_content_before_any_effect() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().to_str().unwrap();
        let cfg: crate::config::Config = serde_json::from_value(json!({"ocmini":{"hooks":{"handlers":[{"id":"redirect","on":"file.change.before","command":["/bin/sh","-c", "cat >/dev/null; printf '%s' '{\"set\":{\"changes\":[{\"path\":\"redirected\",\"before\":null,\"after\":\"hook content\"}]}}'"]}]}}})).unwrap();
        let hooks = crate::hooks::Hooks::for_config(&cfg, directory).unwrap();
        let cancel = std::sync::Arc::new(AtomicBool::new(false));
        let input = prepare_values(
            directory,
            &hooks,
            json!({"directory":directory,"sessionID":"session"}),
            cancel.clone(),
            "write",
            json!({"filePath":"original","content":"model content"}),
        )
        .await
        .unwrap();
        assert_eq!(
            input["filePath"],
            root.path().join("redirected").to_str().unwrap()
        );
        assert_eq!(input["content"], "hook content");
        assert_eq!(paths(&input).unwrap(), vec![root.path().join("redirected")]);
        assert!(!root.path().join("redirected").exists());
        commit(&input, &cancel).unwrap();
        assert!(!root.path().join("original").exists());
        assert_eq!(
            std::fs::read_to_string(root.path().join("redirected")).unwrap(),
            "hook content"
        );
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn blocked_whole_patch_has_no_effects() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().to_str().unwrap();
        std::fs::write(root.path().join("existing"), "original\n").unwrap();
        let cfg: crate::config::Config = serde_json::from_value(json!({"ocmini":{"hooks":{"handlers":[{"id":"veto","on":"file.change.before","command":["/bin/sh","-c", "cat >/dev/null; printf '%s' '{\"decision\":\"block\",\"reason\":\"batch rejected\"}'"]}]}}})).unwrap();
        let hooks = crate::hooks::Hooks::for_config(&cfg, directory).unwrap();
        let result = prepare_values(directory,&hooks,json!({"directory":directory}),std::sync::Arc::new(AtomicBool::new(false)),"apply_patch",json!({"patchText":"*** Begin Patch\n*** Add File: added\n+new\n*** Update File: existing\n@@\n-original\n+changed\n*** End Patch"})).await;
        assert!(result.unwrap_err().message.contains("batch rejected"));
        assert!(!root.path().join("added").exists());
        assert_eq!(
            std::fs::read_to_string(root.path().join("existing")).unwrap(),
            "original\n"
        );
    }
}
