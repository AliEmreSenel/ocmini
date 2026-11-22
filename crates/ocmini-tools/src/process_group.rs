//! Process-group ownership shared by hooks and shell tools. Protocol framing,
//! output capture and scheduling remain with their callers.
use std::io;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

static TRACKED: std::sync::LazyLock<std::sync::Mutex<std::collections::HashSet<u32>>> =
    std::sync::LazyLock::new(Default::default);
fn lock_tracked() -> std::sync::MutexGuard<'static, std::collections::HashSet<u32>> {
    TRACKED.lock().unwrap_or_else(|e| e.into_inner())
}
pub fn tracked(pid: u32) -> bool {
    lock_tracked().contains(&pid)
}

struct Inner {
    id: u32,
    reaped: AtomicBool,
}
impl Drop for Inner {
    fn drop(&mut self) {
        if !self.reaped.load(Ordering::Acquire) {
            kill_group(self.id);
        }
        lock_tracked().remove(&self.id);
    }
}

#[derive(Clone)]
pub struct Group(Arc<Inner>);
pub fn kill_group(id: u32) {
    #[cfg(unix)]
    if id > 1 && id <= i32::MAX as u32 {
        unsafe {
            libc::kill(-(id as i32), libc::SIGKILL);
        }
    }
    #[cfg(not(unix))]
    let _ = id;
}
impl Group {
    pub fn new(id: u32) -> Self {
        lock_tracked().insert(id);
        Self(Arc::new(Inner {
            id,
            reaped: AtomicBool::new(false),
        }))
    }
    pub fn kill(&self) {
        if !self.0.reaped.load(Ordering::Acquire) {
            kill_group(self.0.id);
        }
    }
    pub async fn wait(
        &self,
        child: &mut tokio::process::Child,
    ) -> io::Result<std::process::ExitStatus> {
        let (id, reaped) = (self.0.id, &self.0.reaped);
        #[cfg(unix)]
        {
            let mut exits = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::child())?;
            loop {
                let exited = {
                    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
                    // WNOWAIT keeps the leader's PID reserved until descendants have
                    // been stopped; never signal a process-group ID after reaping it.
                    let result = unsafe {
                        libc::waitid(
                            libc::P_PID,
                            id,
                            &mut info,
                            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                        )
                    };
                    if result < 0 {
                        let error = io::Error::last_os_error();
                        if error.kind() == io::ErrorKind::Interrupted {
                            continue;
                        }
                        if error.raw_os_error() == Some(libc::ECHILD) {
                            reaped.store(true, Ordering::Release);
                        }
                        return Err(error);
                    }
                    unsafe { info.si_pid() == id as i32 }
                };
                if exited {
                    self.kill();
                    break;
                }
                if exits.recv().await.is_none() {
                    return Err(io::Error::other("process exit watcher closed"));
                }
            }
        }
        #[cfg(not(unix))]
        let _ = id;
        let status = child.wait().await?;
        reaped.store(true, Ordering::Release);
        Ok(status)
    }
}
