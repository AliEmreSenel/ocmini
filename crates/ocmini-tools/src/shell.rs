use super::{failure, output};
use crate::types::{OperationError, ToolOutput};
use serde_json::json;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::process::{ExitStatus, Stdio};
#[cfg(unix)]
use std::sync::Mutex;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

const MAX_CAPTURE: usize = 64 * 1024;

fn preview(file: &mut File) -> std::io::Result<(Vec<u8>, u64)> {
    let total = file.metadata()?.len();
    file.rewind()?;
    let mut bytes = Vec::new();
    let head = if total > MAX_CAPTURE as u64 {
        MAX_CAPTURE / 2
    } else {
        MAX_CAPTURE
    };
    Read::by_ref(file)
        .take(head as u64)
        .read_to_end(&mut bytes)?;
    if total > MAX_CAPTURE as u64 {
        file.seek(SeekFrom::End(-(MAX_CAPTURE as i64 / 2)))?;
        Read::by_ref(file)
            .take(MAX_CAPTURE as u64 / 2)
            .read_to_end(&mut bytes)?;
    }
    file.rewind()?;
    Ok((bytes, total))
}

fn exit_annotation(status: &ExitStatus) -> String {
    if status.success() {
        String::new()
    } else {
        format!(
            "\n[exit code: {}]",
            status
                .code()
                .map_or_else(|| "terminated by signal".into(), |v| v.to_string())
        )
    }
}

fn finish(
    mut stdout: File,
    mut stderr: File,
    title: String,
    exit: Option<i32>,
    annotation: String,
    directory: &Path,
) -> std::io::Result<ToolOutput> {
    let (out, stdout_total) = preview(&mut stdout)?;
    let (err, stderr_total) = preview(&mut stderr)?;
    let mut text = String::from_utf8_lossy(&out).into_owned();
    if !err.is_empty() {
        text.push_str("\n[stderr]\n");
        text.push_str(&String::from_utf8_lossy(&err));
    }
    if stdout_total + stderr_total > (out.len() + err.len()) as u64 {
        text.push_str(&format!(
            "\n[{} bytes omitted from captured process output]",
            stdout_total + stderr_total - (out.len() + err.len()) as u64
        ));
    }
    text.push_str(&annotation);
    let result = ToolOutput {
        title,
        output: text,
        metadata: json!({"exit":exit,"truncated":false,"outputBytes":stdout_total+stderr_total}),
    };
    if result.output.len() <= output::MAX_OUTPUT {
        return Ok(result);
    }
    let mut file = output::create(directory)?;
    std::io::copy(&mut stdout.take(stdout_total), &mut file)?;
    if stderr_total > 0 {
        file.write_all(b"\n[stderr]\n")?;
        std::io::copy(&mut stderr.take(stderr_total), &mut file)?;
    }
    file.write_all(annotation.as_bytes())?;
    let path = file.into_temp_path().keep().map_err(std::io::Error::from)?;
    Ok(output::with_file(result, &path))
}

fn shell_command(command: &str, directory: &Path) -> tokio::process::Command {
    let mut process = tokio::process::Command::new(if cfg!(windows) { "cmd" } else { "bash" });
    if cfg!(windows) {
        process.args(["/C", command]);
    } else {
        process.args(["-c", command]);
    }
    process.current_dir(directory);
    process
}

fn spawn_failure(directory: &Path, error: std::io::Error) -> OperationError {
    failure(format!(
        "Could not start shell in {}: {error}",
        directory.display()
    ))
}

async fn stop(child: &mut tokio::process::Child) {
    let _ = child.kill().await;
    let _ = child.wait().await;
}

enum Outcome {
    Exited(ExitStatus),
    TimedOut,
    Cancelled,
}

#[cfg(test)]
pub async fn shell(
    command: &str,
    directory: &Path,
    timeout: Duration,
    cancel: CancellationToken,
    output_directory: &Path,
) -> Result<ToolOutput, OperationError> {
    shell_with_env(
        command,
        directory,
        timeout,
        cancel,
        output_directory,
        &serde_json::Map::new(),
    )
    .await
}

pub async fn shell_with_env(
    command: &str,
    directory: &Path,
    timeout: Duration,
    cancel: CancellationToken,
    output_directory: &Path,
    env: &serde_json::Map<String, serde_json::Value>,
) -> Result<ToolOutput, OperationError> {
    // Let the OS capture output directly; only bounded previews enter memory.
    let capture_dir = output_directory.to_owned();
    let (stdout, stderr, child_stdout, child_stderr) = tokio::task::spawn_blocking(move || {
        std::fs::create_dir_all(&capture_dir)
            .map_err(|e| failure(format!("Cannot capture shell output: {e}")))?;
        let stdout = tempfile::tempfile_in(&capture_dir)
            .map_err(|e| failure(format!("Cannot capture shell output: {e}")))?;
        let stderr = tempfile::tempfile_in(&capture_dir)
            .map_err(|e| failure(format!("Cannot capture shell output: {e}")))?;
        let child_stdout = stdout.try_clone().map_err(|e| failure(e.to_string()))?;
        let child_stderr = stderr.try_clone().map_err(|e| failure(e.to_string()))?;
        Ok::<_, OperationError>((stdout, stderr, child_stdout, child_stderr))
    })
    .await
    .map_err(|e| failure(format!("Shell setup task failed: {e}")))??;
    let mut process = shell_command(command, directory);
    for (key, value) in env {
        if let Some(value) = value.as_str() {
            process.env(key, value);
        } else {
            process.env_remove(key);
        }
    }
    process
        .stdin(Stdio::null())
        .stdout(child_stdout)
        .stderr(child_stderr)
        .kill_on_drop(true);
    #[cfg(unix)]
    process.process_group(0);
    let mut child = process.spawn().map_err(|e| spawn_failure(directory, e))?;
    let group = crate::process_group::Group::new(
        child
            .id()
            .ok_or_else(|| failure("Shell has no process id"))?,
    );
    let outcome = tokio::select! {
        result = group.wait(&mut child) => result.map(Outcome::Exited).map_err(|e| failure(format!("Shell wait failed: {e}")))?,
        _ = tokio::time::sleep(timeout) => Outcome::TimedOut,
        _ = cancel.cancelled() => Outcome::Cancelled,
    };
    // Stop descendants before reading files they inherited from the shell.
    drop(group);
    let timed_out = matches!(outcome, Outcome::TimedOut);
    let (exit, annotation) = match outcome {
        Outcome::Exited(status) => (status.code(), exit_annotation(&status)),
        Outcome::TimedOut => {
            stop(&mut child).await;
            (
                None,
                format!(
                    "\n\n<shell_metadata>\nshell tool terminated command after exceeding timeout {} ms. If this command is expected to take longer, retry with a larger timeout value in milliseconds.\n</shell_metadata>",
                    timeout.as_millis()
                ),
            )
        }
        // Aborting a turn is a request to drop the work, so the anonymous
        // captures disappear without publishing a partial result.
        Outcome::Cancelled => {
            stop(&mut child).await;
            return Err(failure("Shell command cancelled"));
        }
    };
    let title = command.chars().take(80).collect();
    let directory = output_directory.to_owned();
    let result = tokio::task::spawn_blocking(move || {
        finish(stdout, stderr, title, exit, annotation, &directory)
    })
    .await
    .map_err(|e| failure(format!("Shell output task failed: {e}")))?
    .map_err(|e| failure(format!("Cannot read or save shell output: {e}")))?;
    if timed_out {
        // OpenCode reports a command that overran its timeout as a completed
        // call carrying what it printed, with the timeout in shell_metadata;
        // the captured text is the only record of the work the limit cut short.
        let mut result = result;
        if result.output.starts_with("\n\n<shell_metadata>") {
            result.output.insert_str(0, "(no output)");
        }
        result.metadata["timeout"] = true.into();
        return Ok(result);
    }
    Ok(result)
}

/// Background jobs are `(session id, pid)`. They outlive the tool call that
/// started them, so they need an owner: the session that started them ends them
/// when it is aborted or deleted, and the runtime ends the rest when it exits.
#[cfg(unix)]
static BACKGROUND: Mutex<Vec<BackgroundJob>> = Mutex::new(Vec::new());

#[cfg(unix)]
const EXITED_JOB_TTL: Duration = Duration::from_secs(24 * 60 * 60);

#[cfg(unix)]
struct BackgroundJob {
    session: String,
    id: String,
    pid: u32,
    log: Option<File>,
    path: std::path::PathBuf,
    identity: LogIdentity,
    // None is running; Some(None) is an observed termination without an exit code.
    exit: Option<Option<i32>>,
    exited_at: Option<std::time::Instant>,
}

#[cfg(unix)]
fn prune_exited(jobs: &mut Vec<BackgroundJob>, now: std::time::Instant, ttl: Duration) {
    jobs.retain(|job| {
        job.exited_at
            .is_none_or(|exited_at| now.saturating_duration_since(exited_at) < ttl)
    });
}

#[cfg(unix)]
#[derive(Clone, Copy)]
struct LogIdentity {
    dev: u64,
    ino: u64,
    created: Option<std::time::SystemTime>,
}

#[cfg(unix)]
impl LogIdentity {
    fn from_metadata(m: &std::fs::Metadata) -> Self {
        use std::os::unix::fs::MetadataExt;
        Self {
            dev: m.dev(),
            ino: m.ino(),
            created: m.created().ok(),
        }
    }
    fn matches(self, m: &std::fs::Metadata) -> bool {
        let other = Self::from_metadata(m);
        m.is_file()
            && self.dev == other.dev
            && self.ino == other.ino
            && self.created == other.created
    }
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct BackgroundReadInput {
    pub job_id: String,
    #[serde(default)]
    pub offset: u64,
    pub limit: Option<u64>,
}

pub fn read_background(
    session: &str,
    input: &BackgroundReadInput,
) -> Result<ToolOutput, OperationError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{FileExt, OpenOptionsExt};
        let (id, exit, handle, path, identity) = {
            let jobs = BACKGROUND.lock().map_err(|e| failure(e.to_string()))?;
            let job = jobs
                .iter()
                .find(|job| job.session == session && job.id == input.job_id)
                .ok_or_else(|| failure("Background job not found in this session"))?;
            (
                job.id.clone(),
                job.exit,
                job.log
                    .as_ref()
                    .map(File::try_clone)
                    .transpose()
                    .map_err(|e| failure(e.to_string()))?,
                job.path.clone(),
                job.identity,
            )
        };
        let file = match handle {
            Some(file) => file,
            None => {
                let file = std::fs::OpenOptions::new()
                    .read(true)
                    .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
                    .open(path)
                    .map_err(|e| failure(format!("Cannot reopen background output: {e}")))?;
                if !identity.matches(&file.metadata().map_err(|e| failure(e.to_string()))?) {
                    return Err(failure(
                        "Background output no longer has its original identity",
                    ));
                }
                file
            }
        };
        // Reserve room for the status and for lossy UTF-8 expansion. Never let
        // the generic output limiter discard bytes covered by nextOffset.
        let limit = input
            .limit
            .map_or(output::MAX_OUTPUT, |value| {
                usize::try_from(value).unwrap_or(usize::MAX)
            })
            .min((output::MAX_OUTPUT - 512) / 3);
        let requested = limit;
        let limit = if limit == 0 { 0 } else { limit.max(4) };
        let mut bytes = vec![0; limit];
        let mut filled = 0;
        while filled < limit {
            let count = file
                .read_at(
                    &mut bytes[filled..],
                    input.offset.saturating_add(filled as u64),
                )
                .map_err(|e| failure(format!("Cannot read background output: {e}")))?;
            if count == 0 {
                break;
            }
            filled += count;
        }
        bytes.truncate(filled);
        if bytes.len() > requested {
            let mut end = requested;
            if requested > 0 {
                if let Err(error) = std::str::from_utf8(&bytes[..requested]) {
                    if error.error_len().is_none() && error.valid_up_to() == 0 {
                        while end < bytes.len() && std::str::from_utf8(&bytes[..end]).is_err() {
                            end += 1;
                        }
                    }
                }
            }
            bytes.truncate(end);
        }
        // Keep an incomplete suffix for the next page while the process runs.
        // Invalid complete sequences remain visible as replacement characters.
        let mut at = 0;
        while at < bytes.len() {
            match std::str::from_utf8(&bytes[at..]) {
                Ok(_) => break,
                Err(error) => match error.error_len() {
                    Some(len) => at += error.valid_up_to() + len,
                    None => {
                        if exit.is_none() || at + error.valid_up_to() > 0 {
                            bytes.truncate(at + error.valid_up_to());
                        }
                        break;
                    }
                },
            }
        }
        let next = input.offset.saturating_add(bytes.len() as u64);
        Ok(ToolOutput {
            title: format!("background {}", id),
            output: format!(
                "{}\n[background job {}; status: {}; exit: {}; next byte offset: {}]",
                String::from_utf8_lossy(&bytes),
                id,
                if exit.is_some() { "exited" } else { "running" },
                exit.flatten()
                    .map_or_else(|| "unknown".to_string(), |code| code.to_string()),
                next
            ),
            metadata: json!({"jobID":id,"offset":input.offset,"nextOffset":next,"status":if exit.is_some() {"exited"} else {"running"},"exit":exit.flatten()}),
        })
    }
    #[cfg(not(unix))]
    {
        let _ = (session, input);
        Err(failure(
            "Background job polling is unavailable on this platform",
        ))
    }
}

#[cfg(unix)]
use crate::process_group::kill_group;

#[cfg(unix)]
fn take_jobs(keep: impl Fn(&BackgroundJob) -> bool) -> Vec<u32> {
    let mut jobs = BACKGROUND.lock().unwrap();
    let pids = jobs
        .iter()
        .filter(|job| !keep(job))
        .filter(|job| job.exit.is_none())
        .map(|job| job.pid)
        .collect();
    jobs.retain(keep);
    pids
}

/// Ends every background job. Called when the runtime exits, matching the
/// registry's process-local lifetime.
pub fn stop_background_jobs() {
    #[cfg(unix)]
    for pid in take_jobs(|_| false) {
        kill_group(pid);
    }
}

/// Ends the background jobs one session started. Aborting a turn and deleting a
/// session both own their sessions' work, so their jobs end with it.
pub fn cancel_background_jobs(session_id: &str) {
    #[cfg(not(unix))]
    let _ = session_id;
    #[cfg(unix)]
    for pid in take_jobs(|job| job.session != session_id) {
        kill_group(pid);
    }
}

#[cfg(all(test, target_os = "linux"))]
pub async fn shell_background(
    command: &str,
    directory: &Path,
    session_id: &str,
    output_directory: &Path,
) -> Result<ToolOutput, OperationError> {
    shell_background_with_env(
        command,
        directory,
        session_id,
        output_directory,
        &serde_json::Map::new(),
    )
    .await
}

pub async fn shell_background_with_env(
    command: &str,
    directory: &Path,
    session_id: &str,
    output_directory: &Path,
    env: &serde_json::Map<String, serde_json::Value>,
) -> Result<ToolOutput, OperationError> {
    #[cfg(not(unix))]
    let _ = session_id;
    let capture_dir = output_directory.to_owned();
    #[cfg(unix)]
    let (file, child_stdout, log, log_handle, identity) = tokio::task::spawn_blocking(move || {
        std::fs::create_dir_all(&capture_dir)
            .map_err(|e| failure(format!("Cannot capture shell output: {e}")))?;
        // The log lives with the other tool-output files so existing retention
        // rules remove it. Unlike captured shell output it is not size-bounded.
        // keep() leaves the log on disk; into_file() would unlink it.
        let (file, log) = output::create(&capture_dir)
            .map_err(|e| failure(format!("Cannot capture shell output: {e}")))?
            .keep()
            .map_err(|e| failure(format!("Cannot capture shell output: {}", e.error)))?;
        let log_handle = file.try_clone().map_err(|e| failure(e.to_string()))?;
        let child_stdout = file.try_clone().map_err(|e| failure(e.to_string()))?;
        let identity =
            LogIdentity::from_metadata(&file.metadata().map_err(|e| failure(e.to_string()))?);
        Ok::<_, OperationError>((file, child_stdout, log, log_handle, identity))
    })
    .await
    .map_err(|e| failure(format!("Shell setup task failed: {e}")))??;
    #[cfg(not(unix))]
    let (file, child_stdout, log) = tokio::task::spawn_blocking(move || {
        std::fs::create_dir_all(&capture_dir)
            .map_err(|e| failure(format!("Cannot capture shell output: {e}")))?;
        let (file, log) = output::create(&capture_dir)
            .map_err(|e| failure(format!("Cannot capture shell output: {e}")))?
            .keep()
            .map_err(|e| failure(format!("Cannot capture shell output: {}", e.error)))?;
        let child_stdout = file.try_clone().map_err(|e| failure(e.to_string()))?;
        Ok::<_, OperationError>((file, child_stdout, log))
    })
    .await
    .map_err(|e| failure(format!("Shell setup task failed: {e}")))??;
    let mut process = shell_command(command, directory);
    for (key, value) in env {
        if let Some(value) = value.as_str() {
            process.env(key, value);
        } else {
            process.env_remove(key);
        }
    }
    process
        .stdin(Stdio::null())
        .stdout(child_stdout)
        .stderr(file);
    #[cfg(unix)]
    // A new session keeps the job outside every process group the harness kills
    // when a command exits, and outside the terminal's own signals.
    unsafe {
        process.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = process.spawn().map_err(|e| spawn_failure(directory, e))?;
    let pid = child
        .id()
        .ok_or_else(|| failure("Shell has no process id"))?;
    let job_id = crate::types::hex_id("job_");
    #[cfg(unix)]
    {
        let mut jobs = BACKGROUND.lock().map_err(|e| failure(e.to_string()))?;
        prune_exited(&mut jobs, std::time::Instant::now(), EXITED_JOB_TTL);
        jobs.push(BackgroundJob {
            session: session_id.to_string(),
            id: job_id.clone(),
            pid,
            log: Some(log_handle),
            path: log.clone(),
            identity,
            exit: None,
            exited_at: None,
        });
    }
    // Reap the job so an exited command never becomes a zombie.
    #[cfg(unix)]
    let waiter_id = job_id.clone();
    tokio::spawn(async move {
        let status = child.wait().await.ok().and_then(|status| status.code());
        #[cfg(not(unix))]
        let _ = status;
        #[cfg(unix)]
        if let Some(job) = BACKGROUND
            .lock()
            .unwrap()
            .iter_mut()
            .find(|job| job.id == waiter_id)
        {
            job.log = None;
            job.exit = Some(status);
            job.exited_at = Some(std::time::Instant::now());
        }
    });
    let polling = if cfg!(unix) {
        "Use background_read with job_id and offset for progress; "
    } else {
        ""
    };
    Ok(ToolOutput {
        title: format!(
            "background {}",
            command.chars().take(60).collect::<String>()
        ),
        output: format!(
            "Started in the background as job {job_id} (pid {pid}).\nOutput: {}\n{polling}`kill {pid}` stops it. It keeps running across tool calls and is stopped when ocmini exits.",
            log.display()
        ),
        metadata: json!({"background":true,"jobID":job_id,"pid":pid,"outputPath":log.to_string_lossy()}),
    })
}

pub fn tracked_background(pid: u32) -> bool {
    #[cfg(unix)]
    {
        BACKGROUND
            .lock()
            .unwrap()
            .iter()
            .any(|job| job.pid == pid && job.exit.is_none())
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[cfg(unix)]
    async fn large_shell_output_preserves_the_middle_stderr_and_exit_status() {
        let root = tempfile::tempdir().unwrap();
        let text = format!(
            "BEGIN\n{}MIDDLE_MARKER\n{}END\n",
            "ordinary line\n".repeat(10_000),
            "ordinary line\n".repeat(10_000)
        );
        std::fs::write(root.path().join("input.txt"), &text).unwrap();
        let result = shell(
            "cat input.txt; cat input.txt >&2; exit 7",
            root.path(),
            Duration::from_secs(5),
            CancellationToken::new(),
            &root.path().join("output"),
        )
        .await
        .unwrap();
        assert!(result.output.starts_with("BEGIN"));
        assert!(result.output.contains("END"));
        assert!(result.output.contains("exit code: 7"));
        assert!(!result.output.contains("MIDDLE_MARKER"));
        assert!(result.output.len() < output::MAX_OUTPUT + 1024);
        assert_eq!(result.metadata["exit"], 7);
        assert_eq!(result.metadata["outputBytes"], text.len() * 2);
        let path = result.metadata["outputPath"].as_str().unwrap();
        assert_eq!(
            std::fs::read_to_string(path).unwrap(),
            format!("{text}\n[stderr]\n{text}\n[exit code: 7]")
        );
        let read = crate::tools::files::run(
            root.path().to_str().unwrap(),
            "read",
            &json!({"filePath":path,"offset":10002,"limit":1}),
            &CancellationToken::new(),
        )
        .unwrap();
        assert!(read.output.contains("MIDDLE_MARKER"));
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn shell_spill_boundaries_preserve_exact_bytes() {
        let root = tempfile::tempdir().unwrap();
        for size in [
            output::MAX_OUTPUT,
            output::MAX_OUTPUT + 1,
            MAX_CAPTURE,
            MAX_CAPTURE + 1,
        ] {
            let directory = root.path().join(format!("output-{size}"));
            let mut bytes = vec![b'x'; size];
            bytes[size / 2] = 0xff;
            std::fs::write(root.path().join("input"), &bytes).unwrap();
            let result = shell(
                "cat input",
                root.path(),
                Duration::from_secs(5),
                CancellationToken::new(),
                &directory,
            )
            .await
            .unwrap();
            // Lossy UTF-8 rendering can cross the preview limit even when the
            // raw byte count is exactly at it. The saved file must stay exact.
            assert_eq!(
                std::fs::read(result.metadata["outputPath"].as_str().unwrap()).unwrap(),
                bytes
            );
        }
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn cancelled_large_capture_leaves_no_saved_output() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("output");
        let cancel = CancellationToken::new();
        let flag = cancel.clone();
        let ready = root.path().join("ready");
        tokio::spawn(async move {
            while !ready.exists() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            flag.cancel();
        });
        let result = shell(
            "head -c 1048576 /dev/zero; touch ready; sleep 30",
            root.path(),
            Duration::from_secs(5),
            cancel,
            &directory,
        )
        .await;
        assert!(result.unwrap_err().message.contains("cancelled"));
        // Anonymous capture files close and disappear without publishing an
        // incomplete result when execution is cancelled.
        assert!(std::fs::read_dir(directory).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("tool_")
        }));
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn shell_uses_directory_and_reports_exit() {
        let root = crate::test_tempdir().unwrap();
        let directory = root.path();
        let result = shell(
            "pwd; printf 'problem' >&2; exit 7",
            directory,
            Duration::from_secs(2),
            CancellationToken::new(),
            directory,
        )
        .await
        .unwrap();
        assert!(result.output.contains(&directory.display().to_string()));
        assert!(result.output.contains("problem"));
        assert_eq!(result.metadata["exit"], 7);
    }

    #[tokio::test]
    #[cfg(target_os = "linux")]
    async fn timeout_kills_descendants_and_reaps_shell() {
        let directory = std::env::temp_dir();
        let pidfile = directory.join(format!("ocmini-test-{}", crate::types::hex_id("pid_")));
        let command = format!("sleep 30 & echo $$ $! > '{}'; wait", pidfile.display());
        let result = shell(
            &command,
            &directory,
            Duration::from_secs(1),
            CancellationToken::new(),
            &directory,
        )
        .await;
        assert_eq!(result.unwrap().metadata["timeout"], true);
        let pids: Vec<i32> = std::fs::read_to_string(&pidfile)
            .unwrap()
            .split_whitespace()
            .map(|pid| pid.parse().unwrap())
            .collect();
        let _ = std::fs::remove_file(pidfile);
        assert!(
            !Path::new(&format!("/proc/{}", pids[0])).exists(),
            "Shell was not reaped"
        );
        assert_process_stopped(pids[1]).await;
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn cancellation_is_prompt() {
        let cancel = CancellationToken::new();
        let flag = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(40)).await;
            flag.cancel();
        });
        let result = shell(
            "sleep 30",
            &std::env::temp_dir(),
            Duration::from_secs(30),
            cancel,
            &std::env::temp_dir(),
        )
        .await;
        assert!(result.unwrap_err().message.contains("cancelled"));
    }
    #[tokio::test]
    #[cfg(target_os = "linux")]
    async fn dropping_execution_kills_the_process_group() {
        let pidfile = std::env::temp_dir().join(crate::types::hex_id("ocmini-drop-pid-"));
        let path = pidfile.clone();
        let task = tokio::spawn(async move {
            let command = format!("sleep 30 & echo $! > '{}'; wait", path.display());
            shell(
                &command,
                &std::env::temp_dir(),
                Duration::from_secs(30),
                CancellationToken::new(),
                &std::env::temp_dir(),
            )
            .await
        });
        for _ in 0..100 {
            if pidfile.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let pid: i32 = std::fs::read_to_string(&pidfile)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        task.abort();
        let _ = task.await;
        let _ = std::fs::remove_file(pidfile);
        assert_process_stopped(pid).await;
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn timeout_reports_the_output_captured_before_the_limit() {
        let root = tempfile::tempdir().unwrap();
        let result = shell(
            "echo BEFORE_LIMIT; echo BEFORE_STDERR >&2; sleep 30",
            root.path(),
            Duration::from_millis(500),
            CancellationToken::new(),
            &root.path().join("output"),
        )
        .await
        .unwrap();
        // The captured output survives the limit and the timeout is metadata.
        assert!(result.output.contains("BEFORE_LIMIT"), "{}", result.output);
        assert!(result.output.contains("BEFORE_STDERR"), "{}", result.output);
        assert!(
            result.output.contains("exceeding timeout 500 ms"),
            "{}",
            result.output
        );
        assert_eq!(result.metadata["timeout"], true);
        assert!(result.metadata["exit"].is_null());
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn a_timed_out_silent_command_reports_no_output() {
        let root = tempfile::tempdir().unwrap();
        let result = shell(
            "sleep 30",
            root.path(),
            Duration::from_millis(300),
            CancellationToken::new(),
            &root.path().join("output"),
        )
        .await
        .unwrap();
        assert!(
            result.output.starts_with("(no output)"),
            "{}",
            result.output
        );
    }

    #[tokio::test]
    #[cfg(target_os = "linux")]
    async fn background_commands_survive_the_tool_call_and_stop_with_their_session() {
        let root = tempfile::tempdir().unwrap();
        let result = shell_background(
            "for i in 1 2 3 4 5 6; do echo tick $i; sleep 0.2; done",
            root.path(),
            "ses_owner",
            &root.path().join("output"),
        )
        .await
        .unwrap();
        let pid = result.metadata["pid"].as_u64().unwrap() as i32;
        let log = std::path::PathBuf::from(result.metadata["outputPath"].as_str().unwrap());
        for _ in 0..100 {
            if std::fs::read_to_string(&log).is_ok_and(|text| text.contains("tick 3")) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        // The tool call returned long before the job finished its later ticks.
        let text = std::fs::read_to_string(&log).unwrap();
        assert!(text.contains("tick 3"), "{text}");
        assert!(
            Path::new(&format!("/proc/{pid}")).exists(),
            "background job was stopped with the command"
        );
        // Another session's abort leaves this job alone; its own session ends it.
        cancel_background_jobs("ses_other");
        assert!(Path::new(&format!("/proc/{pid}")).exists());
        cancel_background_jobs("ses_owner");
        assert_process_stopped(pid).await;
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn background_poll_tracks_offsets_status_and_owner() {
        let root = tempfile::tempdir().unwrap();
        let result = shell_background_with_env(
            "printf first; sleep 0.2; printf second; exit 7",
            root.path(),
            "poll_owner",
            root.path(),
            &serde_json::Map::new(),
        )
        .await
        .unwrap();
        let id = result.metadata["jobID"].as_str().unwrap().to_owned();
        let input = |offset, limit| BackgroundReadInput {
            job_id: id.clone(),
            offset,
            limit,
        };
        assert!(read_background("poll_other", &input(0, None)).is_err());
        tokio::time::sleep(Duration::from_millis(60)).await;
        let first = read_background("poll_owner", &input(0, Some(5))).unwrap();
        assert!(first.output.starts_with("first\n[background job"));
        assert_eq!(first.metadata["status"], "running");
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let next = read_background("poll_owner", &input(5, None)).unwrap();
                if next.metadata["status"] == "exited" {
                    assert!(next.output.starts_with("second\n[background job"));
                    assert_eq!(next.metadata["nextOffset"], 11);
                    assert_eq!(next.metadata["exit"], 7);
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        cancel_background_jobs("poll_owner");
        assert!(read_background("poll_owner", &input(0, None)).is_err());
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn polling_pages_are_bounded_and_reject_replaced_logs() {
        let root = tempfile::tempdir().unwrap();
        let source = format!("{}🙂end", "x".repeat(output::MAX_OUTPUT));
        std::fs::write(root.path().join("source"), &source).unwrap();
        let launch = shell_background_with_env(
            "cat source",
            root.path(),
            "page_owner",
            root.path(),
            &serde_json::Map::new(),
        )
        .await
        .unwrap();
        let id = launch.metadata["jobID"].as_str().unwrap().to_owned();
        let input = |offset| BackgroundReadInput {
            job_id: id.clone(),
            offset,
            limit: Some(u64::MAX),
        };
        let mut offset = 0;
        let mut text = String::new();
        loop {
            let page = read_background("page_owner", &input(offset)).unwrap();
            let end = page.metadata["nextOffset"].as_u64().unwrap();
            assert!(page.output.len() <= output::MAX_OUTPUT);
            let content = page.output.split("\n[background job ").next().unwrap();
            text.push_str(content);
            offset = end;
            if page.metadata["status"] == "exited" && offset == source.len() as u64 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(text, source);
        let path = launch.metadata["outputPath"].as_str().unwrap();
        std::fs::remove_file(path).unwrap();
        std::fs::write(path, "replacement").unwrap();
        assert!(read_background("page_owner", &input(0)).is_err());
        cancel_background_jobs("page_owner");
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn background_poll_survives_grandchild_writes_after_shell_exit() {
        let root = tempfile::tempdir().unwrap();
        let launch = shell_background_with_env(
            "(sleep 0.15; printf late) & printf early; exit 0",
            root.path(),
            "grandchild_owner",
            root.path(),
            &serde_json::Map::new(),
        )
        .await
        .unwrap();
        let id = launch.metadata["jobID"].as_str().unwrap().to_owned();
        let input = |offset| BackgroundReadInput {
            job_id: id.clone(),
            offset,
            limit: None,
        };
        let mut offset = 0;
        let mut combined = String::new();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let page = read_background("grandchild_owner", &input(offset)).unwrap();
                let content = page.output.split("\n[background job ").next().unwrap();
                combined.push_str(content);
                offset = page.metadata["nextOffset"].as_u64().unwrap();
                if page.metadata["status"] == "exited" && combined == "earlylate" {
                    assert_eq!(page.metadata["exit"], 0);
                    break;
                }
                tokio::time::sleep(Duration::from_millis(15)).await;
            }
        })
        .await
        .unwrap();
        cancel_background_jobs("grandchild_owner");
    }

    #[test]
    #[cfg(unix)]
    fn prune_exited_reclaims_only_expired_finished_jobs() {
        let root = tempfile::tempdir().unwrap();
        let log_path = root.path().join("job.txt");
        std::fs::write(&log_path, "ok").unwrap();
        let identity = LogIdentity::from_metadata(&std::fs::metadata(&log_path).unwrap());
        let now = std::time::Instant::now();
        let ttl = Duration::from_secs(60);
        let mut jobs = vec![
            BackgroundJob {
                session: "s".into(),
                id: "running".into(),
                pid: 1,
                log: None,
                path: log_path.clone(),
                identity,
                exit: None,
                exited_at: None,
            },
            BackgroundJob {
                session: "s".into(),
                id: "recent".into(),
                pid: 2,
                log: None,
                path: log_path.clone(),
                identity,
                exit: Some(Some(0)),
                exited_at: Some(now),
            },
            BackgroundJob {
                session: "s".into(),
                id: "expired".into(),
                pid: 3,
                log: None,
                path: log_path,
                identity,
                exit: Some(Some(0)),
                exited_at: Some(now - Duration::from_secs(120)),
            },
        ];
        prune_exited(&mut jobs, now, ttl);
        let kept: Vec<_> = jobs.iter().map(|job| job.id.as_str()).collect();
        assert_eq!(kept, vec!["running", "recent"]);
    }

    #[cfg(target_os = "linux")]
    async fn assert_process_stopped(pid: i32) {
        // SIGKILL delivery is asynchronous; the descendant can still be runnable
        // after its parent has been reaped. Accept an exited or zombie process.
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
                    Err(error)
                        if error.kind() == std::io::ErrorKind::NotFound
                            || error.raw_os_error() == Some(libc::ESRCH) =>
                    {
                        return
                    }
                    Ok(state) if state.rsplit_once(") ").unwrap().1.starts_with('Z') => return,
                    Err(error) => panic!("Cannot inspect process {pid}: {error}"),
                    _ => tokio::time::sleep(Duration::from_millis(10)).await,
                }
            }
        })
        .await
        .expect("Shell descendant survived process group termination");
    }
    #[tokio::test]
    async fn shell_environment_is_local_to_each_process() {
        let root = tempfile::tempdir().unwrap();
        let key = "OCMINI_TEST_PROCESS_LOCAL_ENV";
        let env = serde_json::json!({(key):"private value"});
        let first = shell_with_env(
            if cfg!(windows) {
                "echo %OCMINI_TEST_PROCESS_LOCAL_ENV%"
            } else {
                r#"printf '%s' "$OCMINI_TEST_PROCESS_LOCAL_ENV""#
            },
            root.path(),
            Duration::from_secs(5),
            CancellationToken::new(),
            root.path(),
            env.as_object().unwrap(),
        );
        let second = shell(
            if cfg!(windows) {
                "if defined OCMINI_TEST_PROCESS_LOCAL_ENV (echo %OCMINI_TEST_PROCESS_LOCAL_ENV%) else (echo unset)"
            } else {
                r#"printf '%s' "${OCMINI_TEST_PROCESS_LOCAL_ENV-unset}""#
            },
            root.path(),
            Duration::from_secs(5),
            CancellationToken::new(),
            root.path(),
        );
        let (first, second) = tokio::join!(first, second);
        assert_eq!(first.unwrap().output.trim_end(), "private value");
        assert_eq!(second.unwrap().output.trim_end(), "unset");
        assert!(std::env::var_os(key).is_none());
    }
}
