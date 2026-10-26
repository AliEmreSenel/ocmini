use super::input::{terminal_width, Display, Draft};
use super::{RESET, YELLOW};
use crate::attachments::Attachment;
use std::path::{Path, PathBuf};

pub(super) fn file_attachment(path: &Path) -> Result<Attachment, String> {
    let attachment = crate::attachments::from_file(path)?;
    if !attachment.mime.starts_with("image/") {
        return Err(format!(
            "{} is not a PNG, JPEG, GIF or WebP image",
            path.display()
        ));
    }
    Ok(attachment)
}

/// Retain supported clipboard images in the message itself, without temp files.
pub(super) fn image_attachment(bytes: Vec<u8>) -> Option<Attachment> {
    let mime = crate::tools::media_type(&bytes).filter(|mime| mime.starts_with("image/"))?;
    let ext = mime.rsplit('/').next().unwrap_or("png");
    Some(Attachment {
        filename: format!("clipboard.{ext}"),
        url: crate::attachments::data_url(mime, &bytes).ok()?,
        mime,
    })
}

/// A paste that is only a quoted, file: or ~/ path to an image file. Many
/// terminals paste dragged or copied files as paths, so those become
/// attachments; anything else stays plain text.
pub(super) fn pasted_image_path(text: &str) -> Option<PathBuf> {
    let trimmed = text.trim();
    if trimmed.contains(['\n', '\r']) {
        return None;
    }
    let unquoted = trimmed
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .or_else(|| {
            trimmed
                .strip_prefix('\'')
                .and_then(|s| s.strip_suffix('\''))
        })
        .unwrap_or(trimmed);
    let path = if unquoted.starts_with("file:") {
        reqwest::Url::parse(unquoted).ok()?.to_file_path().ok()?
    } else {
        PathBuf::from(unquoted.replace("\\ ", " "))
    };
    let expanded = match path.to_str()?.strip_prefix("~/") {
        Some(rest) => std::env::var_os("HOME").map(|home| PathBuf::from(home).join(rest))?,
        None => path,
    };
    is_image_file(&expanded).then_some(expanded)
}

// Sniff only the header: pasting a path to a huge file must not read it all.
pub(super) fn is_image_file(path: &Path) -> bool {
    use std::io::Read;
    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };
    if !file.metadata().is_ok_and(|m| m.is_file()) {
        return false;
    }
    let mut head = [0u8; 16];
    let read = file.read(&mut head).unwrap_or(0);
    crate::tools::media_type(&head[..read]).is_some_and(|mime| mime.starts_with("image/"))
}

pub(super) async fn paste_clipboard(draft: &mut Draft, display: &mut Display) {
    let result = tokio::time::timeout(std::time::Duration::from_secs(3), clipboard_image()).await;
    let message = match result {
        Ok(Ok(Some(attachment))) => {
            draft.attachments.push(attachment);
            return;
        }
        Ok(Ok(None)) => {
            "Clipboard has no supported image. Paste an image file path to attach it.".into()
        }
        Ok(Err(message)) => message,
        Err(_) => "Clipboard read timed out. Paste an image file path to attach it.".into(),
    };
    display.output(&format!("\n{YELLOW}{message}{RESET}\n"), terminal_width());
}

#[cfg(target_os = "linux")]
pub(super) async fn clipboard_image() -> Result<Option<Attachment>, String> {
    use tokio::process::Command;
    let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();
    let x11 = std::env::var_os("DISPLAY").is_some();
    if !wayland && !x11 {
        return Err(
            if std::env::var_os("SSH_CONNECTION").is_some() || std::env::var_os("SSH_TTY").is_some()
            {
                "SSH does not forward your local image clipboard. Copy the image to this host (for example with scp), then paste its remote path; X11 clipboard forwarding also works with a forwarded DISPLAY and xclip.".into()
            } else {
                "No desktop clipboard is available. Paste an image file path, or use wl-paste (Wayland) / xclip (X11) in a desktop session.".into()
            },
        );
    }
    let mut available = false;
    for (program, enabled) in [("wl-paste", wayland), ("xclip", x11)] {
        if !enabled {
            continue;
        }
        for mime in [
            "image/png",
            "image/jpeg",
            "image/webp",
            "image/gif",
            "text/uri-list",
        ] {
            let mut command = Command::new(program);
            if program == "wl-paste" {
                // --no-newline keeps binary payloads byte-exact.
                command.args(["--type", mime, "--no-newline"]);
            } else {
                command.args(["-selection", "clipboard", "-t", mime, "-o"]);
            }
            match command.kill_on_drop(true).output().await {
                Ok(out) => {
                    available = true;
                    if out.status.success() {
                        if mime == "text/uri-list" {
                            if let Ok(text) = std::str::from_utf8(&out.stdout) {
                                let paths: Vec<_> = text
                                    .lines()
                                    .filter(|line| {
                                        !line.starts_with('#') && !line.trim().is_empty()
                                    })
                                    .collect();
                                if let [path] = paths.as_slice() {
                                    if let Some(path) = pasted_image_path(path) {
                                        return file_attachment(&path).map(Some);
                                    }
                                }
                            }
                        } else if let Some(attachment) = image_attachment(out.stdout) {
                            return Ok(Some(attachment));
                        }
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
                Err(error) => {
                    return Err(format!("Cannot read clipboard using {program}: {error}"))
                }
            }
        }
    }
    if !available {
        return Err("Image paste needs wl-paste (Wayland) or xclip (X11); neither helper could be started. You can also paste an image file path.".into());
    }
    Ok(None)
}

#[cfg(target_os = "macos")]
pub(super) async fn clipboard_image() -> Result<Option<Attachment>, String> {
    use tokio::process::Command;
    let directory = tempfile::tempdir().map_err(|e| e.to_string())?;
    let file = directory.path().join("clipboard.png");
    let out = Command::new("osascript")
        .args([
            "-e",
            &format!(
                "set f to open for access POSIX file \"{}\" with write permission",
                file.display()
            ),
            "-e",
            "write (the clipboard as «class PNGf») to f starting at 1",
            "-e",
            "close access f",
        ])
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|e| format!("Cannot run osascript: {e}"))?;
    let bytes = std::fs::read(&file).ok();
    let _ = std::fs::remove_file(&file);
    if !out.status.success() {
        return Ok(None);
    }
    Ok(bytes.and_then(image_attachment))
}

#[cfg(windows)]
pub(super) async fn clipboard_image() -> Result<Option<Attachment>, String> {
    use tokio::process::Command;
    let directory = tempfile::tempdir().map_err(|e| e.to_string())?;
    let file = directory.path().join("clipboard.png");
    let script = format!(
        "Add-Type -AssemblyName System.Windows.Forms; if ($i = [System.Windows.Forms.Clipboard]::GetImage()) {{ $i.Save('{}') }}",
        file.display()
    );
    let out = Command::new("powershell")
        .args(["-NoProfile", "-Command", &script])
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|e| format!("Cannot run powershell: {e}"))?;
    let bytes = std::fs::read(&file).ok();
    let _ = std::fs::remove_file(&file);
    if !out.status.success() {
        return Ok(None);
    }
    Ok(bytes.and_then(image_attachment))
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
pub(super) async fn clipboard_image() -> Result<Option<Attachment>, String> {
    Err("Image paste is not supported on this platform".into())
}
