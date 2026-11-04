//! One file transaction for ordinary tools and hook-transformed tools.
use super::{decode, failure, files, patch, resolve_path, ToolCtx};
use crate::types::{OperationError, ToolOutput};
use patch::Change;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, Weak};
use tokio::sync::{Mutex as PathMutex, OwnedMutexGuard};
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
    edit_lock: Mutex<Option<OwnedMutexGuard<()>>>,
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
                edit_lock: Mutex::new(None),
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
        edit_lock: Mutex::new(None),
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
    // Like upstream, an edit reads and prepares its replacement only after
    // earlier edits to the same file have finished. Keep the lock through
    // hooks, permission approval and commit, including across sessions.
    let edit_path = match &input {
        Input::Edit(input) => Some(target(&resolve_path(directory, &input.file_path))?),
        _ => None,
    };
    let edit_lock = match &edit_path {
        Some(path) => Some(lock_edit(path, &cancel).await?),
        None => None,
    };
    let work_dir = directory.to_owned();
    let work_cancel = cancel.clone();
    let mut plan =
        tokio::task::spawn_blocking(move || prepare_plan(&work_dir, &input, &work_cancel))
            .await
            .map_err(|e| failure(e.to_string()))??;
    if edit_path.is_some_and(|path| path != plan.changes[0].path) {
        return Err(failure("File change has a repeated or changed path"));
    }
    *plan.edit_lock.get_mut().unwrap() = edit_lock;
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
    if kind == Kind::Edit && changes[0].path != plan.changes[0].path {
        // A hook may redirect an edit. Release its original target before
        // waiting on the new one so crossed redirects cannot deadlock.
        drop(plan.edit_lock.get_mut().unwrap().take());
        *plan.edit_lock.get_mut().unwrap() = Some(lock_edit(&changes[0].path, &cancel).await?);
        validate_changes(&changes, &cancel)?;
    }
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

/// A commit validates its baselines and then writes, so an unprotected commit
/// is a check-then-write window. Tool calls from one model step run
/// concurrently, and two batches prepared from the same file would both pass
/// validation, letting the later write silently discard the earlier change.
/// Commits take their paths in sorted order, which serializes overlapping
/// batches (including moves, whose source and destination are separate
/// changes) without deadlocking.
static PATH_LOCKS: LazyLock<Mutex<HashMap<PathBuf, Weak<PathMutex<()>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn path_locks(paths: impl IntoIterator<Item = impl Into<PathBuf>>) -> Vec<Arc<PathMutex<()>>> {
    let mut keys: Vec<PathBuf> = paths.into_iter().map(Into::into).collect();
    keys.sort_unstable();
    keys.dedup();
    {
        let mut registry = PATH_LOCKS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // Keep one identity for holders and waiters, without retaining every
        // file ever edited by a long-running server.
        registry.retain(|_, lock| lock.strong_count() > 0);
        keys.into_iter()
            .map(|key| {
                let entry = registry.entry(key).or_default();
                let lock = entry.upgrade().unwrap_or_default();
                *entry = Arc::downgrade(&lock);
                lock
            })
            .collect::<Vec<_>>()
    }
}

async fn lock_edit(
    path: &Path,
    cancel: &CancellationToken,
) -> Result<OwnedMutexGuard<()>, OperationError> {
    let lock = path_locks([path]).pop().expect("one edit target");
    tokio::select! {
        guard = lock.lock_owned() => Ok(guard),
        _ = cancel.cancelled() => Err(failure("Tool cancelled")),
    }
}

#[cfg(test)]
fn with_path_locks<T>(
    paths: impl IntoIterator<Item = impl Into<PathBuf>>,
    write: impl FnOnce() -> Result<T, OperationError>,
) -> Result<T, OperationError> {
    let locks = path_locks(paths);
    let _guards = locks
        .iter()
        .map(|lock| futures::executor::block_on(lock.lock()))
        .collect::<Vec<_>>();
    write()
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
    pub(super) async fn run(
        self: Arc<Self>,
        cancel: CancellationToken,
    ) -> Result<ToolOutput, OperationError> {
        let mut guards = Vec::new();
        let edit_lock = self.edit_lock.lock().unwrap().take();
        if edit_lock.is_none() {
            // Wait outside the blocking pool: another edit may be awaiting
            // permission for this path. Cancellation must also end this wait.
            for lock in path_locks(self.paths()) {
                guards.push(tokio::select! {
                    guard = lock.lock_owned() => guard,
                    _ = cancel.cancelled() => return Err(failure("Tool cancelled")),
                });
            }
        }
        tokio::task::spawn_blocking(move || {
            let _edit_lock = edit_lock;
            let _guards = guards;
            self.commit_locked(&cancel)
        })
        .await
        .map_err(|e| failure(e.to_string()))?
    }

    fn commit_locked(&self, cancel: &CancellationToken) -> Result<ToolOutput, OperationError> {
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

    #[cfg(test)]
    pub fn commit(&self, cancel: &CancellationToken) -> Result<ToolOutput, OperationError> {
        let edit_lock = self.edit_lock.lock().unwrap().take();
        if edit_lock.is_some() {
            self.commit_locked(cancel)
        } else {
            with_path_locks(self.paths(), || self.commit_locked(cancel))
        }
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
            edit_lock: Mutex::new(None),
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
    #[test]
    fn concurrent_commits_to_one_path_never_lose_a_change() {
        // Tool calls admitted in one model step commit on separate blocking
        // threads. Overlapping batches must serialize so exactly one plan
        // commits and the file keeps the winning plan's content.
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("file");
        for round in 0..32 {
            std::fs::write(&path, "base").unwrap();
            let start = Arc::new(std::sync::Barrier::new(2));
            let commit = |after: &'static str| {
                let path = path.clone();
                let start = start.clone();
                std::thread::spawn(move || {
                    let plan = plan(json!([{"path":path,"before":"base","after":after}]));
                    start.wait();
                    plan.commit(&CancellationToken::new())
                })
            };
            let (a, b) = (commit("A"), commit("B"));
            let (a, b) = (a.join().unwrap(), b.join().unwrap());
            assert_ne!(
                a.is_ok(),
                b.is_ok(),
                "exactly one commit may win round {round}"
            );
            assert_eq!(
                std::fs::read_to_string(&path).unwrap(),
                if a.is_ok() { "A" } else { "B" },
                "round {round} must keep the winning content"
            );
            assert!(a
                .err()
                .or_else(|| b.err())
                .unwrap()
                .message
                .contains("changed since preparation"));
        }
    }
    #[test]
    fn commit_validates_after_a_concurrent_writer_releases_the_path() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("file");
        std::fs::write(&path, "base").unwrap();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (completed_tx, completed_rx) = std::sync::mpsc::channel();
        let (waiter, blocked) = with_path_locks([path.clone()], || {
            let path = path.clone();
            let waiting_path = path.clone();
            let waiter = std::thread::spawn(move || {
                let plan = plan(json!([{"path":waiting_path,"before":"base","after":"mine"}]));
                started_tx.send(()).unwrap();
                let result = plan.commit(&CancellationToken::new());
                completed_tx.send(()).unwrap();
                result
            });
            started_rx.recv().unwrap();
            // Keep the old baseline until the waiter has had a chance to
            // commit: bypassing the lock would now succeed, so this test also
            // detects validation that runs before acquiring the lock.
            let blocked = matches!(
                completed_rx.recv_timeout(std::time::Duration::from_millis(100)),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout)
            );
            std::fs::write(&path, "theirs").unwrap();
            Ok((waiter, blocked))
        })
        .unwrap();
        let result = waiter.join().unwrap();
        assert!(
            blocked,
            "commit must wait while another writer owns the path"
        );
        let error = result.expect_err("a committed change must force a fresh baseline check");
        assert!(error.message.contains("changed since preparation"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "theirs");
    }
    #[test]
    fn overlapping_batches_in_reverse_order_keep_the_winners_whole_batch() {
        let root = tempfile::tempdir().unwrap();
        let a = root.path().join("a");
        let b = root.path().join("b");
        std::fs::write(&a, "base").unwrap();
        std::fs::write(&b, "base").unwrap();
        let start = Arc::new(std::sync::Barrier::new(2));
        let commit = |changes| {
            let start = start.clone();
            std::thread::spawn(move || {
                let plan = plan(changes);
                start.wait();
                plan.commit(&CancellationToken::new())
            })
        };
        let first = commit(
            json!([{"path":a,"before":"base","after":"A"},{"path":b,"before":"base","after":"A"}]),
        );
        let second = commit(
            json!([{"path":b,"before":"base","after":"B"},{"path":a,"before":"base","after":"B"}]),
        );
        let (first, second) = (first.join().unwrap(), second.join().unwrap());
        assert_ne!(first.is_ok(), second.is_ok());
        let winner = if first.is_ok() { "A" } else { "B" };
        assert_eq!(std::fs::read_to_string(a).unwrap(), winner);
        assert_eq!(std::fs::read_to_string(b).unwrap(), winner);
    }
    #[test]
    fn unrelated_paths_do_not_wait_and_unused_locks_are_reclaimed() {
        let root = tempfile::tempdir().unwrap();
        let held = root.path().join("held");
        let unrelated = root.path().join("unrelated");
        with_path_locks([held.clone()], || {
            let (done_tx, done_rx) = std::sync::mpsc::channel();
            let waiter = std::thread::spawn(move || {
                let result = plan(json!([{"path":unrelated,"before":null,"after":"new"}]))
                    .commit(&CancellationToken::new());
                done_tx.send(result).unwrap();
            });
            done_rx
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap()
                .unwrap();
            waiter.join().unwrap();
            assert!(PATH_LOCKS
                .lock()
                .unwrap()
                .get(&held)
                .unwrap()
                .upgrade()
                .is_some());
            Ok(())
        })
        .unwrap();
        assert!(PATH_LOCKS
            .lock()
            .unwrap()
            .get(&held)
            .is_none_or(|lock| lock.upgrade().is_none()));
        with_path_locks([root.path().join("next")], || Ok(())).unwrap();
        assert!(!PATH_LOCKS.lock().unwrap().contains_key(&held));
    }
    #[test]
    fn patch_moves_conflict_with_edits_to_either_endpoint() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        let destination = root.path().join("destination");
        let cancel = CancellationToken::new();
        for endpoint in [&source, &destination] {
            std::fs::write(&source, "base\n").unwrap();
            let moved = prepare_sync(root.path().to_str().unwrap(), "apply_patch", &json!({"patchText":"*** Begin Patch\n*** Update File: source\n*** Move to: destination\n@@\n-base\n+moved\n*** End Patch"}), &cancel).unwrap();
            let edited = prepare_sync(
                root.path().to_str().unwrap(),
                "write",
                &json!({"filePath":endpoint,"content":"other\n"}),
                &cancel,
            )
            .unwrap();
            edited.commit(&cancel).unwrap();
            assert!(moved
                .commit(&cancel)
                .unwrap_err()
                .message
                .contains("changed since preparation"));
            assert_eq!(std::fs::read_to_string(endpoint).unwrap(), "other\n");
            if destination.exists() {
                assert_eq!(std::fs::read_to_string(&source).unwrap(), "base\n");
                std::fs::remove_file(&destination).unwrap();
            }
        }
    }
    #[cfg(unix)]
    #[test]
    fn symlink_aliases_share_the_same_conflict_baseline_and_lock() {
        let root = tempfile::tempdir().unwrap();
        let real = root.path().join("real");
        let alias = root.path().join("alias");
        std::fs::write(&real, "base").unwrap();
        std::os::unix::fs::symlink(&real, &alias).unwrap();
        let cancel = CancellationToken::new();
        let prepare = |path: &Path| {
            prepare_sync(
                root.path().to_str().unwrap(),
                "edit",
                &json!({"filePath":path,"oldString":"base","newString":"updated"}),
                &cancel,
            )
            .unwrap()
        };
        let direct = prepare(&real);
        let linked = prepare(&alias);
        assert_eq!(direct.changes[0].path, linked.changes[0].path);
        with_path_locks([direct.changes[0].path.clone()], || {
            assert!(PATH_LOCKS
                .lock()
                .unwrap()
                .get(&linked.changes[0].path)
                .unwrap()
                .upgrade()
                .is_some());
            Ok(())
        })
        .unwrap();
        direct.commit(&cancel).unwrap();
        assert!(linked
            .commit(&cancel)
            .unwrap_err()
            .message
            .contains("changed since preparation"));
        assert!(alias.symlink_metadata().unwrap().file_type().is_symlink());
        assert_eq!(std::fs::read_to_string(alias).unwrap(), "updated");
    }
    fn hooks(directory: &str, script: &str) -> std::sync::Arc<crate::hooks::Hooks> {
        let cfg:crate::config::Config=serde_json::from_value(json!({"ocmini":{"hooks":{"handlers":[{"id":"file-hook","on":"file.change.before","command":["/bin/sh","-c",script]}]}}})).unwrap();
        crate::hooks::Hooks::for_config(&cfg, directory).unwrap()
    }
    #[tokio::test]
    async fn waiting_edits_cancel_and_release_their_path_lock() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("file");
        let held = lock_edit(&path, &CancellationToken::new()).await.unwrap();
        let cancel = CancellationToken::new();
        let waiting = lock_edit(&path, &cancel);
        tokio::pin!(waiting);
        assert!(futures::poll!(&mut waiting).is_pending());
        cancel.cancel();
        assert!(waiting.await.err().unwrap().message.contains("cancelled"));
        drop(held);
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            lock_edit(&path, &CancellationToken::new()),
        )
        .await
        .unwrap()
        .unwrap();
    }
    #[tokio::test]
    async fn edit_lock_releases_after_commit_even_while_the_plan_is_retained() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().to_str().unwrap();
        let path = root.path().join("file");
        std::fs::write(&path, "before").unwrap();
        let hooks =
            crate::hooks::Hooks::for_config(&crate::config::Config::default(), dir).unwrap();
        let (_, plan) = prepare_values(
            dir,
            &hooks,
            json!({"directory":dir}),
            CancellationToken::new(),
            "edit",
            json!({"filePath":"file","oldString":"before","newString":"after"}),
        )
        .await
        .unwrap();
        let plan = Arc::new(plan);
        plan.clone().run(CancellationToken::new()).await.unwrap();
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            lock_edit(&path, &CancellationToken::new()),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(std::fs::read_to_string(path).unwrap(), "after");
        drop(plan);
    }
    #[tokio::test]
    async fn a_write_waiting_on_an_edit_transaction_can_be_cancelled() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("file");
        std::fs::write(&path, "before").unwrap();
        let _held = lock_edit(&path, &CancellationToken::new()).await.unwrap();
        let cancel = CancellationToken::new();
        let plan = prepare_sync(
            root.path().to_str().unwrap(),
            "write",
            &json!({"filePath":"file","content":"after"}),
            &cancel,
        )
        .unwrap();
        let waiting = Arc::new(plan).run(cancel.clone());
        tokio::pin!(waiting);
        assert!(futures::poll!(&mut waiting).is_pending());
        cancel.cancel();
        assert!(waiting.await.unwrap_err().message.contains("cancelled"));
        assert_eq!(std::fs::read_to_string(path).unwrap(), "before");
    }
    #[tokio::test]
    #[cfg(unix)]
    async fn hook_redirects_an_edit_and_releases_its_original_lock() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().to_str().unwrap();
        std::fs::write(root.path().join("original"), "before").unwrap();
        std::fs::write(root.path().join("redirected"), "target").unwrap();
        let hooks = hooks(
            dir,
            r#"cat >/dev/null; printf '%s' '{"set":{"changes":[{"path":"redirected","before":"target","after":"hook content"}]}}'"#,
        );
        let (args, plan) = prepare_values(
            dir,
            &hooks,
            json!({"directory":dir}),
            CancellationToken::new(),
            "edit",
            json!({"filePath":"original","oldString":"before","newString":"after"}),
        )
        .await
        .unwrap();
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            lock_edit(&root.path().join("original"), &CancellationToken::new()),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(
            args["filePath"],
            root.path().join("redirected").to_str().unwrap()
        );
        assert_eq!(args["oldString"], "target");
        assert_eq!(args["newString"], "hook content");
        plan.commit(&CancellationToken::new()).unwrap();
        assert_eq!(
            std::fs::read_to_string(root.path().join("original")).unwrap(),
            "before"
        );
        assert_eq!(
            std::fs::read_to_string(root.path().join("redirected")).unwrap(),
            "hook content"
        );
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
