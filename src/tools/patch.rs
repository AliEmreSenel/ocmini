use super::{failure, files, resolve_path};
use crate::types::{ToolError, ToolOutput};
use serde_json::json;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Debug)]
enum Operation {
    Add { path: String, lines: Vec<String> },
    Delete { path: String },
    Update { path: String, destination: Option<String>, hunks: Vec<Hunk> },
}

#[derive(Debug, Default)]
struct Hunk {
    anchor: Option<String>,
    old: Vec<String>,
    new: Vec<String>,
    eof: bool,
    additions: usize,
    deletions: usize,
}

struct Change {
    path: PathBuf,
    before: Option<Vec<u8>>,
    after: Option<Vec<u8>>,
    permissions: Option<std::fs::Permissions>,
}

pub fn affected_paths(directory: &str, patch: &str) -> Result<Vec<PathBuf>, String> {
    let operations = parse(patch)?;
    let mut paths = Vec::new();
    for operation in operations {
        match operation {
            Operation::Add { path, .. } | Operation::Delete { path } => paths.push(resolve_path(directory, &path)),
            Operation::Update { path, destination, .. } => {
                paths.push(resolve_path(directory, &path));
                if let Some(destination) = destination { paths.push(resolve_path(directory, &destination)); }
            }
        }
    }
    Ok(paths)
}

pub(super) fn apply(directory: &str, patch: &str, cancel: &AtomicBool) -> Result<ToolOutput, ToolError> {
    let operations = parse(patch).map_err(failure)?;
    let mut changes = Vec::new();
    let mut touched = HashSet::new();
    let mut summaries = Vec::new();
    for operation in operations {
        if cancel.load(Ordering::Acquire) { return Err(failure("Patch cancelled before writing")); }
        match operation {
            Operation::Add { path, lines } => {
                let path = resolve_path(directory, &path);
                unique(&mut touched, &path)?;
                if path.symlink_metadata().is_ok() { return Err(failure(format!("Cannot add existing file {}", path.display()))); }
                let text = if lines.is_empty() { String::new() } else { format!("{}\n", lines.join("\n")) };
                summaries.push(json!({"path":path,"type":"add","additions":lines.len(),"deletions":0}));
                changes.push(Change { path, before: None, after: Some(text.into_bytes()), permissions: None });
            }
            Operation::Delete { path } => {
                let path = resolve_path(directory, &path);
                unique(&mut touched, &path)?;
                let content = files::text_file(&path)?;
                if path.symlink_metadata().is_ok_and(|meta| meta.file_type().is_symlink()) { return Err(failure("Patch delete of a symbolic link is not supported; remove it explicitly with the shell tool")); }
                let permissions = std::fs::metadata(&path).ok().map(|meta| meta.permissions());
                summaries.push(json!({"path":path,"type":"delete","additions":0,"deletions":content.lines().count()}));
                changes.push(Change { path, before: Some(content.into_bytes()), after: None, permissions });
            }
            Operation::Update { path, destination, hunks } => {
                let path = resolve_path(directory, &path);
                unique(&mut touched, &path)?;
                let original = files::text_file(&path)?;
                let updated = update(&original, &hunks).map_err(|error| failure(format!("Patch failed for {}: {error}", path.display())))?;
                let permissions = std::fs::metadata(&path).ok().map(|meta| meta.permissions());
                let additions: usize = hunks.iter().map(|h| h.additions).sum();
                let deletions: usize = hunks.iter().map(|h| h.deletions).sum();
                if let Some(destination) = destination {
                    let destination = resolve_path(directory, &destination);
                    unique(&mut touched, &destination)?;
                    if destination.symlink_metadata().is_ok() { return Err(failure(format!("Move destination {} already exists", destination.display()))); }
                    if path.symlink_metadata().is_ok_and(|meta| meta.file_type().is_symlink()) { return Err(failure("Patch move of a symbolic link is not supported")); }
                    summaries.push(json!({"path":path,"movePath":destination,"type":"move","additions":additions,"deletions":deletions}));
                    changes.push(Change { path: destination, before: None, after: Some(updated.into_bytes()), permissions: permissions.clone() });
                    changes.push(Change { path, before: Some(original.into_bytes()), after: None, permissions });
                } else {
                    summaries.push(json!({"path":path,"type":"update","additions":additions,"deletions":deletions}));
                    changes.push(Change { path, before: Some(original.into_bytes()), after: Some(updated.into_bytes()), permissions });
                }
            }
        }
    }
    if cancel.load(Ordering::Acquire) { return Err(failure("Patch cancelled before writing")); }
    for (index, change) in changes.iter().enumerate() {
        if let Err(error) = commit(change, false) {
            let mut rollback_errors = Vec::new();
            for change in changes[..=index].iter().rev() {
                if let Err(error) = commit(change, true) { rollback_errors.push(error); }
            }
            return Err(failure(if rollback_errors.is_empty() { format!("Patch write failed and was rolled back: {error}") } else { format!("Patch write failed: {error}; rollback errors: {}", rollback_errors.join("; ")) }));
        }
    }
    let output = summaries.iter().map(|item| {
        let kind = match item["type"].as_str() { Some("add") => "A", Some("delete") => "D", Some("move") => "R", _ => "M" };
        let suffix = item["movePath"].as_str().map(|path| format!(" -> {path}")).unwrap_or_default();
        format!("{kind} {}{suffix}", item["path"].as_str().unwrap_or_default())
    }).collect::<Vec<_>>().join("\n");
    Ok(ToolOutput { title: format!("Patched {} file(s)", summaries.len()), output: format!("Success. Updated files:\n{output}"), metadata: json!({"files":summaries}) })
}

fn commit(change: &Change, rollback: bool) -> Result<(), String> {
    let content = if rollback { &change.before } else { &change.after };
    if let Some(bytes) = content {
        files::atomic_write(&change.path, bytes)?;
        if let Some(permissions) = &change.permissions {
            std::fs::set_permissions(&change.path, permissions.clone()).map_err(|e| format!("Cannot restore permissions on {}: {e}", change.path.display()))?;
        }
    } else if change.path.exists() {
        std::fs::remove_file(&change.path).map_err(|e| format!("Cannot remove {}: {e}", change.path.display()))?;
    }
    Ok(())
}

fn unique(touched: &mut HashSet<PathBuf>, path: &Path) -> Result<(), ToolError> {
    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_owned());
    if !touched.insert(canonical) { return Err(failure(format!("Patch modifies {} more than once; combine its hunks", path.display()))); }
    Ok(())
}

fn parse(patch: &str) -> Result<Vec<Operation>, String> {
    if patch.len() > 16 * 1024 * 1024 { return Err("Patch exceeds 16 MiB".into()); }
    let patch = patch.trim();
    let lines: Vec<_> = patch.lines().map(|line| line.trim_end_matches('\r')).collect();
    if lines.first() != Some(&"*** Begin Patch") || lines.last() != Some(&"*** End Patch") {
        return Err("Patch must start with *** Begin Patch and end with *** End Patch".into());
    }
    let mut index = 1;
    let mut operations = Vec::new();
    while index + 1 < lines.len() {
        let line = lines[index];
        if let Some(path) = line.strip_prefix("*** Add File: ") {
            let path = patch_path(path)?;
            index += 1;
            let mut content = Vec::new();
            while index < lines.len() && !lines[index].starts_with("*** ") {
                content.push(lines[index].strip_prefix('+').ok_or_else(|| format!("Add-file line {} must start with '+'", index + 1))?.into());
                index += 1;
            }
            operations.push(Operation::Add { path, lines: content });
        } else if let Some(path) = line.strip_prefix("*** Delete File: ") {
            operations.push(Operation::Delete { path: patch_path(path)? });
            index += 1;
        } else if let Some(path) = line.strip_prefix("*** Update File: ") {
            let path = patch_path(path)?;
            index += 1;
            let destination = if let Some(destination) = lines[index].strip_prefix("*** Move to: ") {
                index += 1;
                Some(patch_path(destination)?)
            } else { None };
            let mut hunks = Vec::new();
            while index < lines.len() && !lines[index].starts_with("*** ") {
                let mut hunk = Hunk::default();
                if lines[index] == "@@" {
                    index += 1;
                } else if let Some(anchor) = lines[index].strip_prefix("@@ ") {
                    hunk.anchor = Some(anchor.into());
                    index += 1;
                } else if !hunks.is_empty() {
                    return Err(format!("Expected @@ at patch line {}", index + 1));
                }
                let start = index;
                while index < lines.len() && !lines[index].starts_with("@@") && !lines[index].starts_with("*** ") {
                    let line = lines[index];
                    match line.chars().next() {
                        Some(' ') => { hunk.old.push(line[1..].into()); hunk.new.push(line[1..].into()); }
                        Some('-') => { hunk.old.push(line[1..].into()); hunk.deletions += 1; },
                        Some('+') => { hunk.new.push(line[1..].into()); hunk.additions += 1; },
                        None => { hunk.old.push(String::new()); hunk.new.push(String::new()); }
                        _ => return Err(format!("Invalid update line {}: expected space, '+' or '-' prefix", index + 1)),
                    }
                    index += 1;
                }
                if index == start { return Err("Patch hunk contains no changes or context".into()); }
                if lines.get(index) == Some(&"*** End of File") { hunk.eof = true; index += 1; }
                hunks.push(hunk);
            }
            if hunks.is_empty() && destination.is_none() { return Err(format!("Update for {path} has no hunks")); }
            operations.push(Operation::Update { path, destination, hunks });
        } else {
            return Err(format!("Unknown patch header at line {}: {line}", index + 1));
        }
    }
    if operations.is_empty() { return Err("Patch contains no file operations".into()); }
    Ok(operations)
}

fn patch_path(path: &str) -> Result<String, String> {
    if path.trim().is_empty() || path.contains('\0') { return Err("Patch file path is empty or invalid".into()); }
    Ok(path.into())
}

fn update(content: &str, hunks: &[Hunk]) -> Result<String, String> {
    let crlf = content.contains("\r\n") && !content.replace("\r\n", "").contains('\n');
    let bom = content.starts_with('\u{feff}');
    let normalized = content.trim_start_matches('\u{feff}').replace("\r\n", "\n");
    let final_newline = normalized.ends_with('\n');
    let mut lines: Vec<String> = normalized.lines().map(str::to_owned).collect();
    let mut cursor = 0;
    for hunk in hunks {
        if let Some(anchor) = &hunk.anchor {
            let index = seek(&lines, std::slice::from_ref(anchor), cursor, false).ok_or_else(|| format!("Anchor not found: {anchor}"))?;
            cursor = index + 1;
        }
        let index = if hunk.old.is_empty() {
            lines.len()
        } else {
            seek(&lines, &hunk.old, cursor, hunk.eof)
                .ok_or_else(|| format!("Expected context was not found:\n{}", hunk.old.iter().take(8).cloned().collect::<Vec<_>>().join("\n")))?
        };
        lines.splice(index..index + hunk.old.len(), hunk.new.clone());
        cursor = index + hunk.new.len();
    }
    let mut updated = lines.join("\n");
    if final_newline && !lines.is_empty() { updated.push('\n'); }
    if crlf { updated = updated.replace('\n', "\r\n"); }
    if bom { updated.insert(0, '\u{feff}'); }
    Ok(updated)
}

fn seek(lines: &[String], pattern: &[String], start: usize, eof: bool) -> Option<usize> {
    let last = lines.len().checked_sub(pattern.len())?;
    if start > last { return None; }
    for mode in 0..3 {
        for index in start..=last {
            if eof && index != last { continue; }
            if lines[index..index+pattern.len()].iter().zip(pattern).all(|(left,right)| match mode {
                0 => left == right,
                1 => left.trim_end() == right.trim_end(),
                _ => left.trim() == right.trim(),
            }) { return Some(index); }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patch_updates_multiple_hunks_preserving_crlf() {
        let parsed = parse("*** Begin Patch\n*** Update File: a\n@@\n one\n-two\n+TWO\n@@\n-four\n+FOUR\n*** End of File\n*** End Patch").unwrap();
        let Operation::Update { hunks, .. } = &parsed[0] else { panic!() };
        assert_eq!(update("one\r\ntwo\r\nthree\r\nfour\r\n", hunks).unwrap(), "one\r\nTWO\r\nthree\r\nFOUR\r\n");
    }

    #[test]
    fn invalid_later_hunk_never_writes_earlier_files() {
        let directory = std::env::temp_dir().join(crate::types::hex_id("ocmini-patch-"));
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join("a"), "original\n").unwrap();
        let result = apply(directory.to_str().unwrap(), "*** Begin Patch\n*** Add File: added\n+new\n*** Update File: a\n@@\n-does-not-exist\n+changed\n*** End Patch", &AtomicBool::new(false));
        assert!(result.is_err());
        assert!(!directory.join("added").exists());
        assert_eq!(std::fs::read_to_string(directory.join("a")).unwrap(), "original\n");
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn patch_creates_updates_moves_and_deletes() {
        let directory = std::env::temp_dir().join(crate::types::hex_id("ocmini-patch-"));
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join("old"), "old\n").unwrap();
        std::fs::write(directory.join("remove"), "remove\n").unwrap();
        let result = apply(directory.to_str().unwrap(), "*** Begin Patch\n*** Add File: nested/new\n+hello\n*** Update File: old\n*** Move to: moved\n@@\n-old\n+updated\n*** Delete File: remove\n*** End Patch", &AtomicBool::new(false)).unwrap();
        assert_eq!(result.metadata["files"].as_array().unwrap().len(), 3);
        assert_eq!(std::fs::read_to_string(directory.join("moved")).unwrap(), "updated\n");
        assert_eq!(std::fs::read_to_string(directory.join("nested/new")).unwrap(), "hello\n");
        assert!(!directory.join("old").exists());
        assert!(!directory.join("remove").exists());
        std::fs::remove_dir_all(directory).unwrap();
    }
}
