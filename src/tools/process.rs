use super::{cancelled, failure, output};
use crate::types::{ToolError, ToolOutput};
use serde_json::json;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::process::{ExitStatus, Stdio};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const MAX_CAPTURE: usize = 64 * 1024;

pub(super) struct ProcessGroup {
    pub(super) id: u32,
}

impl Drop for ProcessGroup {
    fn drop(&mut self) {
        #[cfg(unix)]
        unsafe {
            libc::kill(-(self.id as i32), libc::SIGKILL);
        }
    }
}

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

fn spawn_failure(directory: &Path, error: std::io::Error) -> ToolError {
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

pub async fn shell(
    command: &str,
    directory: &Path,
    timeout: Duration,
    cancel: Arc<AtomicBool>,
    output_directory: &Path,
) -> Result<ToolOutput, ToolError> {
    // Let the OS capture output directly; only bounded previews enter memory.
    std::fs::create_dir_all(output_directory)
        .map_err(|e| failure(format!("Cannot capture shell output: {e}")))?;
    let capture = || -> std::io::Result<_> {
        Ok((
            tempfile::tempfile_in(output_directory)?,
            tempfile::tempfile_in(output_directory)?,
        ))
    };
    let (stdout, stderr) =
        capture().map_err(|e| failure(format!("Cannot capture shell output: {e}")))?;
    let mut process = shell_command(command, directory);
    process
        .stdin(Stdio::null())
        .stdout(stdout.try_clone().map_err(|e| failure(e.to_string()))?)
        .stderr(stderr.try_clone().map_err(|e| failure(e.to_string()))?)
        .kill_on_drop(true);
    #[cfg(unix)]
    process.process_group(0);
    let mut child = process.spawn().map_err(|e| spawn_failure(directory, e))?;
    let group = ProcessGroup {
        id: child
            .id()
            .ok_or_else(|| failure("Shell has no process id"))?,
    };
    let outcome = tokio::select! {
        result = child.wait() => result.map(Outcome::Exited).map_err(|e| failure(format!("Shell wait failed: {e}")))?,
        _ = tokio::time::sleep(timeout) => Outcome::TimedOut,
        _ = cancelled(cancel) => Outcome::Cancelled,
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
                    "\n[command timed out after {} milliseconds]",
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
        // The captured text is the only record of work the limit cut short.
        return Err(failure(format!(
            "Shell command timed out after {} milliseconds\n{}",
            timeout.as_millis(),
            result.output
        )));
    }
    Ok(result)
}

/// Commands started with `background: true` outlive their tool call. The
/// runtime stops them when it exits so a session cannot leak processes.
#[cfg(unix)]
static BACKGROUND: Mutex<Vec<u32>> = Mutex::new(Vec::new());

pub fn stop_background_jobs() {
    #[cfg(unix)]
    for pid in BACKGROUND
        .lock()
        .map(|mut jobs| std::mem::take(&mut *jobs))
        .unwrap_or_default()
    {
        unsafe {
            libc::kill(-(pid as i32), libc::SIGKILL);
        }
    }
}

pub async fn shell_background(
    command: &str,
    directory: &Path,
    output_directory: &Path,
) -> Result<ToolOutput, ToolError> {
    std::fs::create_dir_all(output_directory)
        .map_err(|e| failure(format!("Cannot capture shell output: {e}")))?;
    // The log lives with the other tool-output files so existing retention
    // rules remove it. Unlike captured shell output it is not size-bounded.
    // keep() leaves the log on disk; into_file() would unlink it.
    let (file, log) = output::create(output_directory)
        .map_err(|e| failure(format!("Cannot capture shell output: {e}")))?
        .keep()
        .map_err(|e| failure(format!("Cannot capture shell output: {}", e.error)))?;
    let mut process = shell_command(command, directory);
    process
        .stdin(Stdio::null())
        .stdout(file.try_clone().map_err(|e| failure(e.to_string()))?)
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
    #[cfg(unix)]
    BACKGROUND
        .lock()
        .map_err(|e| failure(e.to_string()))?
        .push(pid);
    // Reap the job so an exited command never becomes a zombie.
    tokio::spawn(async move {
        let _ = child.wait().await;
        #[cfg(unix)]
        if let Ok(mut jobs) = BACKGROUND.lock() {
            jobs.retain(|job| *job != pid);
        }
    });
    Ok(ToolOutput {
        title: format!(
            "background {}",
            command.chars().take(60).collect::<String>()
        ),
        output: format!(
            "Started in the background as pid {pid}.\nOutput: {}\nRead that file for progress; `kill {pid}` stops it. It keeps running across tool calls and is stopped when ocmini exits.",
            log.display()
        ),
        metadata: json!({"background":true,"pid":pid,"outputPath":log.to_string_lossy()}),
    })
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
            Arc::new(AtomicBool::new(false)),
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
            &AtomicBool::new(false),
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
                Arc::new(AtomicBool::new(false)),
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
        let cancel = Arc::new(AtomicBool::new(false));
        let flag = cancel.clone();
        let ready = root.path().join("ready");
        tokio::spawn(async move {
            while !ready.exists() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            flag.store(true, std::sync::atomic::Ordering::Release);
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
        let directory = std::env::temp_dir();
        let result = shell(
            "pwd; printf 'problem' >&2; exit 7",
            &directory,
            Duration::from_secs(2),
            Arc::new(AtomicBool::new(false)),
            &directory,
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
            Arc::new(AtomicBool::new(false)),
            &directory,
        )
        .await;
        assert!(result.unwrap_err().message.contains("timed out"));
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
        let cancel = Arc::new(AtomicBool::new(false));
        let flag = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(40)).await;
            flag.store(true, std::sync::atomic::Ordering::Release);
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
                Arc::new(AtomicBool::new(false)),
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
            Arc::new(AtomicBool::new(false)),
            &root.path().join("output"),
        )
        .await;
        let error = result.unwrap_err().message;
        assert!(error.contains("timed out"), "{error}");
        assert!(error.contains("BEFORE_LIMIT"), "{error}");
        assert!(error.contains("BEFORE_STDERR"), "{error}");
    }

    #[tokio::test]
    #[cfg(target_os = "linux")]
    async fn background_commands_survive_the_tool_call_and_stop_with_the_runtime() {
        let root = tempfile::tempdir().unwrap();
        let result = shell_background(
            "for i in 1 2 3 4 5 6; do echo tick $i; sleep 0.2; done",
            root.path(),
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
        stop_background_jobs();
        assert_process_stopped(pid).await;
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
}
