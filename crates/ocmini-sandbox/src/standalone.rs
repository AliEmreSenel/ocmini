//! The standalone executable's trusted supervisor. No ocmini sessions or tools.
use ocmini_sandbox::{broker, config::Policy, jail, refusal, wire};
use std::{
    ffi::OsString,
    fs::File,
    io::{self, Read, Seek, SeekFrom},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::process::CommandExt,
    },
    path::PathBuf,
};
use tokio::signal::unix::{signal, Signal, SignalKind};

struct Signals {
    interrupt: Signal,
    terminate: Signal,
    hangup: Signal,
    quit: Signal,
}
impl Signals {
    fn new() -> io::Result<Self> {
        Ok(Self {
            interrupt: signal(SignalKind::interrupt())?,
            terminate: signal(SignalKind::terminate())?,
            hangup: signal(SignalKind::hangup())?,
            quit: signal(SignalKind::quit())?,
        })
    }
    async fn recv(&mut self) -> i32 {
        tokio::select! {
            _ = self.interrupt.recv() => libc::SIGINT,
            _ = self.terminate.recv() => libc::SIGTERM,
            _ = self.hangup.recv() => libc::SIGHUP,
            _ = self.quit.recv() => libc::SIGQUIT,
        }
    }
}

fn runtime() -> io::Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
}

// A command may take terminal foreground ownership (e.g. an interactive shell).
// Restore the host process group even when the jail uses a PID namespace.
struct Foreground(i32);
impl Drop for Foreground {
    fn drop(&mut self) {
        if self.0 >= 0 {
            let _ = set_foreground(self.0);
        }
    }
}
fn set_foreground(group: i32) -> io::Result<()> {
    unsafe {
        let mut blocked = std::mem::zeroed();
        let mut previous = std::mem::zeroed();
        libc::sigemptyset(&mut blocked);
        libc::sigaddset(&mut blocked, libc::SIGTTOU);
        if libc::sigprocmask(libc::SIG_BLOCK, &blocked, &mut previous) < 0 {
            return Err(io::Error::last_os_error());
        }
        let result = libc::tcsetpgrp(0, group);
        let error = io::Error::last_os_error();
        if libc::sigprocmask(libc::SIG_SETMASK, &previous, std::ptr::null_mut()) < 0 {
            return Err(io::Error::last_os_error());
        }
        if result < 0 {
            Err(error)
        } else {
            Ok(())
        }
    }
}

pub fn run(policy: Policy, directory: PathBuf, command: &[OsString]) -> Result<i32, String> {
    let _foreground = Foreground(unsafe { libc::tcgetpgrp(0) });
    // No database, persistent session output, or daemon is needed. The jail's
    // existing mounts and network bootstrap use one private temporary tree.
    let scratch = tempfile::Builder::new()
        .prefix("ocmini-sandbox-")
        .tempdir()
        .map_err(refusal)?;
    let root = scratch.path().canonicalize().map_err(refusal)?;
    let runtime = runtime().map_err(refusal)?;
    runtime.block_on(async {
        let mut signals = Signals::new().map_err(refusal)?;
        let mut child = jail::spawn_command(jail::RuntimeSpec {
            policy: policy.clone(),
            scratch: root.clone(),
            spill: root,
            directory,
        }, command)?;
        let control = child.control.try_clone().map_err(refusal)?;
        control.set_nonblocking(true).map_err(refusal)?;
        let mut control = tokio::net::UnixStream::from_std(control).map_err(refusal)?;
        // The broker owns the only host end of the packet socket, so the guest
        // relay sees it close and the jail stops if the broker stops.
        let packets = child.packets.take();
        let pid = child.pid;
        let broker = async {
            match packets {
                Some(packets) => broker::run(packets, pid, policy, Default::default()).await,
                None => std::future::pending().await,
            }
        };
        tokio::pin!(broker);
        let mut stopped = None;
        let completion = wire::read_control(&mut control);
        tokio::pin!(completion);
        loop {
            tokio::select! {
                biased;
                result = &mut completion => {
                    return match result {
                        Ok(wire::Control::Result { id: 0, value }) => value.as_i64()
                            .filter(|code| (0..=255).contains(code))
                            .map(|code| code as i32).ok_or_else(|| refusal("invalid command exit status")),
                        Ok(_) => Err(refusal("invalid command supervisor response")),
                        Err(error) => Err(refusal(match stopped {
                            Some(result) => format!("network broker stopped: {result:?}"),
                            None => error.to_string(),
                        })),
                    };
                }
                // The guest closes its packet relay when the command finishes,
                // which can stop the broker before the already written
                // completion is observed. Report the broker only if the
                // supervisor exits without a result.
                result = &mut broker, if stopped.is_none() => stopped = Some(result),
                signal = signals.recv() => {
                    let result = unsafe { libc::syscall(libc::SYS_pidfd_send_signal,
                        child.pidfd.as_raw_fd(), signal, std::ptr::null::<libc::siginfo_t>(), 0) };
                    if result < 0 {
                        let error = io::Error::last_os_error();
                        if error.raw_os_error() != Some(libc::ESRCH) { return Err(refusal(error)); }
                    }
                }
            }
        }
    })
}

fn children(table: &mut File) -> io::Result<Vec<i32>> {
    table.seek(SeekFrom::Start(0))?;
    let mut list = String::new();
    table.read_to_string(&mut list)?;
    Ok(list
        .split_whitespace()
        .filter_map(|pid| pid.parse().ok())
        .collect())
}

fn reap(main: i32) -> io::Result<Option<i32>> {
    let mut exit = None;
    loop {
        let mut status = 0;
        let pid = unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) };
        if pid == 0 {
            return Ok(exit);
        }
        if pid < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return if error.raw_os_error() == Some(libc::ECHILD) {
                Ok(exit)
            } else {
                Err(error)
            };
        }
        if pid == main {
            exit = Some(if libc::WIFEXITED(status) {
                libc::WEXITSTATUS(status)
            } else {
                128 + libc::WTERMSIG(status)
            });
        }
    }
}

pub fn command_main(command: Vec<OsString>) -> Result<i32, String> {
    let spec = jail::inherited_command_spec().map_err(refusal)?;
    let runtime = runtime().map_err(refusal)?;
    let (mut signals, mut child_signal) = {
        let _entered = runtime.enter();
        (
            Signals::new().map_err(refusal)?,
            signal(SignalKind::child()).map_err(refusal)?,
        )
    };
    // Adopt orphaned descendants even in presets without a PID namespace.
    if unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) } < 0 {
        return Err(refusal(io::Error::last_os_error()));
    }
    jail::finish_runtime(&spec.policy).map_err(refusal)?;
    let control = unsafe { std::os::unix::net::UnixStream::from_raw_fd(3) };
    control.set_nonblocking(true).map_err(refusal)?;
    let mut table = unsafe { File::from_raw_fd(7) };
    runtime.block_on(tokio::task::LocalSet::new().run_until(async move {
        let mut control = tokio::net::UnixStream::from_std(control).map_err(refusal)?;
        let relay = if spec.policy.network.mode.mediated() { Some(broker::relay_inherited().map_err(refusal)?) } else { None };
        let relay_end = async move {
            match relay {
                Some(mut relay) => relay.wait().await,
                None => std::future::pending().await,
            }
        };
        tokio::pin!(relay_end);
        let program = command.first().ok_or_else(|| refusal("missing command"))?;
        let foreground = unsafe { libc::tcgetpgrp(0) == libc::getpgrp() };
        let mut process = std::process::Command::new(program);
        process.args(&command[1..]);
        process.process_group(0);
        let spawned = process.spawn();
        let code = match spawned {
            Err(error) => {
                eprintln!("ocmini-sandbox: {}: {error}", program.to_string_lossy());
                if error.kind() == io::ErrorKind::NotFound { 127 } else { 126 }
            }
            Ok(child) => {
                if foreground {
                    set_foreground(child.id() as i32).map_err(refusal)?;
                    // A terminal read can stop the child between spawn and the
                    // foreground handoff. Resume it after assigning the tty.
                    unsafe { libc::kill(-(child.id() as i32), libc::SIGCONT); }
                }
                let result = loop {
                    if let Some(code) = reap(child.id() as i32).map_err(refusal)? { break Ok(code); }
                    tokio::select! {
                        _ = child_signal.recv() => {},
                        signal = signals.recv() => {
                            unsafe { libc::kill(-(child.id() as i32), signal); }
                        }
                        result = &mut relay_end => break Err(refusal(format!("network relay stopped: {result:?}"))),
                    }
                };
                // Kill and reap direct children, then any newly adopted
                // descendants, before acknowledging completion to the host.
                loop {
                    reap(-1).map_err(refusal)?;
                    let remaining = children(&mut table).map_err(refusal)?;
                    if remaining.is_empty() { break; }
                    for pid in remaining { unsafe { libc::kill(pid, libc::SIGKILL); } }
                    child_signal.recv().await;
                }
                result?
            }
        };
        wire::write_control(&mut control, &wire::Control::Result { id: 0, value: code.into() }).await.map_err(refusal)?;
        Ok(code)
    }))
}
