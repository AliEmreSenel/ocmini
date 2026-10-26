//! Process-group ownership shared by hooks and shell tools. Protocol framing,
//! output capture and scheduling remain with their callers.
use std::io;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

#[derive(Clone)]
pub struct Group {
    id: u32,
    reaped: Arc<AtomicBool>,
}
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
        Self {
            id,
            reaped: Arc::new(AtomicBool::new(false)),
        }
    }
    pub fn kill(&self) {
        if !self.reaped.load(Ordering::Acquire) {
            kill_group(self.id);
        }
    }
    pub async fn wait(
        &self,
        child: &mut tokio::process::Child,
    ) -> io::Result<std::process::ExitStatus> {
        let (id, reaped) = (self.id, &self.reaped);
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
impl Drop for Group {
    fn drop(&mut self) {
        self.kill();
    }
}
