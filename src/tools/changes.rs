//! One file transaction for ordinary tools and hook-transformed tools.
use super::{decode, failure, files, patch, resolve_path, ToolCtx};
use crate::types::{OperationError, ToolOutput};
use patch::Change;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use tokio_util::sync::CancellationToken;

#[derive(serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(super) struct WriteInput {
    file_path: String,
    content: String,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(super) struct EditInput {
    file_path: String,
    #[schemars(length(min = 1))]
    pub old_string: String,
    pub new_string: String,
    #[serde(default)]
    pub replace_all: bool,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(super) struct PatchInput {
    patch_text: String,
}

enum Input {
    Write(WriteInput),
    Edit(EditInput),
    Patch(PatchInput),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Write,
    Edit,
    Patch,
}

impl Input {
    fn parse(name: &str, input: &Value) -> Result<Self, OperationError> {
        match super::Builtin::from_name(name) {
            Some(super::Builtin::Write) => Ok(Self::Write(decode(input)?)),
            Some(super::Builtin::Edit) => Ok(Self::Edit(decode(input)?)),
            Some(super::Builtin::ApplyPatch) => Ok(Self::Patch(decode(&super::alias(
                input,
                "patchText",
                "patch",
            ))?)),
            _ => Err(failure(format!("Unknown file tool {name}"))),
        }
    }
    fn kind(&self) -> Kind {
        match self {
            Self::Write(_) => Kind::Write,
            Self::Edit(_) => Kind::Edit,
            Self::Patch(_) => Kind::Patch,
        }
    }
}

pub struct Plan {
    pub(super) changes: Vec<Change>,
    output: ToolOutput,
}
pub(crate) fn target(path: &Path) -> Result<PathBuf, OperationError> {
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

fn current(path: &Path) -> Result<Option<Vec<u8>>, OperationError> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(failure(format!("Cannot read {}: {e}", path.display()))),
    }
}

#[cfg(test)]
pub(super) fn prepare_sync(
    directory: &str,
    name: &str,
    input: &Value,
    cancel: &CancellationToken,
) -> Result<Plan, OperationError> {
    prepare_plan(directory, &Input::parse(name, input)?, cancel)
}

fn prepare_plan(
    directory: &str,
    input: &Input,
    cancel: &CancellationToken,
) -> Result<Plan, OperationError> {
    let (kind, path, before, after, count) = match input {
        Input::Write(input) => {
            let path = resolve_path(directory, &input.file_path);
            let before = current(&path)?;
            (
                Kind::Write,
                path,
                before,
                input.content.as_bytes().to_vec(),
                0,
            )
        }
        Input::Edit(input) => {
            let path = resolve_path(directory, &input.file_path);
            let (before, after, count) = files::prepare_edit(&path, input)?;
            (
                Kind::Edit,
                path,
                Some(before.into_bytes()),
                after.into_bytes(),
                count,
            )
        }
        Input::Patch(input) => {
            let (mut changes, summaries) = patch::prepare(directory, &input.patch_text, cancel)?;
            for change in &mut changes {
                change.path = target(&change.path)?;
            }
            return Ok(Plan {
                changes,
                output: patch::output(&summaries),
            });
        }
    };
    let output = file_output(kind, &path, after.len(), count);
    let permissions = std::fs::metadata(&path).ok().map(|m| m.permissions());
    Ok(Plan {
        changes: vec![Change {
            path: target(&path)?,
            before,
            after: Some(after),
            permissions,
        }],
        output,
    })
}

fn file_output(kind: Kind, path: &Path, bytes: usize, count: usize) -> ToolOutput {
    ToolOutput {
        title: path.display().to_string(),
        output: if kind == Kind::Write {
            format!("Wrote {} ({bytes} bytes)", path.display())
        } else {
            format!(
                "Edited {} ({count} replacement{})",
                path.display(),
                if count > 1 { "s" } else { "" }
            )
        },
        metadata: if kind == Kind::Write {
            json!({"filepath":path,"bytes":bytes})
        } else {
            json!({"filepath":path,"replacements":count})
        },
    }
}

pub(super) async fn prepare(
    ctx: &ToolCtx,
    name: &str,
    args: Value,
) -> Result<(Value, Plan), OperationError> {
    prepare_values(
        &ctx.dir,
        &ctx.app.hooks,
        super::hook_context(ctx, name),
        ctx.cancel.clone(),
        name,
        args,
    )
    .await
}
async fn prepare_values(
    directory: &str,
    hooks: &crate::hooks::Hooks,
    context: Value,
    cancel: CancellationToken,
    name: &str,
    mut args: Value,
) -> Result<(Value, Plan), OperationError> {
    let input = Input::parse(name, &args)?;
    let kind = input.kind();
    let work_dir = directory.to_owned();
    let work_cancel = cancel.clone();
    let mut plan =
        tokio::task::spawn_blocking(move || prepare_plan(&work_dir, &input, &work_cancel))
            .await
            .map_err(|e| failure(e.to_string()))??;
    if !hooks.matches(super::hooks::FILE_CHANGE_BEFORE, &context) {
        return Ok((args, plan));
    }
    let original = serde_json::to_value(&plan.changes)
        .map_err(|e| failure(format!("File changes must contain UTF-8 text: {e}")))?;
    let mut accepted = None;
    let effective = hooks
        .run_with(
            super::hooks::FILE_CHANGE_BEFORE,
            &context,
            json!({"changes":original}),
            Some(&cancel),
            |_, set| {
                if let Some(wire) = set.get("changes") {
                    accepted = Some(
                        transformed_changes(directory, kind, wire, &plan.changes, &cancel)
                            .map_err(|error| error.message)?,
                    );
                }
                Ok(())
            },
        )
        .await?
        .input;
    if effective["changes"] == original {
        return Ok((args, plan));
    }
    let changes = accepted.ok_or_else(|| failure("Missing accepted file changes"))?;
    if matches!(kind, Kind::Write | Kind::Edit) {
        let change = &changes[0];
        args["filePath"] = json!(change.path);
        if kind == Kind::Write {
            args["content"] = json!(std::str::from_utf8(change.after.as_ref().unwrap())
                .map_err(|e| failure(e.to_string()))?);
        }
        if kind == Kind::Edit {
            args["oldString"] = json!(std::str::from_utf8(change.before.as_ref().unwrap())
                .map_err(|e| failure(e.to_string()))?);
            args["newString"] = json!(std::str::from_utf8(change.after.as_ref().unwrap())
                .map_err(|e| failure(e.to_string()))?);
            args["replaceAll"] = json!(false);
        }
        plan.output = file_output(kind, &change.path, change.after.as_ref().unwrap().len(), 1);
    } else {
        let summaries=changes.iter().map(|c|json!({"path":c.path,"type":if c.before.is_none(){"add"}else if c.after.is_none(){"delete"}else{"update"},"bytes":c.after.as_ref().map_or(0,Vec::len)})).collect::<Vec<_>>();
        plan.output = patch::output(&summaries);
    }
    plan.changes = changes;
    Ok((args, plan))
}

fn transformed_changes(
    directory: &str,
    kind: Kind,
    wire: &Value,
    baseline: &[Change],
    cancel: &CancellationToken,
) -> Result<Vec<Change>, OperationError> {
    let mut changes: Vec<Change> = serde_json::from_value(wire.clone())
        .map_err(|e| failure(format!("Invalid file changes: {e}")))?;
    for change in &mut changes {
        let text = change
            .path
            .to_str()
            .filter(|s| !s.is_empty() && !s.contains(['\0', '\n', '\r']))
            .ok_or_else(|| failure("File change path is invalid"))?;
        change.path = target(&resolve_path(directory, text))?;
        if let Some(previous) = baseline.iter().find(|p| p.path == change.path) {
            if previous.before != change.before {
                return Err(failure(
                    "A file hook cannot replace the original conflict baseline",
                ));
            }
            change.permissions = previous.permissions.clone();
        } else {
            change.before = current(&change.path)?;
            change.permissions = std::fs::metadata(&change.path)
                .ok()
                .map(|m| m.permissions());
        }
        if change
            .before
            .iter()
            .chain(&change.after)
            .any(|v| v.contains(&0))
        {
            return Err(failure("File change contains binary content"));
        }
    }
    if matches!(kind, Kind::Write | Kind::Edit) {
        if changes.len() != 1 || changes[0].after.is_none() {
            return Err(failure(
                "Write/edit hooks must return one file with after content",
            ));
        }
        if kind == Kind::Edit && changes[0].before.as_ref().is_none_or(Vec::is_empty) {
            return Err(failure("Edit target must be an existing nonempty file"));
        }
    }
    validate_changes(&changes, cancel)?;
    Ok(changes)
}

fn validate_changes(changes: &[Change], cancel: &CancellationToken) -> Result<(), OperationError> {
    if changes.is_empty() {
        return Err(failure("File change batch must not be empty"));
    }
    let mut paths = HashSet::new();
    for change in changes {
        if cancel.is_cancelled() {
            return Err(failure("File change cancelled before writing"));
        }
        if !change.path.is_absolute()
            || target(&change.path)? != change.path
            || !paths.insert(&change.path)
        {
            return Err(failure("File change has a repeated or changed path"));
        }
        if change.before.is_none() && change.after.is_none() {
            return Err(failure("File change needs before or after content"));
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

impl Plan {
    pub(super) fn paths(&self) -> impl Iterator<Item = &Path> {
        self.changes.iter().map(|change| change.path.as_path())
    }
    pub fn commit(&self, cancel: &CancellationToken) -> Result<ToolOutput, OperationError> {
        validate_changes(&self.changes, cancel)?;
        for (index, change) in self.changes.iter().enumerate() {
            if let Err(error) = patch::commit(change, false) {
                let rollback = self.changes[..=index]
                    .iter()
                    .rev()
                    .filter_map(|c| patch::commit(c, true).err())
                    .collect::<Vec<_>>();
                return Err(failure(format!(
                    "File change failed: {error}; rollback {}",
                    if rollback.is_empty() {
                        "completed".into()
                    } else {
                        rollback.join("; ")
                    }
                )));
            }
        }
        Ok(self.output.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn plan(changes: Value) -> Plan {
        Plan {
            changes: serde_json::from_value(changes).unwrap(),
            output: ToolOutput {
                title: String::new(),
                output: String::new(),
                metadata: json!({}),
            },
        }
    }
    #[test]
    fn conflict_in_later_file_prevents_all_writes() {
        let root = tempfile::tempdir().unwrap();
        let a = root.path().join("a");
        let b = root.path().join("b");
        std::fs::write(&a, "one").unwrap();
        std::fs::write(&b, "two").unwrap();
        let plan = plan(
            json!([{"path":a,"before":"one","after":"changed"},{"path":b,"before":"two","after":"changed"}]),
        );
        std::fs::write(&b, "external edit").unwrap();
        assert!(plan
            .commit(&CancellationToken::new())
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
        plan(json!([{"path":path,"before":null,"after":"no final newline"}]))
            .commit(&CancellationToken::new())
            .unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "no final newline");
        assert!(plan(json!([{"path":path,"before":"no final newline","after":"one"},{"path":path,"before":"no final newline","after":null}])).commit(&CancellationToken::new()).is_err());
    }
    fn hooks(directory: &str, script: &str) -> std::sync::Arc<crate::hooks::Hooks> {
        let cfg:crate::config::Config=serde_json::from_value(json!({"ocmini":{"hooks":{"handlers":[{"id":"file-hook","on":"file.change.before","command":["/bin/sh","-c",script]}]}}})).unwrap();
        crate::hooks::Hooks::for_config(&cfg, directory).unwrap()
    }
    #[tokio::test]
    #[cfg(unix)]
    async fn hook_redirects_path_and_content_before_any_effect() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().to_str().unwrap();
        let hooks = hooks(
            dir,
            r#"cat >/dev/null; printf '%s' '{"set":{"changes":[{"path":"redirected","before":null,"after":"hook content"}]}}'"#,
        );
        let (_, plan) = prepare_values(
            dir,
            &hooks,
            json!({"directory":dir}),
            CancellationToken::new(),
            "write",
            json!({"filePath":"original","content":"model content"}),
        )
        .await
        .unwrap();
        assert_eq!(
            plan.paths().map(Path::to_path_buf).collect::<Vec<_>>(),
            vec![root.path().join("redirected")]
        );
        assert!(!root.path().join("redirected").exists());
        plan.commit(&CancellationToken::new()).unwrap();
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
        let dir = root.path().to_str().unwrap();
        std::fs::write(root.path().join("existing"), "original\n").unwrap();
        let hooks = hooks(
            dir,
            r#"cat >/dev/null; printf '%s' '{"decision":"block","reason":"batch rejected"}'"#,
        );
        let result=prepare_values(dir,&hooks,json!({"directory":dir}),CancellationToken::new(),"apply_patch",json!({"patchText":"*** Begin Patch\n*** Add File: added\n+new\n*** Update File: existing\n@@\n-original\n+changed\n*** End Patch"})).await;
        assert!(result.err().unwrap().message.contains("batch rejected"));
        assert!(!root.path().join("added").exists());
        assert_eq!(
            std::fs::read_to_string(root.path().join("existing")).unwrap(),
            "original\n"
        );
    }
    #[tokio::test]
    #[cfg(unix)]
    async fn no_op_hook_preserves_edit_arguments_and_result() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().to_str().unwrap();
        let path = root.path().join("file");
        std::fs::write(path, "one one").unwrap();
        let input =
            json!({"filePath":"file","oldString":"one","newString":"two","replaceAll":true});
        let plain = prepare_sync(dir, "edit", &input, &CancellationToken::new()).unwrap();
        let hooks = hooks(dir, "cat >/dev/null; printf '%s' '{}'");
        let (args, hooked) = prepare_values(
            dir,
            &hooks,
            json!({"directory":dir}),
            CancellationToken::new(),
            "edit",
            input.clone(),
        )
        .await
        .unwrap();
        assert_eq!(args, input);
        assert_eq!(hooked.output.metadata, plain.output.metadata);
        assert_eq!(hooked.output.output, plain.output.output);
        assert_eq!(hooked.output.title, plain.output.title);
    }
}
