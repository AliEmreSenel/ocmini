use super::{failure, resolve_path};
use crate::types::{OperationError, ToolOutput};
use base64::Engine;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use tokio_util::sync::CancellationToken;

const MAX_OUTPUT: usize = 50 * 1024;
const MAX_LINE: usize = 2000;
const RANGE_PAGE_NOTES: usize = "\n[Output truncated; continue requested ranges from line ]".len()
    + "\n[End of file after line ; requested lines beyond EOF are missing]".len()
    + 2 * (usize::MAX.ilog10() as usize + 1);

#[derive(Clone, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ReadInput {
    pub file_path: String,
    #[schemars(range(min = 1))]
    offset: Option<u64>,
    #[schemars(range(min = 1))]
    limit: Option<u64>,
    /// Inclusive, one-based line ranges. Cannot be combined with offset or limit.
    ranges: Option<Vec<LineRange>>,
}

#[derive(Clone, serde::Deserialize, schemars::JsonSchema)]
struct LineRange {
    #[schemars(range(min = 1))]
    start: u64,
    #[schemars(range(min = 1))]
    end: u64,
}

#[cfg(test)]
pub fn run(
    directory: &str,
    name: &str,
    input: &Value,
    cancel: &CancellationToken,
) -> Result<ToolOutput, OperationError> {
    let check = || {
        if cancel.is_cancelled() {
            Err(failure("Tool cancelled"))
        } else {
            Ok(())
        }
    };
    check()?;
    match name {
        "read" => read(
            directory,
            &super::decode(&super::alias(input, "filePath", "path"))?,
            cancel,
        ),
        "write" | "edit" => {
            super::changes::prepare_sync(directory, name, input, cancel)?.commit(cancel)
        }
        _ => Err(failure(format!("Unknown file tool {name}"))),
    }
}

pub fn validate_read(input: &ReadInput) -> Result<(), OperationError> {
    if input.offset == Some(0) {
        return Err(failure("offset must be a positive integer (1-based)"));
    }
    if input.limit == Some(0) {
        return Err(failure("limit must be a positive integer"));
    }
    if let Some(ranges) = &input.ranges {
        if input.offset.is_some()
            || input.limit.is_some()
            || ranges.is_empty()
            || ranges.iter().any(|r| r.start == 0 || r.end < r.start)
        {
            return Err(failure("ranges must contain valid one-based inclusive spans and cannot be combined with offset or limit"));
        }
    }
    Ok(())
}

pub fn read(
    directory: &str,
    input: &ReadInput,
    cancel: &CancellationToken,
) -> Result<ToolOutput, OperationError> {
    validate_read(input)?;
    let mut ranges = input.ranges.clone();
    if let Some(ranges) = &mut ranges {
        ranges.sort_by_key(|r| r.start);
        let mut merged: Vec<LineRange> = Vec::with_capacity(ranges.len());
        for range in ranges.drain(..) {
            if let Some(last) = merged.last_mut().filter(|last| range.start <= last.end) {
                last.end = last.end.max(range.end);
            } else {
                merged.push(range);
            }
        }
        *ranges = merged;
    }
    let path = resolve_path(directory, &input.file_path);
    let metadata = std::fs::metadata(&path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound
            && path.parent().is_some_and(|parent| !parent.is_dir())
        {
            let parent = path.parent().expect("checked parent");
            let nearest = parent.ancestors().find(|candidate| candidate.is_dir());
            failure(format!(
                "Cannot read {}: folder {} does not exist; nearest existing folder: {}",
                path.display(),
                parent.display(),
                nearest
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "none".into())
            ))
        } else {
            failure(format!("Cannot read {}: {error}", path.display()))
        }
    })?;
    let offset = usize::try_from(input.offset.unwrap_or(1).max(1)).unwrap_or(usize::MAX);
    let limit = usize::try_from(input.limit.unwrap_or(2000)).unwrap_or(usize::MAX);
    if metadata.is_dir() {
        if ranges.is_some() {
            return Err(failure("ranges are only supported for text files"));
        }
        let mut entries = std::fs::read_dir(&path)
            .map_err(|e| failure(format!("Cannot list {}: {e}", path.display())))?
            .map(|entry| {
                entry.map(|entry| {
                    let file_type = entry.file_type().ok();
                    let name = entry.file_name().to_string_lossy().into_owned();
                    if file_type.is_some_and(|t| t.is_symlink()) {
                        let target_is_dir =
                            std::fs::metadata(entry.path()).is_ok_and(|m| m.is_dir());
                        match std::fs::read_link(entry.path()) {
                            Ok(target) => {
                                let dir_mark = if target_is_dir { "/" } else { "" };
                                format!("{name}@ -> {}{dir_mark}", target.display())
                            }
                            Err(_) => format!("{name}@"),
                        }
                    } else if file_type.is_some_and(|t| t.is_dir()) {
                        format!("{name}/")
                    } else {
                        name
                    }
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
    let mut reader = BufReader::new(
        std::fs::File::open(&path)
            .map_err(|e| failure(format!("Cannot read {}: {e}", path.display())))?,
    );
    let header = reader
        .fill_buf()
        .map_err(|e| failure(format!("Cannot read {}: {e}", path.display())))?;
    if let Some(mime) = media_type(header) {
        if ranges.is_some() {
            return Err(failure("ranges are only supported for text files"));
        }
        if metadata.len() > 16 * 1024 * 1024 {
            return Err(failure("Media attachment exceeds 16 MiB"));
        }
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
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
    let mut range_index = 0;
    let mut next_line = None;
    let page_size = if ranges.is_some() {
        MAX_OUTPUT - RANGE_PAGE_NOTES
    } else {
        MAX_OUTPUT - 80
    };
    let total = scan_text_with(
        &path,
        reader,
        cancel,
        |index| {
            if let Some(ranges) = &ranges {
                while range_index < ranges.len() && ranges[range_index].end < index as u64 {
                    range_index += 1;
                }
                if range_index >= ranges.len() {
                    LineAction::Stop
                } else if ranges[range_index].start <= index as u64 {
                    LineAction::Collect
                } else {
                    LineAction::Skip
                }
            } else if index < offset {
                LineAction::Skip
            } else if limit == 0 {
                LineAction::Stop
            } else {
                LineAction::Collect
            }
        },
        |index, line| {
            let mut preview: String = line.chars().take(MAX_LINE).collect();
            if preview.len() < line.len() {
                preview.push_str("... (line truncated to 2000 chars)");
            }
            let formatted = format!("{index}: {preview}\n");
            let size = formatted.len();
            if text_bytes + size <= page_size {
                text.push_str(&formatted);
                text_bytes += size;
                returned += 1;
            } else {
                output_capped = true;
                next_line = Some(index);
            }
            if ranges.is_some() {
                !output_capped
            } else {
                returned < limit && !output_capped
            }
        },
    )?;
    if let Some(ranges) = &ranges {
        let missing: Vec<_> = ranges
            .iter()
            .filter_map(|range| {
                let start = range.start.max((total as u64).saturating_add(1));
                (start <= range.end).then(|| json!({"start": start, "end": range.end}))
            })
            .collect();
        if let Some(next) = next_line {
            text.push_str(&format!(
                "\n[Output truncated; continue requested ranges from line {next}]"
            ));
        }
        if !missing.is_empty() {
            text.push_str(&format!(
                "\n[End of file after line {total}; requested lines beyond EOF are missing]"
            ));
        }
        return output(
            text,
            &path,
            json!({"filepath":path,"totalLines":total,"truncated":output_capped,"nextLine":next_line,"missingRanges":missing}),
        );
    }
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

#[derive(Clone, Copy, PartialEq, Eq)]
enum LineAction {
    Skip,
    Collect,
    Stop,
}

#[cfg(test)]
fn scan_text(
    path: &Path,
    reader: impl BufRead,
    cancel: &CancellationToken,
    visit: impl FnMut(usize, &str) -> bool,
) -> Result<usize, OperationError> {
    scan_text_with(path, reader, cancel, |_| LineAction::Collect, visit)
}

/// Reuse one line buffer instead of retaining the entire file. Scan even past
/// the requested output window to validate UTF-8/binary data and count lines.
fn scan_text_with(
    path: &Path,
    reader: impl BufRead,
    cancel: &CancellationToken,
    mut action: impl FnMut(usize) -> LineAction,
    mut visit: impl FnMut(usize, &str) -> bool,
) -> Result<usize, OperationError> {
    let mut reader = reader;
    let mut line = String::new();
    let mut carry = Vec::new();
    let mut decoded = Vec::new();
    let mut characters = 0;
    let mut extra_characters = false;
    let mut has_bytes = false;
    let mut total = 0;
    let mut current = action(1);
    if current == LineAction::Stop {
        return count_remaining_lines(path, reader, cancel);
    }
    loop {
        if cancel.is_cancelled() {
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
                if current == LineAction::Collect {
                    visit(total, &line);
                }
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
        let slice: &[u8] = if carry.is_empty() {
            chunk
        } else {
            decoded.clear();
            decoded.extend_from_slice(&carry);
            decoded.extend_from_slice(chunk);
            &decoded
        };
        let (text, pending) = match std::str::from_utf8(slice) {
            Ok(text) => (text, &[][..]),
            Err(error) if error.error_len().is_none() => (
                std::str::from_utf8(&slice[..error.valid_up_to()])
                    .map_err(|_| failure("Invalid UTF-8 prefix"))?,
                &slice[error.valid_up_to()..],
            ),
            Err(_) => return Err(failure(format!("{} is not UTF-8 text", path.display()))),
        };
        if current == LineAction::Collect && !extra_characters {
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
        }
        carry.clear();
        carry.extend_from_slice(pending);
        has_bytes |= length > 0;
        if newline.is_some() {
            if !carry.is_empty() {
                return Err(failure(format!("{} is not UTF-8 text", path.display())));
            }
            total += 1;
            let continue_page = if current == LineAction::Collect {
                visit(
                    total,
                    if extra_characters {
                        &line
                    } else {
                        line.strip_suffix('\r').unwrap_or(&line)
                    },
                )
            } else {
                true
            };
            reader.consume(length + 1);
            if !continue_page {
                return count_remaining_lines(path, reader, cancel)
                    .map(|remaining| total + remaining);
            }
            current = action(total + 1);
            if current == LineAction::Stop {
                return count_remaining_lines(path, reader, cancel)
                    .map(|remaining| total + remaining);
            }
            line.clear();
            characters = 0;
            extra_characters = false;
            has_bytes = false;
        } else {
            reader.consume(length);
        }
    }
}

// The remaining bytes need validation and a line count, but no per-line
// decoding, copying or character counts once the page is full.
fn count_remaining_lines(
    path: &Path,
    mut reader: impl BufRead,
    cancel: &CancellationToken,
) -> Result<usize, OperationError> {
    let invalid = || failure(format!("{} is not UTF-8 text", path.display()));
    let mut carry = Vec::new();
    let mut joined = Vec::new();
    let mut lines = 0;
    let mut partial_line = false;
    loop {
        if cancel.is_cancelled() {
            return Err(failure("File scan cancelled"));
        }
        let buffer = reader
            .fill_buf()
            .map_err(|error| failure(format!("Cannot read {}: {error}", path.display())))?;
        let Some(last) = buffer.last() else {
            return if carry.is_empty() {
                Ok(lines + usize::from(partial_line))
            } else {
                Err(invalid())
            };
        };
        if buffer.contains(&0) {
            return Err(failure(format!(
                "{} is binary; use a tool appropriate for its format",
                path.display()
            )));
        }
        let bytes = if carry.is_empty() {
            buffer
        } else {
            joined.clear();
            joined.extend_from_slice(&carry);
            joined.extend_from_slice(buffer);
            &joined
        };
        carry.clear();
        match std::str::from_utf8(bytes) {
            Ok(_) => {}
            Err(error) if error.error_len().is_none() => {
                carry.extend_from_slice(&bytes[error.valid_up_to()..]);
            }
            Err(_) => return Err(invalid()),
        }
        lines += buffer.iter().filter(|&&byte| byte == b'\n').count();
        partial_line = *last != b'\n';
        let len = buffer.len();
        reader.consume(len);
    }
}

pub fn media_type(bytes: &[u8]) -> Option<&'static str> {
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

pub fn text_file(path: &Path) -> Result<String, OperationError> {
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

pub fn prepare_edit(
    path: &Path,
    input: &super::changes::EditInput,
) -> Result<(String, String, usize), OperationError> {
    let old = &input.old_string;
    let new = &input.new_string;
    if old == new {
        return Err(failure("oldString and newString are identical"));
    }
    if old.is_empty() {
        return Err(failure(
            "oldString must not be empty; use write to create or replace a file",
        ));
    }
    let original = text_file(path)?;
    let content = original.replace("\r\n", "\n");
    let old = old.replace("\r\n", "\n");
    let normalized_new = new.replace("\r\n", "\n");
    let count = content.matches(old.as_str()).count();
    if count == 0 {
        if !normalized_new.is_empty() && content.contains(&normalized_new) {
            return Err(failure(
                "oldString was not found; the requested change is already present",
            ));
        }
        let target = old
            .lines()
            .find(|line| !line.trim().is_empty())
            .unwrap_or(&old);
        let words: std::collections::HashSet<_> = target.split_whitespace().collect();
        let closest = content.lines().enumerate().max_by_key(|(_, line)| {
            line.split_whitespace()
                .filter(|word| words.contains(word))
                .count()
        });
        let hint = closest
            .map(|(index, line)| format!(" Closest line {}: {line}", index + 1))
            .unwrap_or_default();
        return Err(failure(format!(
            "oldString was not found in the file.{hint}"
        )));
    }
    let all = input.replace_all;
    if count > 1 && !all {
        return Err(failure(format!(
            "oldString matches {count} locations; supply more context or set replaceAll"
        )));
    }
    let has_crlf = original.contains("\r\n");
    let all_crlf = has_crlf
        && original
            .match_indices('\n')
            .all(|(index, _)| index > 0 && original.as_bytes()[index - 1] == b'\r');
    let replacement = if all_crlf {
        normalized_new.replace('\n', "\r\n")
    } else if has_crlf {
        new.clone()
    } else {
        normalized_new
    };
    // Match against normalized text, then splice the original bytes so an
    // edit never normalizes unrelated lines in a mixed-ending file.
    let mut crs = original.match_indices("\r\n").peekable();
    let mut skipped = 0;
    let mut original_offset = |offset| {
        while crs
            .peek()
            .is_some_and(|(index, _)| *index < offset + skipped)
        {
            crs.next();
            skipped += 1;
        }
        offset + skipped
    };
    let mut changed = String::with_capacity(original.len());
    let mut previous = 0;
    for (offset, _) in content
        .match_indices(&old)
        .take(if all { count } else { 1 })
    {
        let start = original_offset(offset);
        let end = original_offset(offset + old.len());
        changed.push_str(&original[previous..start]);
        changed.push_str(&replacement);
        previous = end;
    }
    changed.push_str(&original[previous..]);
    Ok((original, changed, count))
}

pub fn atomic_write(path: &Path, content: &[u8]) -> Result<(), String> {
    atomic_write_with_permissions(path, content, None)
}

pub fn atomic_write_with_permissions(
    path: &Path,
    content: &[u8],
    explicit_permissions: Option<&std::fs::Permissions>,
) -> Result<(), String> {
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
    let permissions = explicit_permissions.cloned().or_else(|| {
        std::fs::metadata(&target)
            .ok()
            .map(|meta| meta.permissions())
    });
    let mut builder = tempfile::Builder::new();
    builder.prefix(".ocmini-");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let initial_mode = if permissions.is_some() { 0o600 } else { 0o666 };
        builder.permissions(std::fs::Permissions::from_mode(initial_mode));
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

fn output(text: String, path: &Path, metadata: Value) -> Result<ToolOutput, OperationError> {
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
    fn read_disjoint_ranges_once_and_validate_the_whole_file() {
        let directory = Directory::new();
        let path = directory.0.path().join("ranges.txt");
        std::fs::write(&path, "one\ntwo\nthree\nfour\nfive\n").unwrap();
        let input =
            json!({"filePath":"ranges.txt","ranges":[{"start":4,"end":5},{"start":2,"end":4}]});
        let result = run(directory.dir(), "read", &input, &CancellationToken::new()).unwrap();
        assert_eq!(result.output, "2: two\n3: three\n4: four\n5: five\n");
        assert_eq!(result.metadata["totalLines"], 5);
        assert!(run(
            directory.dir(),
            "read",
            &json!({"filePath":"ranges.txt","ranges":[{"start":0,"end":1}]}),
            &CancellationToken::new()
        )
        .is_err());
        std::fs::write(path, b"one\ntwo\n\xff").unwrap();
        assert!(run(
            directory.dir(),
            "read",
            &json!({"filePath":"ranges.txt","ranges":[{"start":1,"end":1}]}),
            &CancellationToken::new()
        )
        .is_err());
    }

    #[test]
    fn range_pagination_and_missing_lines_are_actionable() {
        let directory = Directory::new();
        let path = directory.0.path().join("ranges.txt");
        std::fs::write(
            &path,
            format!("{}tail\n", format!("{}\n", "x".repeat(2000)).repeat(40)),
        )
        .unwrap();
        let result = run(directory.dir(), "read", &json!({"filePath":"ranges.txt", "ranges":[{"start": 1, "end": 15}, {"start": 25, "end": 45}, {"start": 50, "end": 51}]}), &CancellationToken::new()).unwrap();
        let next = result.metadata["nextLine"].as_u64().unwrap();
        assert!((25..=40).contains(&next));
        assert!(result
            .output
            .contains(&format!("continue requested ranges from line {next}")));
        assert_eq!(result.metadata["totalLines"], 41);
        assert_eq!(
            result.metadata["missingRanges"],
            json!([{"start":42,"end":45},{"start":50,"end":51}])
        );
        let resumed = run(
            directory.dir(),
            "read",
            &json!({"filePath":"ranges.txt", "ranges":[{"start": next, "end": 45}]}),
            &CancellationToken::new(),
        )
        .unwrap();
        assert!(resumed.output.starts_with(&format!("{next}: ")));
        std::fs::write(&path, "").unwrap();
        let empty = run(
            directory.dir(),
            "read",
            &json!({"filePath":"ranges.txt", "ranges":[{"start": 2, "end": 3}]}),
            &CancellationToken::new(),
        )
        .unwrap();
        assert_eq!(
            empty.metadata["missingRanges"],
            json!([{"start":2,"end":3}])
        );
        assert_eq!(empty.metadata["truncated"], false);
        for ranges in [json!([]), json!([{"start":2,"end":1}])] {
            assert!(run(
                directory.dir(),
                "read",
                &json!({"filePath":"ranges.txt", "ranges":ranges}),
                &CancellationToken::new()
            )
            .is_err());
        }
        assert!(run(
            directory.dir(),
            "read",
            &json!({"filePath":"ranges.txt", "ranges":[{"start":1,"end":1}], "offset":1}),
            &CancellationToken::new()
        )
        .is_err());
        std::fs::write(&path, "a\nb\nc\n").unwrap();
        let overlapped = run(
            directory.dir(),
            "read",
            &json!({"filePath":"ranges.txt", "ranges":[{"start":1,"end":10},{"start":5,"end":15},{"start":20,"end":22}]}),
            &CancellationToken::new(),
        )
        .unwrap();
        assert_eq!(
            overlapped.output.lines().take(3).collect::<Vec<_>>(),
            ["1: a", "2: b", "3: c"]
        );
        assert_eq!(
            overlapped.metadata["missingRanges"],
            json!([{"start":4,"end":15},{"start":20,"end":22}])
        );
    }

    #[test]
    fn range_page_counts_numbered_lines_and_leaves_space_for_continuation() {
        let directory = Directory::new();
        std::fs::write(directory.0.path().join("short.txt"), "x\n".repeat(20_000)).unwrap();
        let result = run(
            directory.dir(),
            "read",
            &json!({"filePath":"short.txt","ranges":[{"start":1,"end":20_001}]}),
            &CancellationToken::new(),
        )
        .unwrap();
        assert!(result.output.len() <= MAX_OUTPUT);
        let next = result.metadata["nextLine"].as_u64().unwrap();
        assert!(result.output.contains(&format!("{}: x\n", next - 1)));
        assert!(!result.output.contains(&format!("{next}: x\n")));
        assert!(result
            .output
            .contains(&format!("continue requested ranges from line {next}")));
        assert!(result.output.contains("End of file after line 20000"));
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
            &CancellationToken::new(),
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
            &CancellationToken::new(),
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
            &CancellationToken::new(),
        )
        .unwrap();
        assert!(result.output.contains("continue with offset 18"));
        assert!(!result.output.contains("19: "));
        let next = run(
            directory.dir(),
            "read",
            &json!({"filePath":"page.txt","offset":18,"limit":1}),
            &CancellationToken::new(),
        )
        .unwrap();
        assert!(next.output.starts_with("18: "));
        assert!(run(
            directory.dir(),
            "read",
            &json!({"filePath":"page.txt","offset":0}),
            &CancellationToken::new(),
        )
        .is_err());
        assert!(run(
            directory.dir(),
            "read",
            &json!({"filePath":"page.txt","limit":0}),
            &CancellationToken::new(),
        )
        .is_err());
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
            &CancellationToken::new(),
            |_, line| {
                lines.push(line.to_string());
                true
            },
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
        let flag = CancellationToken::new();
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
    fn mixed_ending_edit_preserves_untouched_bytes() {
        let directory = Directory::new();
        let path = directory.0.path().join("file");
        std::fs::write(&path, "α\r\nold\nlast\r\nold\r\nend").unwrap();
        run(
            directory.dir(),
            "edit",
            &json!({"filePath":"file","oldString":"old","newString":"new","replaceAll":true}),
            &CancellationToken::new(),
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(path).unwrap(),
            "α\r\nnew\nlast\r\nnew\r\nend"
        );
    }

    #[test]
    fn ambiguous_edit_leaves_file_unchanged() {
        let directory = Directory::new();
        let path = directory.0.path().join("file");
        std::fs::write(&path, "same same").unwrap();
        assert!(run(
            directory.dir(),
            "edit",
            &json!({"filePath":"file","oldString":"same","newString":"new"}),
            &CancellationToken::new()
        )
        .is_err());
        assert_eq!(std::fs::read_to_string(path).unwrap(), "same same");
    }

    #[test]
    fn streamed_reads_preserve_line_endings_unicode_offsets_and_counts() {
        let directory = Directory::new();
        let path = directory.0.path().join("lines.txt");
        let flag = CancellationToken::new();
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
        let flag = CancellationToken::new();
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
        let flag = CancellationToken::new();
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
        let flag = CancellationToken::new();
        let result = scan_text(
            Path::new("fixture"),
            std::io::Cursor::new(b"first\nsecond\n"),
            &flag,
            |_, _| {
                flag.cancel();
                true
            },
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
            &CancellationToken::new(),
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

    #[test]
    fn fast_skip_validates_skipped_lines_for_binary_and_utf8() {
        let directory = Directory::new();
        std::fs::write(
            directory.0.path().join("nul_prefix.txt"),
            b"bad\0line\nok\nok2\n",
        )
        .unwrap();
        let err = run(
            directory.dir(),
            "read",
            &json!({"filePath":"nul_prefix.txt","offset":2,"limit":2}),
            &CancellationToken::new(),
        )
        .unwrap_err();
        assert!(err.message.contains("binary"));

        std::fs::write(
            directory.0.path().join("bad_utf8_prefix.txt"),
            b"\xff\xfe\nok\nok2\n",
        )
        .unwrap();
        let err = run(
            directory.dir(),
            "read",
            &json!({"filePath":"bad_utf8_prefix.txt","offset":2,"limit":2}),
            &CancellationToken::new(),
        )
        .unwrap_err();
        assert!(err.message.contains("UTF-8"));

        std::fs::write(
            directory.0.path().join("many.txt"),
            (1..=20).map(|i| format!("line-{i}\n")).collect::<String>(),
        )
        .unwrap();
        let out = run(
            directory.dir(),
            "read",
            &json!({"filePath":"many.txt","ranges":[{"start":5,"end":6},{"start":15,"end":15}]}),
            &CancellationToken::new(),
        )
        .unwrap();
        assert!(out.output.contains("5: line-5"));
        assert!(out.output.contains("6: line-6"));
        assert!(out.output.contains("15: line-15"));
        assert!(!out.output.contains("4: line-4"));
        assert_eq!(out.metadata["totalLines"], 20);
    }

    #[test]
    fn read_accepts_path_alias_for_file_path() {
        let directory = Directory::new();
        std::fs::write(directory.0.path().join("alias.txt"), "hello\nworld\n").unwrap();
        let result = run(
            directory.dir(),
            "read",
            &json!({"path": "alias.txt", "offset": 2, "limit": 1}),
            &CancellationToken::new(),
        )
        .unwrap();
        assert_eq!(result.output, "2: world\n");
    }

    #[cfg(unix)]
    #[test]
    fn directory_listing_distinguishes_symlinks() {
        use std::os::unix::fs::symlink;
        let directory = Directory::new();
        std::fs::write(directory.0.path().join("file.txt"), "hi").unwrap();
        std::fs::create_dir(directory.0.path().join("sub")).unwrap();
        symlink("file.txt", directory.0.path().join("link_file")).unwrap();
        symlink("sub", directory.0.path().join("link_dir")).unwrap();
        symlink("missing", directory.0.path().join("link_broken")).unwrap();

        let result = run(
            directory.dir(),
            "read",
            &json!({"path": "."}),
            &CancellationToken::new(),
        )
        .unwrap();
        let lines: Vec<_> = result.output.lines().collect();
        assert_eq!(
            lines,
            vec![
                "file.txt",
                "link_broken@ -> missing",
                "link_dir@ -> sub/",
                "link_file@ -> file.txt",
                "sub/",
            ]
        );
    }
}
