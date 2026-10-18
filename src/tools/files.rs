use super::{failure, resolve_path};
use crate::types::{ToolError, ToolOutput};
use base64::Engine;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

const MAX_OUTPUT: usize = 50 * 1024;
const MAX_LINE: usize = 2000;

pub(super) fn run(
    directory: &str,
    name: &str,
    input: &Value,
    cancel: &AtomicBool,
) -> Result<ToolOutput, ToolError> {
    let check = || {
        if cancel.load(Ordering::Acquire) {
            Err(failure("Tool cancelled"))
        } else {
            Ok(())
        }
    };
    check()?;
    match name {
        "read" => read(directory, input, cancel),
        "write" => {
            let path = resolve_path(directory, super::required(input, "filePath")?);
            let content = super::required(input, "content")?;
            atomic_write(&path, content.as_bytes()).map_err(failure)?;
            output(
                format!("Wrote {} ({} bytes)", path.display(), content.len()),
                &path,
                json!({"filepath":path,"bytes":content.len()}),
            )
        }
        "edit" => edit(directory, input),
        _ => Err(failure(format!("Unknown file tool {name}"))),
    }
}

fn read(directory: &str, input: &Value, cancel: &AtomicBool) -> Result<ToolOutput, ToolError> {
    let path = resolve_path(directory, super::required(input, "filePath")?);
    let metadata = std::fs::metadata(&path)
        .map_err(|e| failure(format!("Cannot read {}: {e}", path.display())))?;
    let offset = nonnegative(input, "offset", 1)?.max(1) as usize;
    let limit = nonnegative(input, "limit", 2000)? as usize;
    if metadata.is_dir() {
        let mut entries = std::fs::read_dir(&path)
            .map_err(|e| failure(format!("Cannot list {}: {e}", path.display())))?
            .map(|entry| {
                entry.map(|entry| {
                    let suffix = if entry.file_type().is_ok_and(|t| t.is_dir()) {
                        "/"
                    } else {
                        ""
                    };
                    format!("{}{suffix}", entry.file_name().to_string_lossy())
                })
            })
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| failure(format!("Directory listing failed: {e}")))?;
        entries.sort();
        let remaining = entries
            .len()
            .saturating_sub(offset.saturating_sub(1).saturating_add(limit));
        let mut text = entries
            .into_iter()
            .skip(offset - 1)
            .take(limit)
            .collect::<Vec<_>>()
            .join("\n");
        if remaining > 0 {
            text.push_str(&format!(
                "\n[{remaining} more entries; use offset and limit]"
            ));
        }
        let metadata = json!({"filepath":path,"type":"directory","truncated":remaining>0 || text.len()>MAX_OUTPUT});
        return output(text, &path, metadata);
    }
    if !metadata.is_file() {
        return Err(failure("Read supports regular files and directories"));
    }
    let mut reader = text_reader(&path)?;
    let header = reader
        .fill_buf()
        .map_err(|e| failure(format!("Cannot read {}: {e}", path.display())))?;
    if let Some(mime) = media_type(header) {
        if metadata.len() > 16 * 1024 * 1024 {
            return Err(failure("Media attachment exceeds 16 MiB"));
        }
        let mut bytes = Vec::new();
        reader
            .take(16 * 1024 * 1024 + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| failure(format!("Cannot read {}: {e}", path.display())))?;
        if bytes.len() > 16 * 1024 * 1024 {
            return Err(failure("Media attachment exceeds 16 MiB"));
        }
        let data = base64::engine::general_purpose::STANDARD.encode(bytes);
        let attachment = if mime.starts_with("image/") {
            json!({"type":"image","mimeType":mime,"data":data})
        } else {
            json!({"type":"resource","resource":{"uri":format!("file://{}",path.display()),"mimeType":mime,"blob":data}})
        };
        return output(
            format!("Read {} ({mime} attachment)", path.display()),
            &path,
            json!({"filepath":path,"attachments":[attachment]}),
        );
    }
    let mut text = String::new();
    let mut text_bytes = 0;
    let mut returned = 0;
    let mut output_capped = false;
    let total = scan_text(&path, reader, cancel, |index, line| {
        if index >= offset && returned < limit && !output_capped {
            let mut preview: String = line.chars().take(MAX_LINE).collect();
            if preview.len() < line.len() {
                preview.push_str("... (line truncated to 2000 chars)");
            }
            let formatted = format!("{index}: {preview}\n");
            let size = preview.len() + usize::from(returned > 0);
            if text_bytes + size <= MAX_OUTPUT {
                text.push_str(&formatted);
                text_bytes += size;
                returned += 1;
            } else {
                output_capped = true;
            }
        }
    })?;
    if total > 0 && offset > total {
        return Err(failure(format!(
            "Offset {offset} exceeds file length ({total} lines)"
        )));
    }
    let remaining = total.saturating_sub(offset.saturating_sub(1).saturating_add(returned));
    if remaining > 0 {
        text.push_str(&format!(
            "\n[{remaining} more lines; continue with offset {}]",
            offset + returned
        ));
    }
    let metadata = json!({"filepath":path,"totalLines":total,"truncated":remaining>0 || text.len()>MAX_OUTPUT});
    output(text, &path, metadata)
}

fn text_reader(path: &Path) -> Result<BufReader<std::fs::File>, ToolError> {
    let metadata = std::fs::metadata(path)
        .map_err(|e| failure(format!("Cannot read {}: {e}", path.display())))?;
    if !metadata.is_file() {
        return Err(failure(format!(
            "{} is not a regular text file",
            path.display()
        )));
    }
    std::fs::File::open(path)
        .map(BufReader::new)
        .map_err(|e| failure(format!("Cannot read {}: {e}", path.display())))
}

/// Reuse one line buffer instead of retaining the entire file. Scan even past
/// the requested output window to validate UTF-8/binary data and count lines.
fn scan_text(
    path: &Path,
    reader: impl BufRead,
    cancel: &AtomicBool,
    mut visit: impl FnMut(usize, &str),
) -> Result<usize, ToolError> {
    let mut reader = reader;
    let mut line = String::new();
    let mut carry = Vec::new();
    let mut decoded = Vec::new();
    let mut characters = 0;
    let mut extra_characters = false;
    let mut has_bytes = false;
    let mut total = 0;
    loop {
        if cancel.load(Ordering::Acquire) {
            return Err(failure("File scan cancelled"));
        }
        let buffer = reader
            .fill_buf()
            .map_err(|error| failure(format!("Cannot read {}: {error}", path.display())))?;
        if buffer.is_empty() {
            if !carry.is_empty() {
                return Err(failure(format!("{} is not UTF-8 text", path.display())));
            }
            if has_bytes {
                total += 1;
                visit(total, &line);
            }
            return Ok(total);
        }
        let newline = buffer.iter().position(|byte| *byte == b'\n');
        let length = newline.unwrap_or(buffer.len());
        let chunk = &buffer[..length];
        if chunk.contains(&0) {
            return Err(failure(format!(
                "{} is binary; use a tool appropriate for its format",
                path.display()
            )));
        }
        decoded.clear();
        decoded.extend_from_slice(&carry);
        decoded.extend_from_slice(chunk);
        let (text, pending) = match std::str::from_utf8(&decoded) {
            Ok(text) => (text, &[][..]),
            Err(error) if error.error_len().is_none() => (
                std::str::from_utf8(&decoded[..error.valid_up_to()])
                    .map_err(|_| failure("Invalid UTF-8 prefix"))?,
                &decoded[error.valid_up_to()..],
            ),
            Err(_) => return Err(failure(format!("{} is not UTF-8 text", path.display()))),
        };
        // Retain only enough characters to expose the per-line truncation.
        // Continue validating and counting without allocating the whole line.
        let mut incoming = text.chars();
        for character in incoming
            .by_ref()
            .take((MAX_LINE + 1).saturating_sub(characters))
        {
            line.push(character);
            characters += 1;
        }
        extra_characters |= incoming.next().is_some();
        carry.clear();
        carry.extend_from_slice(pending);
        has_bytes |= length > 0;
        if newline.is_some() {
            if !carry.is_empty() {
                return Err(failure(format!("{} is not UTF-8 text", path.display())));
            }
            total += 1;
            visit(
                total,
                if extra_characters {
                    &line
                } else {
                    line.strip_suffix('\r').unwrap_or(&line)
                },
            );
            line.clear();
            characters = 0;
            extra_characters = false;
            has_bytes = false;
        }
        reader.consume(length + usize::from(newline.is_some()));
    }
}

pub(crate) fn media_type(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(&b"WEBP"[..]) {
        Some("image/webp")
    } else if bytes.starts_with(b"%PDF-") {
        Some("application/pdf")
    } else {
        None
    }
}

pub(super) fn text_file(path: &Path) -> Result<String, ToolError> {
    let metadata = std::fs::metadata(path)
        .map_err(|e| failure(format!("Cannot read {}: {e}", path.display())))?;
    if !metadata.is_file() {
        return Err(failure(format!(
            "{} is not a regular text file",
            path.display()
        )));
    }
    let bytes =
        std::fs::read(path).map_err(|e| failure(format!("Cannot read {}: {e}", path.display())))?;
    if bytes.contains(&0) {
        return Err(failure(format!(
            "{} is binary; use a tool appropriate for its format",
            path.display()
        )));
    }
    String::from_utf8(bytes).map_err(|_| failure(format!("{} is not UTF-8 text", path.display())))
}

fn nonnegative(input: &Value, key: &str, default: u64) -> Result<u64, ToolError> {
    match input.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(value) => value
            .as_u64()
            .ok_or_else(|| failure(format!("{key} must be a nonnegative integer"))),
    }
}

fn edit(directory: &str, input: &Value) -> Result<ToolOutput, ToolError> {
    let path = resolve_path(directory, super::required(input, "filePath")?);
    let old = super::required(input, "oldString")?;
    let new = super::required(input, "newString")?;
    if old == new {
        return Err(failure("oldString and newString are identical"));
    }
    if old.is_empty() {
        return Err(failure(
            "oldString must not be empty; use write to create or replace a file",
        ));
    }
    let original = text_file(&path)?;
    let crlf = original.contains("\r\n") && !original.replace("\r\n", "").contains('\n');
    let content = original.replace("\r\n", "\n");
    let old = old.replace("\r\n", "\n");
    let new = new.replace("\r\n", "\n");
    let count = content.matches(old.as_str()).count();
    if count == 0 {
        return Err(failure("oldString was not found in the file"));
    }
    let all = match input.get("replaceAll") {
        None => false,
        Some(value) => value
            .as_bool()
            .ok_or_else(|| failure("replaceAll must be boolean"))?,
    };
    if count > 1 && !all {
        return Err(failure(format!(
            "oldString matches {count} locations; supply more context or set replaceAll"
        )));
    }
    let changed = if all {
        content.replace(old.as_str(), &new)
    } else {
        content.replacen(old.as_str(), &new, 1)
    };
    let changed = if crlf {
        changed.replace('\n', "\r\n")
    } else {
        changed
    };
    atomic_write(&path, changed.as_bytes()).map_err(failure)?;
    output(
        format!(
            "Edited {} ({} replacement{})",
            path.display(),
            if all { count } else { 1 },
            if all && count > 1 { "s" } else { "" }
        ),
        &path,
        json!({"filepath":path,"replacements":if all {count} else {1}}),
    )
}

pub(super) fn atomic_write(path: &Path, content: &[u8]) -> Result<(), String> {
    let target = if path.symlink_metadata().is_ok() {
        path.canonicalize()
            .map_err(|e| format!("Cannot resolve {}: {e}", path.display()))?
    } else {
        path.to_owned()
    };
    if target.exists() && !target.is_file() {
        return Err(format!("{} is not a regular file", target.display()));
    }
    let parent = target
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", target.display()))?;
    std::fs::create_dir_all(parent)
        .map_err(|e| format!("Cannot create parent directory {}: {e}", parent.display()))?;
    let permissions = std::fs::metadata(&target)
        .ok()
        .map(|meta| meta.permissions());
    let mut builder = tempfile::Builder::new();
    builder.prefix(".ocmini-");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        builder.permissions(std::fs::Permissions::from_mode(0o666));
    }
    let mut file = builder
        .tempfile_in(parent)
        .map_err(|e| format!("Cannot create temporary file: {e}"))?;
    if let Some(permissions) = permissions {
        file.as_file()
            .set_permissions(permissions)
            .map_err(|e| format!("Cannot preserve file permissions: {e}"))?;
    }
    file.write_all(content)
        .and_then(|_| file.as_file().sync_all())
        .map_err(|e| format!("Cannot write {}: {e}", target.display()))?;
    file.persist(&target)
        .map_err(|e| format!("Cannot replace {}: {e}", target.display()))?;
    Ok(())
}

fn output(text: String, path: &Path, metadata: Value) -> Result<ToolOutput, ToolError> {
    Ok(ToolOutput {
        output: text,
        title: path.display().to_string(),
        metadata,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Directory(tempfile::TempDir);
    impl Directory {
        fn new() -> Self {
            Self(tempfile::tempdir().unwrap())
        }
        fn dir(&self) -> &str {
            self.0.path().to_str().unwrap()
        }
    }

    #[test]
    fn read_pages_support_large_text_files_and_pages_over_20000_lines() {
        let directory = Directory::new();
        let path = directory.0.path().join("large.txt");
        let mut file = std::fs::File::create(&path).unwrap();
        let chunk = "x".repeat(1024 * 1024);
        for _ in 0..33 {
            file.write_all(chunk.as_bytes()).unwrap();
        }
        file.write_all(b"\nLAST_LINE\n").unwrap();
        drop(file);
        let result = run(
            directory.dir(),
            "read",
            &json!({"filePath":"large.txt","offset":2,"limit":1}),
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(result.output, "2: LAST_LINE\n");
        assert_eq!(result.metadata["totalLines"], 2);
        // Directory pages have no byte-driven file-line cap, so this detects
        // the former implicit 20,000-entry clamp without huge file contents.
        let entries = directory.0.path().join("entries");
        std::fs::create_dir(&entries).unwrap();
        for index in 0..20_001 {
            std::fs::File::create(entries.join(format!("{index:05}"))).unwrap();
        }
        let result = run(
            directory.dir(),
            "read",
            &json!({"filePath":"entries","limit":20_001}),
            &AtomicBool::new(false),
        )
        .unwrap();
        assert!(!result.metadata["truncated"].is_null());
        // The raw page covers all entries; its preview can still hit 50 KiB.
        assert!(!result.output.contains("more entries; use offset and limit"));
    }

    #[test]
    fn read_byte_pages_keep_contiguous_lines_and_report_the_next_offset() {
        let directory = Directory::new();
        let content = "é".repeat(1_500) + "\n";
        std::fs::write(directory.0.path().join("page.txt"), content.repeat(50)).unwrap();
        let result = run(
            directory.dir(),
            "read",
            &json!({"filePath":"page.txt","limit":50000}),
            &AtomicBool::new(false),
        )
        .unwrap();
        assert!(result.output.contains("continue with offset 18"));
        assert!(!result.output.contains("19: "));
        let next = run(
            directory.dir(),
            "read",
            &json!({"filePath":"page.txt","offset":18,"limit":1}),
            &AtomicBool::new(false),
        )
        .unwrap();
        assert!(next.output.starts_with("18: "));
        let empty = run(
            directory.dir(),
            "read",
            &json!({"filePath":"page.txt","offset":0,"limit":0}),
            &AtomicBool::new(false),
        )
        .unwrap();
        assert!(empty.output.contains("continue with offset 1"));
    }

    #[test]
    fn line_limits_preserve_split_utf8_and_crlf_boundaries() {
        let content = format!(
            "{}🙂\r\n{}\r{}\r\nlast\r",
            "é".repeat(1999),
            "x".repeat(2000),
            "z".repeat(100)
        );
        let reader = BufReader::with_capacity(3, std::io::Cursor::new(content.as_bytes()));
        let mut lines = Vec::new();
        scan_text(
            Path::new("fixture"),
            reader,
            &AtomicBool::new(false),
            |_, line| lines.push(line.to_string()),
        )
        .unwrap();
        assert!(lines[0].ends_with('🙂'));
        assert_eq!(lines[0].chars().count(), 2000);
        assert_eq!(lines[1].chars().count(), 2001);
        assert!(lines[1].ends_with('\r'));
        assert_eq!(lines[2], "last\r");
    }

    #[test]
    fn files_are_relative_to_session_and_edit_preserves_crlf() {
        let directory = Directory::new();
        let flag = AtomicBool::new(false);
        run(
            directory.dir(),
            "write",
            &json!({"filePath":"nested/α.txt","content":"one\r\ntwo\r\n"}),
            &flag,
        )
        .unwrap();
        run(
            directory.dir(),
            "edit",
            &json!({"filePath":"nested/α.txt","oldString":"two\n","newString":"three\n"}),
            &flag,
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(directory.0.path().join("nested/α.txt")).unwrap(),
            "one\r\nthree\r\n"
        );
        let read = run(
            directory.dir(),
            "read",
            &json!({"filePath":"nested/α.txt","offset":2,"limit":1}),
            &flag,
        )
        .unwrap();
        assert_eq!(read.output, "2: three\n");
        assert!(run(
            directory.dir(),
            "edit",
            &json!({"filePath":"nested/α.txt","oldString":"","newString":"x"}),
            &flag
        )
        .is_err());
    }

    #[test]
    fn ambiguous_edit_leaves_file_unchanged() {
        let directory = Directory::new();
        let path = directory.0.path().join("file");
        std::fs::write(&path, "same same").unwrap();
        assert!(edit(
            directory.dir(),
            &json!({"filePath":"file","oldString":"same","newString":"new"})
        )
        .is_err());
        assert_eq!(std::fs::read_to_string(path).unwrap(), "same same");
    }

    #[test]
    fn streamed_reads_preserve_line_endings_unicode_offsets_and_counts() {
        let directory = Directory::new();
        let path = directory.0.path().join("lines.txt");
        let flag = AtomicBool::new(false);
        let content = "first\r\n\n中文 café ☕\nlast\r";
        std::fs::write(&path, content).unwrap();
        for offset in 1..=4 {
            let result = run(
                directory.dir(),
                "read",
                &json!({"filePath":"lines.txt","offset":offset,"limit":1}),
                &flag,
            )
            .unwrap();
            let lines: Vec<_> = content.lines().collect();
            assert!(result
                .output
                .starts_with(&format!("{offset}: {}\n", lines[offset - 1])));
            assert_eq!(result.metadata["totalLines"], 4);
            assert_eq!(result.metadata["truncated"], offset < 4);
        }
        assert!(run(
            directory.dir(),
            "read",
            &json!({"filePath":"lines.txt","offset":5}),
            &flag
        )
        .unwrap_err()
        .message
        .contains("exceeds file length"));
        std::fs::write(&path, "").unwrap();
        let result = run(
            directory.dir(),
            "read",
            &json!({"filePath":"lines.txt","offset":100}),
            &flag,
        )
        .unwrap();
        assert_eq!(result.output, "");
        assert_eq!(result.metadata["totalLines"], 0);
    }

    #[test]
    fn scans_validate_data_beyond_the_requested_output() {
        let directory = Directory::new();
        let flag = AtomicBool::new(false);
        let prefix = "needle\n".repeat(110);
        for (name, tail) in [("binary.txt", &b"\0"[..]), ("invalid.txt", &b"\xff"[..])] {
            let mut data = prefix.as_bytes().to_vec();
            data.extend_from_slice(tail);
            std::fs::write(directory.0.path().join(name), data).unwrap();
            assert!(run(
                directory.dir(),
                "read",
                &json!({"filePath":name,"limit":1}),
                &flag
            )
            .is_err());
        }
    }

    #[test]
    fn long_read_lines_follow_the_2000_character_limit() {
        let directory = Directory::new();
        let flag = AtomicBool::new(false);
        let line = format!("START{}END", "é".repeat(20_000));
        std::fs::write(directory.0.path().join("long.txt"), &line).unwrap();
        let result = run(
            directory.dir(),
            "read",
            &json!({"filePath":"long.txt"}),
            &flag,
        )
        .unwrap();
        assert!(result.output.contains("START"));
        assert!(!result.output.contains("END"));
        assert!(result.output.contains("line truncated to 2000 chars"));
        assert!(result.output.len() < 17 * 1024);
        assert_eq!(result.metadata["totalLines"], 1);
    }

    #[test]
    fn large_scans_can_be_cancelled_between_lines() {
        let flag = AtomicBool::new(false);
        let result = scan_text(
            Path::new("fixture"),
            std::io::Cursor::new(b"first\nsecond\n"),
            &flag,
            |_, _| flag.store(true, Ordering::Release),
        );
        assert!(result.unwrap_err().message.contains("cancelled"));
    }

    #[test]
    fn image_read_returns_attachment_without_binary_text() {
        let directory = Directory::new();
        let bytes = b"\x89PNG\r\n\x1a\nfixture";
        std::fs::write(directory.0.path().join("image.png"), bytes).unwrap();
        let result = run(
            directory.dir(),
            "read",
            &json!({"filePath":"image.png"}),
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(result.metadata["attachments"][0]["mimeType"], "image/png");
        assert_eq!(
            result.metadata["attachments"][0]["data"],
            base64::engine::general_purpose::STANDARD.encode(bytes)
        );
        assert!(!result.output.contains("fixture"));
    }

    #[cfg(unix)]
    #[test]
    fn atomic_writes_follow_symlinks_and_preserve_executable_permissions() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("script");
        let link = directory.path().join("link");
        std::fs::write(&target, "before").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o751)).unwrap();
        symlink(&target, &link).unwrap();
        atomic_write(&link, b"after").unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "after");
        assert!(link.symlink_metadata().unwrap().is_symlink());
        assert_eq!(
            target.metadata().unwrap().permissions().mode() & 0o777,
            0o751
        );
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 2);
    }
}
