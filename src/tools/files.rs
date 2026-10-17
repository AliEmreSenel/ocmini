use super::{cap_out, failure, resolve_path};
use crate::types::{hex_id, ToolError, ToolOutput};
use base64::Engine;
use serde_json::{json, Value};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::SystemTime;

const MAX_FILE: u64 = 32 * 1024 * 1024;
const MAX_OUTPUT: usize = 200 * 1024;
const MAX_RESULTS: usize = 100;

pub(super) fn run(directory: &str, name: &str, input: &Value, cancel: &AtomicBool) -> Result<ToolOutput, ToolError> {
    let check = || if cancel.load(Ordering::Acquire) { Err(failure("Tool cancelled")) } else { Ok(()) };
    check()?;
    match name {
        "read" => read(directory, input),
        "write" => {
            let path = resolve_path(directory, super::required(input, "filePath")?);
            let content = super::required(input, "content")?;
            if content.len() as u64 > MAX_FILE { return Err(failure("Write content exceeds 32 MiB")); }
            atomic_write(&path, content.as_bytes()).map_err(failure)?;
            output(format!("Wrote {} ({} bytes)", path.display(), content.len()), &path, json!({"filepath":path,"bytes":content.len()}))
        }
        "edit" => edit(directory, input),
        "glob" => search(directory, input, None, cancel),
        "grep" => {
            let expression = regex::Regex::new(super::required(input, "pattern")?).map_err(|e| failure(format!("Invalid regex: {e}")))?;
            search(directory, input, Some(expression), cancel)
        }
        _ => Err(failure(format!("Unknown file tool {name}"))),
    }
}

fn read(directory: &str, input: &Value) -> Result<ToolOutput, ToolError> {
    let path = resolve_path(directory, super::required(input, "filePath")?);
    let metadata = std::fs::metadata(&path).map_err(|e| failure(format!("Cannot read {}: {e}", path.display())))?;
    let offset = positive(input, "offset", 1)? as usize;
    let limit = positive(input, "limit", 2000)?.min(20_000) as usize;
    if metadata.is_dir() {
        let mut entries = std::fs::read_dir(&path).map_err(|e| failure(format!("Cannot list {}: {e}", path.display())))?
            .map(|entry| entry.map(|entry| {
                let suffix = if entry.file_type().is_ok_and(|t| t.is_dir()) { "/" } else { "" };
                format!("{}{suffix}", entry.file_name().to_string_lossy())
            })).collect::<Result<Vec<_>, _>>().map_err(|e| failure(format!("Directory listing failed: {e}")))?;
        entries.sort();
        let remaining = entries.len().saturating_sub(offset.saturating_sub(1).saturating_add(limit));
        let mut text = entries.into_iter().skip(offset - 1).take(limit).collect::<Vec<_>>().join("\n");
        if remaining > 0 { text.push_str(&format!("\n[{remaining} more entries; use offset and limit]")); }
        return output(cap_out(&text, MAX_OUTPUT), &path, json!({"filepath":path,"type":"directory","truncated":remaining>0 || text.len()>MAX_OUTPUT}));
    }
    if !metadata.is_file() { return Err(failure("Read supports regular files and directories")); }
    if metadata.len() > MAX_FILE { return Err(failure(format!("{} exceeds the 32 MiB text read limit; use a targeted shell command", path.display()))); }
    let bytes = std::fs::read(&path).map_err(|e| failure(format!("Cannot read {}: {e}", path.display())))?;
    if let Some(mime) = media_type(&bytes) {
        if bytes.len() > 16 * 1024 * 1024 { return Err(failure("Media attachment exceeds 16 MiB")); }
        let data = base64::engine::general_purpose::STANDARD.encode(bytes);
        let attachment = if mime.starts_with("image/") {
            json!({"type":"image","mimeType":mime,"data":data})
        } else {
            json!({"type":"resource","resource":{"uri":format!("file://{}",path.display()),"mimeType":mime,"blob":data}})
        };
        return output(format!("Read {} ({mime} attachment)", path.display()), &path, json!({"filepath":path,"attachments":[attachment]}));
    }
    if bytes.contains(&0) { return Err(failure(format!("{} is binary; use a tool appropriate for its format", path.display()))); }
    let content = String::from_utf8(bytes).map_err(|_| failure(format!("{} is not UTF-8 text", path.display())))?;
    let total = content.lines().count();
    if total > 0 && offset > total {
        return Err(failure(format!("Offset {offset} exceeds file length ({total} lines)")));
    }
    let mut text = String::new();
    let mut returned = 0;
    for (index, line) in content.lines().enumerate().skip(offset - 1).take(limit) {
        if text.len() >= MAX_OUTPUT { break; }
        text.push_str(&format!("{}: {}\n", index + 1, cap_out(line, 16 * 1024)));
        returned += 1;
    }
    let remaining = total.saturating_sub(offset.saturating_sub(1).saturating_add(returned));
    if remaining > 0 { text.push_str(&format!("\n[{remaining} more lines; continue with offset {}]", offset + returned)); }
    output(cap_out(&text, MAX_OUTPUT), &path, json!({"filepath":path,"totalLines":total,"truncated":remaining>0 || text.len()>MAX_OUTPUT}))
}

fn media_type(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") { Some("image/png") }
    else if bytes.starts_with(b"\xff\xd8\xff") { Some("image/jpeg") }
    else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") { Some("image/gif") }
    else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(&b"WEBP"[..]) { Some("image/webp") }
    else if bytes.starts_with(b"%PDF-") { Some("application/pdf") }
    else { None }
}

pub(super) fn text_file(path: &Path) -> Result<String, ToolError> {
    let metadata = std::fs::metadata(path).map_err(|e| failure(format!("Cannot read {}: {e}", path.display())))?;
    if !metadata.is_file() || metadata.len() > MAX_FILE { return Err(failure(format!("{} is not a regular text file of at most 32 MiB", path.display()))); }
    let bytes = std::fs::read(path).map_err(|e| failure(format!("Cannot read {}: {e}", path.display())))?;
    if bytes.contains(&0) { return Err(failure(format!("{} is binary; use a tool appropriate for its format", path.display()))); }
    String::from_utf8(bytes).map_err(|_| failure(format!("{} is not UTF-8 text", path.display())))
}

fn positive(input: &Value, key: &str, default: u64) -> Result<u64, ToolError> {
    match input.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(value) => value.as_u64().filter(|n| *n > 0).ok_or_else(|| failure(format!("{key} must be a positive integer"))),
    }
}

fn edit(directory: &str, input: &Value) -> Result<ToolOutput, ToolError> {
    let path = resolve_path(directory, super::required(input, "filePath")?);
    let old = super::required(input, "oldString")?;
    let new = super::required(input, "newString")?;
    if old == new { return Err(failure("oldString and newString are identical")); }
    if old.is_empty() { return Err(failure("oldString must not be empty; use write to create or replace a file")); }
    let original = text_file(&path)?;
    let crlf = original.contains("\r\n") && !original.replace("\r\n", "").contains('\n');
    let content = original.replace("\r\n", "\n");
    let old = old.replace("\r\n", "\n");
    let new = new.replace("\r\n", "\n");
    let count = content.matches(old.as_str()).count();
    if count == 0 { return Err(failure("oldString was not found in the file")); }
    let all = match input.get("replaceAll") {
        None => false,
        Some(value) => value.as_bool().ok_or_else(|| failure("replaceAll must be boolean"))?,
    };
    if count > 1 && !all { return Err(failure(format!("oldString matches {count} locations; supply more context or set replaceAll"))); }
    let changed = if all { content.replace(old.as_str(), &new) } else { content.replacen(old.as_str(), &new, 1) };
    let changed = if crlf { changed.replace('\n', "\r\n") } else { changed };
    atomic_write(&path, changed.as_bytes()).map_err(failure)?;
    output(format!("Edited {} ({} replacement{})", path.display(), if all { count } else { 1 }, if all && count > 1 { "s" } else { "" }), &path,
        json!({"filepath":path,"replacements":if all {count} else {1}}))
}

pub(super) fn atomic_write(path: &Path, content: &[u8]) -> Result<(), String> {
    let target = if path.symlink_metadata().is_ok() { path.canonicalize().map_err(|e| format!("Cannot resolve {}: {e}", path.display()))? } else { path.to_owned() };
    if target.exists() && !target.is_file() { return Err(format!("{} is not a regular file", target.display())); }
    let parent = target.parent().ok_or_else(|| format!("{} has no parent directory", target.display()))?;
    std::fs::create_dir_all(parent).map_err(|e| format!("Cannot create parent directory {}: {e}", parent.display()))?;
    let permissions = std::fs::metadata(&target).ok().map(|meta| meta.permissions());
    let temporary = parent.join(format!(".ocmini-{}.tmp", hex_id("write_")));
    let result = (|| {
        let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(&temporary).map_err(|e| format!("Cannot create temporary file: {e}"))?;
        if let Some(permissions) = permissions { file.set_permissions(permissions).map_err(|e| format!("Cannot preserve file permissions: {e}"))?; }
        file.write_all(content).and_then(|_| file.sync_all()).map_err(|e| format!("Cannot write {}: {e}", target.display()))?;
        drop(file);
        std::fs::rename(&temporary, &target).map_err(|e| format!("Cannot replace {}: {e}", target.display()))?;
        Ok(())
    })();
    if result.is_err() { let _ = std::fs::remove_file(&temporary); }
    result
}

fn search(directory: &str, input: &Value, expression: Option<regex::Regex>, cancel: &AtomicBool) -> Result<ToolOutput, ToolError> {
    let base = resolve_path(directory, input["path"].as_str().unwrap_or("."));
    if !base.exists() { return Err(failure(format!("Search path {} does not exist", base.display()))); }
    let pattern = super::required(input, "pattern")?;
    let include = if expression.is_some() { input["include"].as_str() } else { Some(pattern) };
    let patterns = include.map(|pattern| expand_braces(pattern).into_iter().map(|value| glob::Pattern::new(&value).map_err(|e| failure(format!("Invalid glob pattern: {e}")))).collect::<Result<Vec<_>,_>>()).transpose()?;
    let mut builder = ignore::WalkBuilder::new(&base);
    builder.hidden(false).follow_links(false).git_ignore(true).git_global(true).git_exclude(true).filter_entry(|entry| {
        entry.depth() == 0 || !entry.file_type().is_some_and(|kind| kind.is_dir()) || !matches!(entry.file_name().to_str(), Some(".git" | "node_modules" | "target"))
    });
    let mut files = Vec::new();
    let mut scan_errors = 0;
    for entry in builder.build() {
        if cancel.load(Ordering::Acquire) { return Err(failure("Search cancelled")); }
        let entry = match entry { Ok(entry) => entry, Err(_) => { scan_errors += 1; continue; } };
        if !entry.file_type().is_some_and(|kind| kind.is_file()) { continue; }
        let path = entry.into_path();
        let relative = path.strip_prefix(&base).unwrap_or(&path);
        if let Some(patterns) = &patterns {
            let matches = patterns.iter().any(|pattern| pattern.matches_path(relative) || pattern.matches_path(&path) || (expression.is_some() && path.file_name().is_some_and(|name| pattern.matches(&name.to_string_lossy()))));
            if !matches { continue; }
        }
        let modified = path.metadata().and_then(|meta| meta.modified()).unwrap_or(SystemTime::UNIX_EPOCH);
        files.push((modified, path));
        if files.len() >= 100_000 { return Err(failure("Search has more than 100000 candidate files; narrow the path or pattern")); }
    }
    files.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    let mut text = String::new();
    let mut count = 0;
    let mut truncated = false;
    for (_, path) in files {
        if cancel.load(Ordering::Acquire) { return Err(failure("Search cancelled")); }
        if let Some(expression) = &expression {
            let content = match text_file(&path) { Ok(text) => text, Err(_) => continue };
            for (index, line) in content.lines().enumerate() {
                if expression.is_match(line) {
                    if count >= MAX_RESULTS || text.len() >= MAX_OUTPUT { truncated = true; break; }
                    text.push_str(&format!("{}:{}:{}\n", path.display(), index + 1, cap_out(line, 4096)));
                    count += 1;
                }
            }
        } else {
            if count >= MAX_RESULTS { truncated = true; break; }
            text.push_str(&format!("{}\n", path.display()));
            count += 1;
        }
        if truncated { break; }
    }
    if count == 0 { text.push_str("No matches found."); }
    if truncated { text.push_str("\n[Results truncated; narrow the pattern or search path]"); }
    if scan_errors > 0 { text.push_str(&format!("\n[{scan_errors} paths could not be traversed]")); }
    Ok(ToolOutput { output: cap_out(&text, MAX_OUTPUT), title: pattern.into(), metadata: json!({"matches":count,"truncated":truncated,"scanErrors":scan_errors}) })
}

fn expand_braces(pattern: &str) -> Vec<String> {
    let Some(open) = pattern.find('{') else { return vec![pattern.into()]; };
    let Some(close) = pattern[open..].find('}').map(|value| open + value) else { return vec![pattern.into()]; };
    let choices = &pattern[open+1..close];
    if !choices.contains(',') { return vec![pattern.into()]; }
    choices.split(',').take(32).flat_map(|choice| expand_braces(&format!("{}{choice}{}", &pattern[..open], &pattern[close+1..]))).take(256).collect()
}

fn output(text: String, path: &Path, metadata: Value) -> Result<ToolOutput, ToolError> {
    Ok(ToolOutput { output: text, title: path.display().to_string(), metadata })
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Directory(PathBuf);
    impl Directory {
        fn new() -> Self { let path = std::env::temp_dir().join(hex_id("ocmini-files-")); std::fs::create_dir_all(&path).unwrap(); Self(path) }
        fn dir(&self) -> &str { self.0.to_str().unwrap() }
    }
    impl Drop for Directory { fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.0); } }

    #[test]
    fn files_are_relative_to_session_and_edit_preserves_crlf() {
        let directory = Directory::new();
        let flag = AtomicBool::new(false);
        run(directory.dir(), "write", &json!({"filePath":"nested/α.txt","content":"one\r\ntwo\r\n"}), &flag).unwrap();
        run(directory.dir(), "edit", &json!({"filePath":"nested/α.txt","oldString":"two\n","newString":"three\n"}), &flag).unwrap();
        assert_eq!(std::fs::read_to_string(directory.0.join("nested/α.txt")).unwrap(), "one\r\nthree\r\n");
        let read = run(directory.dir(), "read", &json!({"filePath":"nested/α.txt","offset":2,"limit":1}), &flag).unwrap();
        assert_eq!(read.output, "2: three\n");
        assert!(run(directory.dir(), "edit", &json!({"filePath":"nested/α.txt","oldString":"","newString":"x"}), &flag).is_err());
    }

    #[test]
    fn ambiguous_edit_leaves_file_unchanged() {
        let directory = Directory::new();
        let path = directory.0.join("file");
        std::fs::write(&path, "same same").unwrap();
        assert!(edit(directory.dir(), &json!({"filePath":"file","oldString":"same","newString":"new"})).is_err());
        assert_eq!(std::fs::read_to_string(path).unwrap(), "same same");
    }

    #[test]
    fn search_respects_gitignore_but_includes_hidden_files() {
        let directory = Directory::new();
        std::fs::create_dir(directory.0.join(".git")).unwrap();
        std::fs::write(directory.0.join(".gitignore"), "ignored.txt\n").unwrap();
        std::fs::write(directory.0.join("ignored.txt"), "needle").unwrap();
        std::fs::write(directory.0.join(".visible.txt"), "needle").unwrap();
        let found = run(directory.dir(), "grep", &json!({"pattern":"needle","include":"*.txt"}), &AtomicBool::new(false)).unwrap();
        assert!(found.output.contains(".visible.txt:1:needle"));
        assert!(!found.output.contains("ignored.txt"));
    }
    #[test]
    fn image_read_returns_attachment_without_binary_text() {
        let directory = Directory::new();
        let bytes = b"\x89PNG\r\n\x1a\nfixture";
        std::fs::write(directory.0.join("image.png"), bytes).unwrap();
        let result = run(directory.dir(), "read", &json!({"filePath":"image.png"}), &AtomicBool::new(false)).unwrap();
        assert_eq!(result.metadata["attachments"][0]["mimeType"], "image/png");
        assert_eq!(result.metadata["attachments"][0]["data"], base64::engine::general_purpose::STANDARD.encode(bytes));
        assert!(!result.output.contains("fixture"));
    }

}
