//! One file transaction for ordinary tools and hook-transformed tools.
use super::{decode, failure, files, patch, resolve_path};
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
pub struct WriteInput {
    pub file_path: String,
    pub content: String,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct EditInput {
    pub file_path: String,
    #[schemars(length(min = 1))]
    pub old_string: String,
    pub new_string: String,
    #[serde(default)]
    pub replace_all: bool,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PatchInput {
    pub patch_text: String,
}

enum Input {
    Write(WriteInput),
    Edit(EditInput),
    Patch(Vec<patch::Operation>),
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
            Some(super::Builtin::ApplyPatch) => {
                let input: PatchInput = decode(&super::alias(input, "patchText", "patch"))?;
                Ok(Self::Patch(patch::parse(&input.patch_text)?))
            }
            _ => Err(failure(format!("Unknown file tool {name}"))),
        }
    }
    fn targets(&self, directory: &str) -> Result<Targets, OperationError> {
        let paths = match self {
            Self::Write(input) => vec![input.file_path.as_str()],
            Self::Edit(input) => vec![input.file_path.as_str()],
            Self::Patch(operations) => operations
                .iter()
                .flat_map(patch::Operation::paths)
                .collect(),
        };
        paths
            .into_iter()
            .map(|path| {
                let path = resolve_path(directory, path);
                Ok((path.clone(), target(&path)?))
            })
            .collect()
    }

    fn kind(&self) -> Kind {
        match self {
            Self::Write(_) => Kind::Write,
            Self::Edit(_) => Kind::Edit,
            Self::Patch(_) => Kind::Patch,
        }
    }
}

type Targets = Vec<(PathBuf, PathBuf)>;

fn checked_target(path: &Path, approved: &Path) -> Result<PathBuf, OperationError> {
    if target(path)? != approved {
        return Err(failure(format!(
            "File path changed since approval: {}",
            path.display()
        )));
    }
    Ok(approved.to_owned())
}

pub struct Plan {
    pub changes: Vec<Change>,
    output: ToolOutput,
    edit_lock: Mutex<Option<OwnedMutexGuard<()>>>,
}
pub fn target(path: &Path) -> Result<PathBuf, OperationError> {
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

pub fn prepare_sync(
    directory: &str,
    name: &str,
    input: &Value,
    cancel: &CancellationToken,
) -> Result<Plan, OperationError> {
    let input = Input::parse(name, input)?;
    let targets = input.targets(directory)?;
    prepare_plan(directory, input, cancel, &targets)
}

fn prepare_plan(
    directory: &str,
    input: Input,
    cancel: &CancellationToken,
    targets: &Targets,
) -> Result<Plan, OperationError> {
    let approved = |path: &Path| {
        let (_, canonical) = targets
            .iter()
            .find(|(raw, _)| raw == path)
            .ok_or_else(|| failure("File path was not approved"))?;
        checked_target(path, canonical)
    };
    let (kind, path, canonical, before, after, count) = match input {
        Input::Write(input) => {
            let path = resolve_path(directory, &input.file_path);
            let canonical = approved(&path)?;
            let before = current(&canonical)?;
            (
                Kind::Write,
                path,
                canonical,
                before,
                input.content.as_bytes().to_vec(),
                0,
            )
        }
        Input::Edit(input) => {
            let path = resolve_path(directory, &input.file_path);
            let canonical = approved(&path)?;
            let (before, after, count) = files::prepare_edit(&canonical, &input)?;
            (
                Kind::Edit,
                path,
                canonical,
                Some(before.into_bytes()),
                after.into_bytes(),
                count,
            )
        }
        Input::Patch(operations) => {
            let (changes, summaries) = patch::prepare(directory, operations, cancel, approved)?;
            return Ok(Plan {
                changes,
                output: patch::output(&summaries),
                edit_lock: Mutex::new(None),
            });
        }
    };
    let diff = (kind == Kind::Edit).then(|| {
        let before_str = before
            .as_deref()
            .and_then(|b| std::str::from_utf8(b).ok())
            .unwrap_or_default();
        let after_str = std::str::from_utf8(&after).unwrap_or_default();
        unified_diff(&path, before_str, after_str)
    });
    let output = file_output(kind, &path, after.len(), count, diff);
    let permissions = std::fs::metadata(&canonical).ok().map(|m| m.permissions());
    Ok(Plan {
        changes: vec![Change {
            path: canonical,
            before,
            after: Some(after),
            permissions,
        }],
        output,
        edit_lock: Mutex::new(None),
    })
}

fn file_output(
    kind: Kind,
    path: &Path,
    bytes: usize,
    count: usize,
    diff: Option<String>,
) -> ToolOutput {
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
            json!({"filepath":path,"replacements":count,"diff":diff.unwrap_or_default()})
        },
    }
}

#[derive(Clone, Copy)]
enum DiffOp {
    Equal(usize),
    Delete(usize),
    Insert(usize),
}

/// Produce a standard unified diff with 3 lines of context, without external crates.
fn unified_diff(path: &Path, before: &str, after: &str) -> String {
    if before == after {
        return String::new();
    }
    let old_lines: Vec<&str> = before.lines().collect();
    let new_lines: Vec<&str> = after.lines().collect();

    let mut prefix = 0;
    while prefix < old_lines.len()
        && prefix < new_lines.len()
        && old_lines[prefix] == new_lines[prefix]
    {
        prefix += 1;
    }
    let mut suffix = 0;
    while suffix < old_lines.len() - prefix
        && suffix < new_lines.len() - prefix
        && old_lines[old_lines.len() - 1 - suffix] == new_lines[new_lines.len() - 1 - suffix]
    {
        suffix += 1;
    }

    let old_mid = &old_lines[prefix..old_lines.len() - suffix];
    let new_mid = &new_lines[prefix..new_lines.len() - suffix];

    let mut ops = Vec::with_capacity(old_lines.len() + new_lines.len());
    for i in 0..prefix {
        ops.push(DiffOp::Equal(i));
    }
    ops.extend(diff_middle(old_mid, new_mid, prefix));
    let old_tail = old_lines.len() - suffix;
    for i in 0..suffix {
        ops.push(DiffOp::Equal(old_tail + i));
    }

    let mut change_indices: Vec<usize> = ops
        .iter()
        .enumerate()
        .filter_map(|(idx, op)| (!matches!(op, DiffOp::Equal(_))).then_some(idx))
        .collect();
    if change_indices.is_empty() {
        // Only line-ending or trailing-newline formatting changed.
        if let Some(last) = ops.len().checked_sub(1) {
            change_indices.push(last);
        } else {
            return String::new();
        }
    }

    const CONTEXT: usize = 3;
    let mut out = format!("--- a/{0}\n+++ b/{0}\n", path.display());
    let mut group_start = 0;
    while group_start < change_indices.len() {
        let mut group_end = group_start;
        while group_end + 1 < change_indices.len()
            && change_indices[group_end + 1] <= change_indices[group_end] + CONTEXT * 2 + 1
        {
            group_end += 1;
        }
        let start = change_indices[group_start].saturating_sub(CONTEXT);
        let end = (change_indices[group_end] + CONTEXT + 1).min(ops.len());

        let mut old_pos = 0usize;
        let mut new_pos = 0usize;
        for op in &ops[..start] {
            match op {
                DiffOp::Equal(_) => {
                    old_pos += 1;
                    new_pos += 1;
                }
                DiffOp::Delete(_) => old_pos += 1,
                DiffOp::Insert(_) => new_pos += 1,
            }
        }
        let mut old_count = 0usize;
        let mut new_count = 0usize;
        for op in &ops[start..end] {
            match op {
                DiffOp::Equal(_) => {
                    old_count += 1;
                    new_count += 1;
                }
                DiffOp::Delete(_) => old_count += 1,
                DiffOp::Insert(_) => new_count += 1,
            }
        }
        let old_start = if old_count == 0 { old_pos } else { old_pos + 1 };
        let new_start = if new_count == 0 { new_pos } else { new_pos + 1 };
        out.push_str(&format!(
            "@@ -{old_start},{old_count} +{new_start},{new_count} @@\n"
        ));
        for op in &ops[start..end] {
            match *op {
                DiffOp::Equal(o) => {
                    out.push(' ');
                    out.push_str(old_lines[o]);
                    out.push('\n');
                }
                DiffOp::Delete(o) => {
                    out.push('-');
                    out.push_str(old_lines[o]);
                    out.push('\n');
                }
                DiffOp::Insert(n) => {
                    out.push('+');
                    out.push_str(new_lines[n]);
                    out.push('\n');
                }
            }
        }
        group_start = group_end + 1;
    }
    out
}

fn diff_middle(old_mid: &[&str], new_mid: &[&str], base: usize) -> Vec<DiffOp> {
    let n = old_mid.len();
    let m = new_mid.len();
    if n == 0 {
        return (0..m).map(|j| DiffOp::Insert(base + j)).collect();
    }
    if m == 0 {
        return (0..n).map(|i| DiffOp::Delete(base + i)).collect();
    }
    const MAX_D: usize = 512;
    let max_d = (n + m).min(MAX_D);
    let offset = max_d as isize;
    let mut v = vec![0isize; 2 * max_d + 1];
    let mut trace: Vec<Vec<isize>> = Vec::new();
    let mut found = false;

    'outer: for d in 0..=max_d {
        let d_i = d as isize;
        let mut k = -d_i;
        while k <= d_i {
            let idx = (k + offset) as usize;
            let mut x = if k == -d_i
                || (k != d_i && v[(k - 1 + offset) as usize] < v[(k + 1 + offset) as usize])
            {
                v[(k + 1 + offset) as usize]
            } else {
                v[(k - 1 + offset) as usize] + 1
            };
            let mut y = x - k;
            while (x as usize) < n && (y as usize) < m && old_mid[x as usize] == new_mid[y as usize]
            {
                x += 1;
                y += 1;
            }
            v[idx] = x;
            if x as usize >= n && y as usize >= m {
                trace.push(v.clone());
                found = true;
                break 'outer;
            }
            k += 2;
        }
        trace.push(v.clone());
    }

    if !found {
        let mut fallback = Vec::with_capacity(n + m);
        for i in 0..n {
            fallback.push(DiffOp::Delete(base + i));
        }
        for j in 0..m {
            fallback.push(DiffOp::Insert(base + j));
        }
        return fallback;
    }

    let mut ops = Vec::with_capacity(n + m);
    let mut x = n as isize;
    let mut y = m as isize;
    for d in (1..trace.len()).rev() {
        let v_prev = &trace[d - 1];
        let d_i = d as isize;
        let k = x - y;
        let prev_k = if k == -d_i
            || (k != d_i && v_prev[(k - 1 + offset) as usize] < v_prev[(k + 1 + offset) as usize])
        {
            k + 1
        } else {
            k - 1
        };
        let prev_x = v_prev[(prev_k + offset) as usize];
        let prev_y = prev_x - prev_k;
        while x > prev_x && y > prev_y {
            x -= 1;
            y -= 1;
            ops.push(DiffOp::Equal(base + x as usize));
        }
        if x == prev_x {
            y -= 1;
            ops.push(DiffOp::Insert(base + y as usize));
        } else {
            x -= 1;
            ops.push(DiffOp::Delete(base + x as usize));
        }
    }
    while x > 0 && y > 0 {
        x -= 1;
        y -= 1;
        ops.push(DiffOp::Equal(base + x as usize));
    }
    ops.reverse();
    ops
}

fn transformed_changes(
    directory: &str,
    kind: Kind,
    wire: &Value,
    baseline: &[Change],
) -> Result<(Vec<Change>, Targets), OperationError> {
    let mut changes: Vec<Change> = serde_json::from_value(wire.clone())
        .map_err(|e| failure(format!("Invalid file changes: {e}")))?;
    if changes.is_empty() {
        return Err(failure("File change batch must not be empty"));
    }
    let mut targets = Vec::new();
    let mut seen = HashSet::new();
    for change in &mut changes {
        let text = change
            .path
            .to_str()
            .filter(|s| !s.is_empty() && !s.contains(['\0', '\n', '\r']))
            .ok_or_else(|| failure("File change path is invalid"))?;
        let path = resolve_path(directory, text);
        change.path = target(&path)?;
        if !seen.insert(change.path.clone()) {
            return Err(failure("File change has a repeated or changed path"));
        }
        targets.push((path, change.path.clone()));
        if let Some(previous) = baseline.iter().find(|p| p.path == change.path) {
            if previous.before != change.before {
                return Err(failure(
                    "A file hook cannot replace the original conflict baseline",
                ));
            }
            change.permissions = previous.permissions.clone();
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
    if matches!(kind, Kind::Write | Kind::Edit)
        && (changes.len() != 1 || changes[0].after.is_none())
    {
        return Err(failure(
            "Write/edit hooks must return one file with after content",
        ));
    }
    Ok((changes, targets))
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
    pub fn paths(&self) -> impl Iterator<Item = &Path> {
        self.changes.iter().map(|change| change.path.as_path())
    }
    pub async fn run(
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

    pub fn commit(&self, cancel: &CancellationToken) -> Result<ToolOutput, OperationError> {
        let edit_lock = self.edit_lock.lock().unwrap().take();
        if edit_lock.is_some() {
            self.commit_locked(cancel)
        } else {
            with_path_locks(self.paths(), || self.commit_locked(cancel))
        }
    }
}

pub struct Reservation {
    input: Input,
    pub targets: Targets,
    guard: Option<OwnedMutexGuard<()>>,
}
pub async fn reserve(
    directory: &str,
    name: &str,
    args: &Value,
    cancel: &CancellationToken,
) -> Result<Reservation, OperationError> {
    let input = Input::parse(name, args)?;
    // Resolve targets before the first `.await` so chained edits to the same
    // file in `join_all` acquire `lock_edit` strictly in model-call order.
    let targets = input.targets(directory)?;
    let guard = if input.kind() == Kind::Edit {
        Some(lock_edit(&targets[0].1, cancel).await?)
    } else {
        None
    };
    Ok(Reservation {
        input,
        targets,
        guard,
    })
}
impl Reservation {
    pub async fn prepare(
        self,
        directory: &str,
        cancel: CancellationToken,
    ) -> Result<Plan, OperationError> {
        let Self {
            input,
            targets,
            guard,
        } = self;
        let dir = directory.to_owned();
        let mut plan =
            tokio::task::spawn_blocking(move || prepare_plan(&dir, input, &cancel, &targets))
                .await
                .map_err(|e| failure(e.to_string()))??;
        *plan.edit_lock.get_mut().unwrap() = guard;
        Ok(plan)
    }
}
pub async fn prepare_plain(
    directory: &str,
    name: &str,
    args: &Value,
    cancel: CancellationToken,
) -> Result<Plan, OperationError> {
    reserve(directory, name, args, &cancel)
        .await?
        .prepare(directory, cancel)
        .await
}
impl Plan {
    pub fn transform_targets(
        &self,
        directory: &str,
        name: &str,
        wire: &Value,
    ) -> Result<Value, OperationError> {
        let kind = match name {
            "write" => Kind::Write,
            "edit" => Kind::Edit,
            _ => Kind::Patch,
        };
        let (changes, targets) = transformed_changes(directory, kind, wire, &self.changes)?;
        Ok(json!({"changes":changes,"targets":targets}))
    }
    pub fn wire_changes(&self) -> Result<Value, OperationError> {
        serde_json::to_value(&self.changes).map_err(|e| failure(e.to_string()))
    }
    pub async fn transform(
        &mut self,
        directory: &str,
        name: &str,
        wire: &Value,
        cancel: CancellationToken,
    ) -> Result<(), OperationError> {
        let kind = match name {
            "write" => Kind::Write,
            "edit" => Kind::Edit,
            _ => Kind::Patch,
        };
        let dir = directory.to_owned();
        let wire = wire.clone();
        let existing_changes = self.changes.clone();
        let (mut changes, _) = tokio::task::spawn_blocking(move || {
            transformed_changes(&dir, kind, &wire, &existing_changes)
        })
        .await
        .map_err(|e| failure(e.to_string()))??;
        if kind == Kind::Edit && changes[0].path != self.changes[0].path {
            drop(self.edit_lock.get_mut().unwrap().take());
            *self.edit_lock.get_mut().unwrap() = Some(lock_edit(&changes[0].path, &cancel).await?);
        }
        let original_paths: HashSet<_> = self.paths().map(Path::to_owned).collect();
        let cancel_blocking = cancel.clone();
        let changes = tokio::task::spawn_blocking(move || {
            for change in &mut changes {
                if !original_paths.contains(&change.path) {
                    change.before = current(&change.path)?;
                    change.permissions = std::fs::metadata(&change.path)
                        .ok()
                        .map(|m| m.permissions());
                }
            }
            validate_changes(&changes, &cancel_blocking)?;
            Ok::<_, OperationError>(changes)
        })
        .await
        .map_err(|e| failure(e.to_string()))??;
        if kind != Kind::Patch {
            let c = &changes[0];
            let after = c.after.as_ref().unwrap();
            let diff = (kind == Kind::Edit).then(|| {
                let before_str = c
                    .before
                    .as_deref()
                    .and_then(|b| std::str::from_utf8(b).ok())
                    .unwrap_or_default();
                let after_str = std::str::from_utf8(after).unwrap_or_default();
                unified_diff(&c.path, before_str, after_str)
            });
            self.output = file_output(kind, &c.path, after.len(), 1, diff);
        } else {
            let root = Path::new(directory)
                .canonicalize()
                .unwrap_or_else(|_| PathBuf::from(directory));
            let summaries: Vec<Value> = changes
                .iter()
                .map(|c| {
                    let display = c
                        .path
                        .strip_prefix(&root)
                        .unwrap_or(&c.path)
                        .to_string_lossy()
                        .into_owned();
                    let kind = match (&c.before, &c.after) {
                        (None, Some(_)) => "add",
                        (Some(_), None) => "delete",
                        _ => "update",
                    };
                    let additions = c
                        .after
                        .as_deref()
                        .map(|b| String::from_utf8_lossy(b).lines().count())
                        .unwrap_or(0);
                    let deletions = c
                        .before
                        .as_deref()
                        .map(|b| String::from_utf8_lossy(b).lines().count())
                        .unwrap_or(0);
                    json!({"path":display,"type":kind,"additions":additions,"deletions":deletions})
                })
                .collect();
            self.output = patch::output(&summaries);
        }
        self.changes = changes;
        Ok(())
    }
}
pub fn targets(
    directory: &str,
    name: &str,
    args: &Value,
) -> Result<Vec<(PathBuf, PathBuf)>, OperationError> {
    Input::parse(name, args)?.targets(directory)
}

/// Brain-side coordination uses the same path locks as direct tools. The
/// runner owns contents and effects; only canonical path identities cross here.
pub async fn coordinate_edit(
    path: &Path,
    cancel: &CancellationToken,
) -> Result<OwnedMutexGuard<()>, OperationError> {
    lock_edit(path, cancel).await
}
pub async fn coordinate_commit(
    paths: &[PathBuf],
    cancel: &CancellationToken,
) -> Result<Vec<OwnedMutexGuard<()>>, OperationError> {
    let mut guards = Vec::new();
    for lock in path_locks(paths.iter().cloned()) {
        guards.push(tokio::select!{g=lock.lock_owned()=>g,_=cancel.cancelled()=>return Err(failure("Tool cancelled"))});
    }
    Ok(guards)
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
        let root = crate::test_tempdir().unwrap();
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
        let root = crate::test_tempdir().unwrap();
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
        let root = crate::test_tempdir().unwrap();
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
        let root = crate::test_tempdir().unwrap();
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
        let root = crate::test_tempdir().unwrap();
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
        let root = crate::test_tempdir().unwrap();
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
        let root = crate::test_tempdir().unwrap();
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
        let root = crate::test_tempdir().unwrap();
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

    #[test]
    fn edit_emits_unified_diff_in_metadata() {
        let root = crate::test_tempdir().unwrap();
        let file = root.path().join("sample.txt");
        std::fs::write(
            &file,
            "line1\nline2\nline3\nold_a\nline5\nline6\nline7\nline8\nline9\nline10\nline11\nold_b\nline13\n",
        )
        .unwrap();
        let cancel = CancellationToken::new();
        let result = prepare_sync(
            root.path().to_str().unwrap(),
            "edit",
            &json!({"filePath": "sample.txt", "oldString": "old_a", "newString": "new_a\nextra_a"}),
            &cancel,
        )
        .unwrap()
        .commit(&cancel)
        .unwrap();
        let diff = result.metadata["diff"].as_str().unwrap();
        assert!(diff.contains("--- a/"), "{diff}");
        assert!(diff.contains("+++ b/"), "{diff}");
        assert!(diff.contains("@@ -1,7 +1,8 @@"), "{diff}");
        assert!(diff.contains("-old_a\n+new_a\n+extra_a\n"), "{diff}");
        assert!(diff.contains(" line1\n"), "{diff}");
        assert!(!diff.contains("line10"), "{diff}");

        // Multi-hunk replaceAll emits separate hunks for distant changes.
        let file2 = root.path().join("multi.txt");
        let mut content = String::from("target\n");
        for i in 0..12 {
            content.push_str(&format!("mid{i}\n"));
        }
        content.push_str("target\n");
        std::fs::write(&file2, content).unwrap();
        let result2 = prepare_sync(
            root.path().to_str().unwrap(),
            "edit",
            &json!({"filePath": "multi.txt", "oldString": "target", "newString": "replaced", "replaceAll": true}),
            &cancel,
        )
        .unwrap()
        .commit(&cancel)
        .unwrap();
        let diff2 = result2.metadata["diff"].as_str().unwrap();
        assert_eq!(diff2.matches("@@ ").count(), 2, "{diff2}");
        assert!(diff2.contains("-target\n+replaced\n"), "{diff2}");
    }
}
