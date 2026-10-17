use super::{cancelled, cap_out, failure};
use crate::types::{ToolError, ToolOutput};
use serde_json::json;
use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt};

const MAX_CAPTURE: usize = 64 * 1024;

struct ProcessGroup {
    id: u32,
}

impl Drop for ProcessGroup {
    fn drop(&mut self) {
        #[cfg(unix)]
        unsafe {
            libc::kill(-(self.id as i32), libc::SIGKILL);
        }
    }
}

async fn capture(mut stream: impl AsyncRead + Unpin) -> std::io::Result<(Vec<u8>, usize)> {
    let mut output = Vec::new();
    let mut total = 0;
    let mut chunk = [0u8; 8192];
    loop {
        let count = stream.read(&mut chunk).await?;
        if count == 0 {
            break;
        }
        total += count;
        output.extend_from_slice(&chunk[..count]);
        if output.len() > MAX_CAPTURE {
            let excess = output.len() - MAX_CAPTURE;
            output.drain(MAX_CAPTURE / 2..MAX_CAPTURE / 2 + excess);
        }
    }
    Ok((output, total))
}

pub async fn shell(
    command: &str,
    directory: &Path,
    timeout: Duration,
    cancel: Arc<AtomicBool>,
) -> Result<ToolOutput, ToolError> {
    let mut process = tokio::process::Command::new(if cfg!(windows) { "cmd" } else { "bash" });
    if cfg!(windows) {
        process.args(["/C", command]);
    } else {
        process.args(["-c", command]);
    }
    process
        .current_dir(directory)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    process.process_group(0);
    let mut child = process.spawn().map_err(|e| {
        failure(format!(
            "Could not start shell in {}: {e}",
            directory.display()
        ))
    })?;
    let group = ProcessGroup {
        id: child
            .id()
            .ok_or_else(|| failure("Shell has no process id"))?,
    };
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| failure("Shell stdout is unavailable"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| failure("Shell stderr is unavailable"))?;
    let stdout_task = tokio::spawn(capture(stdout));
    let stderr_task = tokio::spawn(capture(stderr));
    let status = tokio::select! {
        result = child.wait() => result.map_err(|e| failure(format!("Shell wait failed: {e}")))?,
        _ = tokio::time::sleep(timeout) => {
            drop(group);
            let _ = child.kill().await;
            let _ = child.wait().await;
            stdout_task.abort();
            stderr_task.abort();
            return Err(failure(format!("Shell command timed out after {} milliseconds", timeout.as_millis())));
        }
        _ = cancelled(cancel) => {
            drop(group);
            let _ = child.kill().await;
            let _ = child.wait().await;
            stdout_task.abort();
            stderr_task.abort();
            return Err(failure("Shell command cancelled"));
        }
    };
    // Kill background descendants before joining their inherited output pipes.
    drop(group);
    let (stdout, stdout_total) = stdout_task
        .await
        .map_err(|e| failure(format!("Shell stdout task failed: {e}")))?
        .map_err(|e| failure(format!("Shell stdout failed: {e}")))?;
    let (stderr, stderr_total) = stderr_task
        .await
        .map_err(|e| failure(format!("Shell stderr task failed: {e}")))?
        .map_err(|e| failure(format!("Shell stderr failed: {e}")))?;
    let mut text = String::from_utf8_lossy(&stdout).into_owned();
    if !stderr.is_empty() {
        text.push_str("\n[stderr]\n");
        text.push_str(&String::from_utf8_lossy(&stderr));
    }
    if stdout_total > stdout.len() || stderr_total > stderr.len() {
        text.push_str(&format!(
            "\n[{} bytes omitted from captured process output]",
            stdout_total + stderr_total - stdout.len() - stderr.len()
        ));
    }
    if !status.success() {
        text.push_str(&format!(
            "\n[exit code: {}]",
            status
                .code()
                .map_or_else(|| "terminated by signal".into(), |v| v.to_string())
        ));
    }
    Ok(ToolOutput {
        title: command.chars().take(80).collect(),
        output: cap_out(&text, 50 * 1024),
        metadata: json!({"exit":status.code(),"truncated":stdout_total + stderr_total > 50 * 1024,"outputBytes":stdout_total+stderr_total}),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[cfg(unix)]
    async fn shell_uses_directory_and_reports_exit() {
        let directory = std::env::temp_dir();
        let result = shell(
            "pwd; printf 'problem' >&2; exit 7",
            &directory,
            Duration::from_secs(2),
            Arc::new(AtomicBool::new(false)),
        )
        .await
        .unwrap();
        assert!(result.output.contains(&directory.display().to_string()));
        assert!(result.output.contains("problem"));
        assert_eq!(result.metadata["exit"], 7);
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn timeout_kills_descendants_and_reaps_shell() {
        let directory = std::env::temp_dir();
        let pidfile = directory.join(format!("ocmini-test-{}", crate::types::hex_id("pid_")));
        let command = format!("sleep 30 & echo $! > '{}'; wait", pidfile.display());
        let result = shell(
            &command,
            &directory,
            Duration::from_millis(150),
            Arc::new(AtomicBool::new(false)),
        )
        .await;
        assert!(result.unwrap_err().message.contains("timed out"));
        let pid: i32 = std::fs::read_to_string(&pidfile)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let _ = std::fs::remove_file(pidfile);
        let state = std::fs::read_to_string(format!("/proc/{pid}/stat"));
        assert!(state.is_err() || state.unwrap().split_whitespace().nth(2) == Some("Z"));
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
        for _ in 0..100 {
            let state = std::fs::read_to_string(format!("/proc/{pid}/stat"));
            if state.is_err() || state.unwrap().split_whitespace().nth(2) == Some("Z") {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("Shell descendant survived dropped execution");
    }
}
