use super::process::ProcessError;
use serde::{Deserialize, Serialize};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use tempfile::TempDir;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PayloadMode {
    #[default]
    Inline,
    File,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PayloadRef {
    pub path: String,
    pub bytes: u64,
    pub content_type: String,
    pub lifetime: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileEnvelopeV2 {
    pub version: u32,
    pub payload: PayloadRef,
    #[serde(rename = "invocationID")]
    pub invocation_id: String,
}

#[derive(Debug)]
pub struct Files {
    pub(crate) temp_dir: TempDir,
    pub(crate) payload_path: PathBuf,
    pub(crate) response_path: PathBuf,
    pub(crate) payload_bytes: u64,
    pub(crate) invocation_id: String,
    #[cfg(unix)]
    pub(crate) directory_file: std::fs::File,
    #[cfg(unix)]
    _lease: std::fs::File,
}

impl Files {
    pub fn payload_path(&self) -> &Path {
        &self.payload_path
    }

    pub fn response_path(&self) -> &Path {
        &self.response_path
    }

    pub fn dir_path(&self) -> &Path {
        self.temp_dir.path()
    }

    pub fn v2_stdin_bytes(&self) -> Result<Vec<u8>, ProcessError> {
        let wrapper = FileEnvelopeV2 {
            version: 2,
            payload: PayloadRef {
                path: self.payload_path.to_string_lossy().into_owned(),
                bytes: self.payload_bytes,
                content_type: "application/json".into(),
                lifetime: "invocation".into(),
            },
            invocation_id: self.invocation_id.clone(),
        };
        serde_json::to_vec(&wrapper)
            .map_err(|e| ProcessError::payload(format!("cannot encode v2 stdin wrapper: {e}")))
    }
}

struct BoundedFileWriter<W: Write> {
    writer: W,
    bytes: usize,
    limit: usize,
}

impl<W: Write> Write for BoundedFileWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if buf.len() > self.limit.saturating_sub(self.bytes) {
            return Err(io::Error::other("hook payload exceeds byte budget"));
        }
        let n = self.writer.write(buf)?;
        self.bytes += n;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.writer.flush()
    }
}

pub fn spool<T: Serialize>(
    envelope: &T,
    max_payload_bytes: usize,
    invocation_id: &str,
) -> Result<Files, ProcessError> {
    let base = payload_base()?;
    cleanup_abandoned(&base);
    let temp_dir = tempfile::Builder::new()
        .prefix("invocation-")
        .tempdir_in(base)
        .map_err(|e| ProcessError::io(format!("cannot create hook temp directory: {e}")))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(temp_dir.path(), std::fs::Permissions::from_mode(0o700)).map_err(
            |e| ProcessError::io(format!("cannot restrict hook payload directory: {e}")),
        )?;
    }

    let payload_path = temp_dir.path().join("payload.json");
    let response_path = temp_dir.path().join("response.json");
    #[cfg(unix)]
    let lease = {
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::OpenOptionsExt;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(temp_dir.path().join("owner.lock"))
            .map_err(|e| ProcessError::io(format!("cannot create hook payload lease: {e}")))?;
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(ProcessError::io("cannot lock hook payload lease"));
        }
        file
    };

    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&payload_path)
        .map_err(|e| ProcessError::io(format!("cannot create hook payload file: {e}")))?;

    let mut writer = BoundedFileWriter {
        writer: std::io::BufWriter::new(file),
        bytes: 0,
        limit: max_payload_bytes,
    };

    serde_json::to_writer(&mut writer, envelope)
        .map_err(|e| ProcessError::payload(format!("cannot encode hook payload: {e}")))?;
    writer
        .flush()
        .map_err(|e| ProcessError::io(format!("cannot flush hook payload: {e}")))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        writer
            .writer
            .get_ref()
            .set_permissions(std::fs::Permissions::from_mode(0o400))
            .map_err(|e| ProcessError::io(format!("cannot restrict hook payload file: {e}")))?;
    }

    let payload_bytes = writer.bytes as u64;
    #[cfg(unix)]
    let directory_file = std::fs::File::open(temp_dir.path())
        .map_err(|e| ProcessError::io(format!("cannot retain hook directory: {e}")))?;

    Ok(Files {
        temp_dir,
        payload_path,
        response_path,
        payload_bytes,
        invocation_id: invocation_id.to_string(),
        #[cfg(unix)]
        directory_file,
        #[cfg(unix)]
        _lease: lease,
    })
}

fn private_directory(path: &Path) -> Result<(), ProcessError> {
    std::fs::create_dir_all(path)
        .map_err(|e| ProcessError::io(format!("cannot create hook storage directory: {e}")))?;
    let info = std::fs::symlink_metadata(path)
        .map_err(|e| ProcessError::io(format!("cannot inspect hook storage directory: {e}")))?;
    if !info.is_dir() || info.file_type().is_symlink() {
        return Err(ProcessError::io("hook storage must be a private directory"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if info.uid() != unsafe { libc::geteuid() } {
            return Err(ProcessError::io("hook storage has a different owner"));
        }
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).map_err(|e| {
            ProcessError::io(format!("cannot restrict hook storage directory: {e}"))
        })?;
    }
    Ok(())
}
fn payload_base() -> Result<PathBuf, ProcessError> {
    #[cfg(unix)]
    let owner = unsafe { libc::geteuid() };
    #[cfg(not(unix))]
    let owner = std::process::id();
    let path = std::env::temp_dir().join(format!("ocmini-hook-payloads-{owner}"));
    private_directory(&path)?;
    Ok(path)
}
fn cleanup_abandoned(base: &Path) {
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::OpenOptionsExt;
        let Ok(entries) = std::fs::read_dir(base) else {
            return;
        };
        for entry in entries.take(128).flatten() {
            if !entry
                .file_name()
                .to_string_lossy()
                .starts_with("invocation-")
                || !entry.file_type().is_ok_and(|t| t.is_dir())
            {
                continue;
            }
            let lock = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
                .open(entry.path().join("owner.lock"));
            let Ok(lock) = lock else {
                continue;
            };
            if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
    }
    #[cfg(not(unix))]
    let _ = base;
}

/// Private bounded diagnostics survive the invocation without copying stderr
/// into transcript errors or broad event payloads.
pub fn diagnostic_artifact(invocation: &str, tail: &[u8]) -> Result<Option<PathBuf>, ProcessError> {
    if tail.is_empty() {
        return Ok(None);
    }
    if !invocation
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_')
    {
        return Err(ProcessError::io("invalid diagnostic invocation ID"));
    }
    crate::blocking::run(|| {
        let base = Path::new(&crate::config::data_home()).join("ocmini/hook-diagnostics");
        private_directory(&base)?;
        #[cfg(unix)]
        let _lock = {
            use std::os::fd::AsRawFd;
            use std::os::unix::fs::OpenOptionsExt;
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
                .open(base.join("lock"))
                .map_err(|e| ProcessError::io(format!("cannot open diagnostic lock: {e}")))?;
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
                return Err(ProcessError::io("cannot lock diagnostic storage"));
            }
            file
        };
        let keep = tail.len().min(4 * 1024 * 1024);
        let mut files = std::fs::read_dir(&base)
            .map_err(|e| ProcessError::io(format!("cannot list diagnostics: {e}")))?
            .flatten()
            .filter_map(|e| {
                let info = std::fs::symlink_metadata(e.path()).ok()?;
                (info.is_file() && e.file_name().to_string_lossy().ends_with(".log")).then(|| {
                    (
                        info.modified().unwrap_or(std::time::UNIX_EPOCH),
                        info.len(),
                        e.path(),
                    )
                })
            })
            .collect::<Vec<_>>();
        files.sort_by_key(|e| e.0);
        let mut total: u64 = files.iter().map(|e| e.1).sum();
        let mut count = files.len();
        for (_, size, path) in files {
            if count < 32 && total.saturating_add(keep as u64) <= 4 * 1024 * 1024 {
                break;
            }
            std::fs::remove_file(path)
                .map_err(|e| ProcessError::io(format!("cannot expire hook diagnostic: {e}")))?;
            count -= 1;
            total = total.saturating_sub(size);
        }
        let mut file = tempfile::NamedTempFile::new_in(&base)
            .map_err(|e| ProcessError::io(format!("cannot create hook diagnostic: {e}")))?;
        file.write_all(&tail[tail.len() - keep..])
            .map_err(|e| ProcessError::io(format!("cannot write hook diagnostic: {e}")))?;
        let path = base.join(format!("{invocation}.log"));
        file.persist_noclobber(&path)
            .map_err(|e| ProcessError::io(format!("cannot retain hook diagnostic: {e}")))?;
        Ok(Some(path))
    })
}
