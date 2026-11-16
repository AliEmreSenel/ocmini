//! Attachment admission snapshots local data; later requests reuse the saved
//! bytes instead of depending on a mutable file path.
use base64::Engine as _;
use serde_json::{json, Value};
use std::io::Read;
use std::path::Path;

pub const MAX_BYTES: u64 = 20 * 1024 * 1024;
pub struct Attachment {
    pub filename: String,
    pub url: String,
    pub mime: &'static str,
}
impl Attachment {
    pub fn part(&self) -> Value {
        json!({"type":"file","filename":self.filename,"url":self.url,"mime":self.mime})
    }
}

pub fn from_file(path: &Path) -> Result<Attachment, String> {
    let path = path
        .canonicalize()
        .map_err(|e| format!("Cannot read {}: {e}", path.display()))?;
    let mut file = std::fs::File::open(&path).map_err(|e| e.to_string())?;
    if file.metadata().map_err(|e| e.to_string())?.len() > MAX_BYTES {
        return Err("Attachment exceeds 20 MiB".into());
    }
    let mut prefix = [0; 512];
    let count = file.read(&mut prefix).map_err(|e| e.to_string())?;
    let mime = crate::tools::media_type(&prefix[..count]).unwrap_or_else(|| {
        match path
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase()
            .as_str()
        {
            "json" => "application/json",
            "xml" => "application/xml",
            "js" => "application/javascript",
            "yaml" | "yml" => "application/x-yaml",
            "txt" | "md" | "markdown" | "rs" | "py" | "ts" | "tsx" | "jsx" | "toml" | "html"
            | "css" | "sh" | "c" | "cpp" | "h" | "go" | "java" => "text/plain",
            _ => "application/octet-stream",
        }
    });
    Ok(Attachment {
        filename: path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned(),
        url: reqwest::Url::from_file_path(&path)
            .map_err(|_| "Invalid local attachment path")?
            .to_string(),
        mime,
    })
}

/// Local snapshots and clipboard media use the same inline encoding and byte limit.
pub fn data_url(mime: &str, bytes: &[u8]) -> Result<String, String> {
    if bytes.len() as u64 > MAX_BYTES {
        return Err("Attachment exceeds 20 MiB".into());
    }
    Ok(format!(
        "data:{mime};base64,{}",
        base64::engine::general_purpose::STANDARD.encode(bytes)
    ))
}

pub fn snapshot(part: &mut Value) -> Result<(), String> {
    snapshot_file(part)?;
    let url = part["url"].as_str().ok_or("File part has no URL")?;
    if url.starts_with("data:") && url.len() as u64 > MAX_BYTES.div_ceil(3) * 4 + 1024 {
        return Err("Attachment exceeds 20 MiB".into());
    }
    // Validate the same URL/content rules used to build provider history.
    content(part).map(|_| ())
}
fn snapshot_file(part: &mut Value) -> Result<(), String> {
    let url = part["url"].as_str().ok_or("File part has no URL")?;
    if !url.starts_with("file:") {
        return Ok(());
    }
    let path = reqwest::Url::parse(url)
        .map_err(|e| format!("Invalid file URL: {e}"))?
        .to_file_path()
        .map_err(|_| "Invalid local file URL")?;
    let file = std::fs::File::open(&path)
        .map_err(|e| format!("Cannot read attachment {}: {e}", path.display()))?;
    let mut bytes = Vec::new();
    file.take(MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    let mime = part["mime"].as_str().ok_or("File part has no MIME type")?;
    let snapshot = data_url(mime, &bytes)?;
    part["ocminiSourceURL"] = json!(url);
    part["url"] = json!(snapshot);
    Ok(())
}

pub fn content(part: &Value) -> Result<Value, String> {
    let mime = part["mime"].as_str().ok_or("File part has no MIME type")?;
    let filename = part["filename"].as_str().unwrap_or("attachment");
    let mut snapshot = std::borrow::Cow::Borrowed(part);
    if part["url"]
        .as_str()
        .is_some_and(|url| url.starts_with("file:"))
    {
        snapshot_file(snapshot.to_mut())?;
    }
    let url = snapshot["url"].as_str().ok_or("File part has no URL")?;
    if mime.starts_with("image/") {
        return Ok(json!({"type":"image_url","image_url":{"url":url}}));
    }
    if (mime.starts_with("text/")
        || [
            "application/json",
            "application/xml",
            "application/javascript",
            "application/x-yaml",
        ]
        .contains(&mime))
        && url.starts_with("data:")
    {
        let (header, data) = url.split_once(',').ok_or("Malformed attachment data URL")?;
        let bytes = if header.ends_with(";base64") {
            base64::engine::general_purpose::STANDARD
                .decode(data)
                .map_err(|e| format!("Invalid attachment base64: {e}"))?
        } else {
            crate::http::percent_bytes(data)?
        };
        if bytes.len() > MAX_BYTES as usize {
            return Err("Attachment exceeds 20 MiB".into());
        }
        let text = String::from_utf8(bytes).map_err(|_| "Text attachment is not UTF-8")?;
        return Ok(json!({"type":"text","text":format!("Attachment {filename}:\n{text}")}));
    }
    if !url.starts_with("data:") && !url.starts_with("https:") && !url.starts_with("http:") {
        return Err("Attachment URL must use data:, file:, http: or https:".into());
    }
    Ok(json!({"type":"file","file":{"file_data":url,"filename":filename}}))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn admission_preserves_local_input_after_modification_and_deletion() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("context.txt");
        std::fs::write(&path, "original context").unwrap();
        let mut part = from_file(&path).unwrap().part();
        let source = part["url"].clone();
        snapshot(&mut part).unwrap();
        std::fs::write(&path, "changed context").unwrap();
        std::fs::remove_file(&path).unwrap();
        assert_eq!(part["ocminiSourceURL"], source);
        assert!(content(&part).unwrap()["text"]
            .as_str()
            .unwrap()
            .ends_with("original context"));
    }
    #[test]
    fn file_size_is_checked_before_reading_and_inline_images_are_bounded() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("huge.png");
        std::fs::File::create(&path)
            .unwrap()
            .set_len(MAX_BYTES + 1)
            .unwrap();
        assert!(from_file(&path).err().unwrap().contains("20 MiB"));
        let mut part = json!({"type":"file","mime":"image/png","url":format!("data:image/png;base64,{}", "A".repeat((MAX_BYTES * 2) as usize))});
        assert!(snapshot(&mut part).unwrap_err().contains("20 MiB"));
    }
}
