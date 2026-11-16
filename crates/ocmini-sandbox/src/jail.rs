//! All fallible bootstrap work precedes exec. The clone child uses only raw
//! syscalls and preallocated data: no allocator or inherited runtime locks.
use crate::config::{NetMode, Policy, ProcessIsolation, UnixSockets};
use crate::wire;
use landlock::{Access, AccessFs, CompatLevel, Compatible, Ruleset, RulesetAttr, ABI};
use serde::{Deserialize, Serialize};
use std::ffi::{CString, OsStr, OsString};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::{
    ffi::OsStrExt,
    fs::{MetadataExt, OpenOptionsExt},
};
use std::path::{Path, PathBuf};

#[derive(Clone, Serialize, Deserialize)]
pub struct RuntimeSpec {
    pub policy: Policy,
    pub scratch: PathBuf,
    pub spill: PathBuf,
    pub directory: PathBuf,
}
pub struct Child {
    pub pid: i32,
    pub pidfd: OwnedFd,
    pub control: std::os::unix::net::UnixStream,
    pub packets: Option<std::os::unix::net::UnixStream>,
}
impl Drop for Child {
    fn drop(&mut self) {
        unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                self.pidfd.as_raw_fd(),
                libc::SIGKILL,
                std::ptr::null::<libc::siginfo_t>(),
                0,
            );
            libc::waitpid(self.pid, std::ptr::null_mut(), 0);
        }
    }
}
fn cstr(path: impl AsRef<OsStr>) -> io::Result<CString> {
    CString::new(path.as_ref().as_bytes()).map_err(io::Error::other)
}
fn checked(value: libc::c_long) -> io::Result<libc::c_long> {
    if value < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(value)
    }
}
fn open_path(path: &Path) -> io::Result<OwnedFd> {
    let path = cstr(path)?;
    let fd = checked(unsafe { libc::open(path.as_ptr(), libc::O_PATH | libc::O_CLOEXEC) } as _)?;
    Ok(unsafe { OwnedFd::from_raw_fd(fd as _) })
}

fn scratch_file(directory: &std::fs::File, name: &std::ffi::CStr, bytes: &[u8]) -> io::Result<()> {
    let fd = checked(unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK,
            0o600,
        )
    } as _)?;
    let mut file = unsafe { std::fs::File::from_raw_fd(fd as _) };
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.nlink() != 1 {
        return Err(io::Error::other(
            "scratch file must be a regular file with one link",
        ));
    }
    file.set_len(0)?;
    file.write_all(bytes)
}
#[repr(C)]
struct Rule {
    rights: u64,
    fd: i32,
    _padding: u32,
}
#[repr(C)]
struct Attr {
    fs: u64,
    net: u64,
    scope: u64,
}
struct Mount {
    mask: bool,
    ensure: bool,
    file: bool,
    source: CString,
    target: CString,
    kind: Option<CString>,
    flags: libc::c_ulong,
}
struct Boot {
    error: RawFd,
    close_first: Vec<RawFd>,
    fds: [RawFd; 4],
    ruleset: OwnedFd,
    rules: Vec<Rule>,
    _fences: Vec<OwnedFd>,
    fresh_rules: Vec<(CString, u64)>,
    mounts: Vec<Mount>,
    trees: Vec<RawFd>,
    executable: CString,
    arguments: Vec<*const libc::c_char>,
    _argument_storage: Vec<CString>,
    environment: Vec<*const libc::c_char>,
    _environment_storage: Vec<CString>,
    uid_map: Vec<u8>,
    gid_map: Vec<u8>,
    filter: Vec<libc::sock_filter>,
    net: NetMode,
    directory: CString,
    _spec: std::fs::File,
    command: bool,
}
// Boot is only read by the clone child, which has a private address space.
unsafe impl Send for Boot {}

pub fn probe(policy: &Policy) -> Result<(), String> {
    let abi = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            std::ptr::null::<u8>(),
            0,
            1,
        )
    };
    if abi < 6 {
        return Err("Linux Landlock ABI v6 (kernel >= 6.12) is required".into());
    }
    // HardRequirement prevents feature-dependent degradation on newer or older kernels.
    Ruleset::default()
        .set_compatibility(CompatLevel::HardRequirement)
        .handle_access(AccessFs::from_all(ABI::V6))
        .and_then(|r| r.create())
        .map_err(|e| e.to_string())?;
    if policy.network.mode.mediated() {
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/net/tun")
            .map_err(|e| format!("/dev/net/tun: {e}"))?;
    }
    let mut stack = vec![0u8; 64 * 1024];
    extern "C" fn exit_probe(_: *mut libc::c_void) -> i32 {
        unsafe { libc::_exit(0) }
    }
    let flags = libc::CLONE_NEWUSER
        | libc::CLONE_NEWNS
        | libc::SIGCHLD
        | if policy.network.mode != NetMode::Host {
            libc::CLONE_NEWNET
        } else {
            0
        }
        | if policy.process_isolation == ProcessIsolation::Full {
            libc::CLONE_NEWPID
        } else {
            0
        };
    let pid = unsafe {
        libc::clone(
            exit_probe,
            stack.as_mut_ptr().add(stack.len()).cast(),
            flags,
            std::ptr::null_mut(),
        )
    };
    if pid < 0 {
        return Err(format!(
            "required user/mount namespaces: {}",
            io::Error::last_os_error()
        ));
    }
    if unsafe { libc::waitpid(pid, std::ptr::null_mut(), 0) } < 0 {
        return Err(io::Error::last_os_error().to_string());
    }
    Ok(())
}

pub fn spawn(spec: RuntimeSpec) -> Result<Child, String> {
    spawn_inner(
        spec,
        &std::env::current_exe().map_err(crate::refusal)?,
        None,
    )
    .map_err(crate::refusal)
}
pub fn spawn_at(spec: RuntimeSpec, executable: &Path) -> Result<Child, String> {
    spawn_inner(spec, executable, None).map_err(crate::refusal)
}
/// Start this executable's trusted `__command` entry point with inherited stdio.
/// The entry point must finish runtime isolation before executing the arguments;
/// fd 7 is its CLOEXEC child table, and fds 3–6 retain the runner contract.
pub fn spawn_command(spec: RuntimeSpec, arguments: &[OsString]) -> Result<Child, String> {
    spawn_inner(
        spec,
        &std::env::current_exe().map_err(crate::refusal)?,
        Some(arguments),
    )
    .map_err(crate::refusal)
}
fn spawn_inner(
    spec: RuntimeSpec,
    executable_path: &Path,
    command: Option<&[OsString]>,
) -> io::Result<Child> {
    probe(&spec.policy).map_err(io::Error::other)?;
    std::fs::create_dir_all(&spec.scratch)?;
    // Tools can write their scratch directory. Pin it and refuse symlinks or
    // hard links before touching files left by an earlier runner.
    let scratch = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&spec.scratch)?;
    std::fs::create_dir_all(&spec.spill)?;
    let (control, child_control) = wire::socketpair()?;
    let packet_pair = if spec.policy.network.mode.mediated() {
        Some(wire::socketpair()?)
    } else {
        None
    };
    let mut spec_file = unsafe {
        std::fs::File::from_raw_fd(checked(libc::syscall(
            libc::SYS_memfd_create,
            c"ocmini-jail".as_ptr(),
            libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
        ))? as _)
    };
    spec_file.write_all(&serde_json::to_vec(&spec).map_err(io::Error::other)?)?;
    checked(unsafe {
        libc::fcntl(
            spec_file.as_raw_fd(),
            libc::F_ADD_SEALS,
            libc::F_SEAL_WRITE | libc::F_SEAL_GROW | libc::F_SEAL_SHRINK | libc::F_SEAL_SEAL,
        )
    } as _)?;
    // Reopen read-only before inheriting. No tool can mutate the sealed spec.
    let spec_file = std::fs::File::open(format!("/proc/self/fd/{}", spec_file.as_raw_fd()))?;
    let all = AccessFs::from_all(ABI::V6).bits();
    let read = (AccessFs::ReadFile | AccessFs::ReadDir).bits();
    let write = AccessFs::from_write(ABI::V6).bits();
    let mut fences = Vec::new();
    let mut rules = Vec::new();
    for (paths, rights) in [
        (&spec.policy.fs.readable, read),
        (&spec.policy.fs.writable, write),
        (&spec.policy.fs.exec, AccessFs::Execute as u64),
    ] {
        for path in paths {
            let fd = open_path(path)?;
            let rights = if path.is_file() {
                rights & AccessFs::from_file(ABI::V6).bits()
            } else {
                rights
            };
            rules.push(Rule {
                rights,
                fd: fd.as_raw_fd(),
                _padding: 0,
            });
            fences.push(fd);
        }
    }
    for path in [&spec.scratch, &spec.spill] {
        let fd = open_path(path)?;
        rules.push(Rule {
            rights: (read | write) & all,
            fd: fd.as_raw_fd(),
            _padding: 0,
        });
        fences.push(fd);
    }
    for path in ["/dev/null", "/dev/zero", "/dev/net/tun"] {
        let path = Path::new(path);
        if path.exists() {
            let fd = open_path(path)?;
            rules.push(Rule {
                rights: (read | write) & AccessFs::from_file(ABI::V6).bits(),
                fd: fd.as_raw_fd(),
                _padding: 0,
            });
            fences.push(fd);
        }
    }
    // The runtime executable is machinery, not a general exec directory.
    let executable_path = executable_path.to_owned();
    let executable_fd = open_path(&executable_path)?;
    rules.push(Rule {
        rights: (read | AccessFs::Execute as u64) & AccessFs::from_file(ABI::V6).bits(),
        fd: executable_fd.as_raw_fd(),
        _padding: 0,
    });
    fences.push(executable_fd);
    let scope = (if spec.policy.unix_sockets == UnixSockets::Abstract {
        1
    } else {
        0
    }) | (if spec.policy.process_isolation != ProcessIsolation::None {
        2
    } else {
        0
    });
    let attr = Attr {
        fs: all,
        net: 0,
        scope,
    };
    let ruleset = unsafe {
        OwnedFd::from_raw_fd(checked(libc::syscall(
            libc::SYS_landlock_create_ruleset,
            &attr,
            std::mem::size_of::<Attr>(),
            0,
        ))? as _)
    };
    let mut mounts = vec![Mount {
        mask: false,
        ensure: false,
        file: false,
        source: cstr("")?,
        target: cstr("/")?,
        kind: None,
        flags: libc::MS_REC | libc::MS_PRIVATE,
    }];
    let mut fresh_rules = Vec::new();
    if spec.policy.tmp {
        mounts.push(Mount {
            mask: false,
            ensure: false,
            file: false,
            source: cstr("tmpfs")?,
            target: cstr("/tmp")?,
            kind: Some(cstr("tmpfs")?),
            flags: libc::MS_NOSUID | libc::MS_NODEV,
        });
        fresh_rules.push((cstr("/tmp")?, read | write));
    }
    if spec.policy.process_isolation == ProcessIsolation::Full {
        mounts.push(Mount {
            mask: false,
            ensure: false,
            file: false,
            source: cstr("proc")?,
            target: cstr("/proc")?,
            kind: Some(cstr("proc")?),
            flags: libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC,
        });
        if spec
            .policy
            .fs
            .readable
            .iter()
            .any(|p| Path::new("/proc").starts_with(p))
        {
            fresh_rules.push((cstr("/proc")?, read));
        }
    }
    // A private /tmp must not hide explicitly granted projects/toolchains or
    // machinery directories stored under /tmp (including common test setups).
    if spec.policy.tmp {
        let writable: Vec<PathBuf> = spec
            .policy
            .fs
            .writable
            .iter()
            .chain([&spec.scratch, &spec.spill])
            .map(|p| p.canonicalize())
            .collect::<io::Result<_>>()?;
        let mut paths: Vec<PathBuf> = spec
            .policy
            .fs
            .readable
            .iter()
            .chain(&spec.policy.fs.writable)
            .chain(&spec.policy.fs.exec)
            .cloned()
            .chain([
                spec.directory.clone(),
                spec.scratch.clone(),
                spec.spill.clone(),
            ])
            .map(|p| p.canonicalize())
            .collect::<io::Result<_>>()?;
        paths.retain(|p| p.starts_with("/tmp") && p != Path::new("/tmp"));
        paths.sort_by_key(|p| (p.components().count(), p.clone()));
        let mut bound: Vec<(PathBuf, bool)> = vec![];
        for path in paths {
            let can_write = writable.iter().any(|p| path.starts_with(p));
            if bound
                .iter()
                .any(|(p, write)| path.starts_with(p) && (*write || !can_write))
            {
                continue;
            }
            // Only restore a working directory if the policy already grants it.
            if path == spec.directory
                && !spec.policy.fs.readable.iter().any(|r| path.starts_with(r))
            {
                continue;
            }
            let fd = open_path(&path)?;
            mounts.push(Mount {
                mask: false,
                ensure: true,
                file: path.is_file(),
                source: cstr(format!("/proc/self/fd/{}", fd.as_raw_fd()))?,
                target: cstr(path.as_os_str())?,
                kind: None,
                // Landlock's /tmp write grant also covers bind mounts below it.
                // Protect read-only host trees at the mount, restoring explicit
                // writable descendants as separate trees afterward.
                flags: libc::MS_BIND | libc::MS_REC | if can_write { 0 } else { libc::MS_RDONLY },
            });
            fences.push(fd);
            bound.push((path, can_write));
        }
    }
    if spec.policy.network.mode != NetMode::Host {
        let resolver = spec.scratch.join("resolv.conf");
        scratch_file(
            &scratch,
            c"resolv.conf",
            if spec.policy.network.mode.mediated() {
                b"nameserver 198.18.0.2\noptions ndots:0\n"
            } else {
                b"nameserver 127.0.0.1\noptions ndots:0\n"
            },
        )?;
        // Resolve the host symlink before mounts (e.g. systemd-resolved).
        mounts.push(Mount {
            mask: false,
            ensure: false,
            file: false,
            source: cstr(resolver.as_os_str())?,
            target: cstr(std::fs::canonicalize("/etc/resolv.conf")?.as_os_str())?,
            kind: None,
            flags: libc::MS_BIND,
        });
    }
    if !spec.policy.fs.hide.is_empty() {
        let empty_dir = spec.scratch.join("mask-dir");
        let empty_file = spec.scratch.join("mask-file");
        std::fs::create_dir_all(&empty_dir)?;
        scratch_file(&scratch, c"mask-file", &[])?;
        mounts.push(Mount {
            mask: true,
            ensure: false,
            file: false,
            source: cstr("tmpfs")?,
            target: cstr(empty_dir.as_os_str())?,
            kind: Some(cstr("tmpfs")?),
            flags: libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC,
        });
        let mut hidden = spec
            .policy
            .fs
            .hide
            .iter()
            .map(|p| p.canonicalize())
            .collect::<io::Result<Vec<_>>>()?;
        hidden.sort_by_key(|p| (p.components().count(), p.clone()));
        let mut masked: Vec<PathBuf> = vec![];
        for target in hidden {
            if masked.iter().any(|root| target.starts_with(root)) {
                continue;
            }
            let source = if target.is_dir() {
                &empty_dir
            } else {
                &empty_file
            };
            mounts.push(Mount {
                mask: true,
                ensure: false,
                file: false,
                source: cstr(source.as_os_str())?,
                target: cstr(target.as_os_str())?,
                kind: None,
                flags: libc::MS_BIND,
            });
            masked.push(target);
        }
    }
    let mut error_pipe = [0; 2];
    checked(unsafe { libc::pipe2(error_pipe.as_mut_ptr(), libc::O_CLOEXEC) } as _)?;
    let error_read = unsafe { OwnedFd::from_raw_fd(error_pipe[0]) };
    let error_write = unsafe { OwnedFd::from_raw_fd(error_pipe[1]) };
    // Move pinned endpoints above the reserved range before dup2 in the child.
    let pin = |fd| -> io::Result<OwnedFd> {
        Ok(unsafe {
            OwnedFd::from_raw_fd(checked(libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 16) as _)? as _)
        })
    };
    let mut pinned = vec![
        pin(child_control.as_raw_fd())?,
        pin(spec_file.as_raw_fd())?,
        pin(error_write.as_raw_fd())?,
    ];
    let fds = [pinned[0].as_raw_fd(), -1, -1, pinned[1].as_raw_fd()];
    let mut boot = Boot {
        error: pinned[2].as_raw_fd(),
        close_first: std::iter::once(control.as_raw_fd())
            .chain(packet_pair.as_ref().map(|(parent, _)| parent.as_raw_fd()))
            .chain(std::iter::once(error_read.as_raw_fd()))
            .collect(),
        fds,
        ruleset,
        rules,
        _fences: fences,
        fresh_rules,
        trees: vec![-1; mounts.len()],
        mounts,
        executable: cstr(executable_path.as_os_str())?,
        arguments: vec![],
        _argument_storage: std::iter::once(executable_path.as_os_str())
            .chain(std::iter::once(OsStr::new(if command.is_some() {
                "__command"
            } else {
                "__runner"
            })))
            .chain(command.into_iter().flatten().map(OsString::as_os_str))
            .map(cstr)
            .collect::<io::Result<_>>()?,
        environment: vec![],
        _environment_storage: vec![],
        uid_map: format!("0 {} 1\n", unsafe { libc::getuid() }).into_bytes(),
        gid_map: format!("0 {} 1\n", unsafe { libc::getgid() }).into_bytes(),
        filter: filter(UnixSockets::Off, spec.policy.io_uring),
        net: spec.policy.network.mode,
        directory: cstr(spec.directory.as_os_str())?,
        _spec: spec_file,
        command: command.is_some(),
    };
    if let Some((_, child)) = &packet_pair {
        pinned.push(pin(child.as_raw_fd())?);
        boot.fds[1] = pinned.last().unwrap().as_raw_fd();
    }
    boot.arguments = boot
        ._argument_storage
        .iter()
        .map(|s| s.as_ptr())
        .chain(std::iter::once(std::ptr::null()))
        .collect();
    for (key, value) in spec
        .policy
        .environment(std::env::vars_os())
        .map_err(io::Error::other)?
    {
        let mut bytes = key.as_bytes().to_vec();
        bytes.push(b'=');
        bytes.extend_from_slice(value.as_bytes());
        boot._environment_storage
            .push(CString::new(bytes).map_err(io::Error::other)?);
    }
    boot.environment = boot
        ._environment_storage
        .iter()
        .map(|s| s.as_ptr())
        .chain(std::iter::once(std::ptr::null()))
        .collect();
    let mut stack = vec![0u8; 1024 * 1024];
    let flags = libc::CLONE_NEWUSER
        | libc::CLONE_NEWNS
        | libc::SIGCHLD
        | if spec.policy.network.mode != NetMode::Host {
            libc::CLONE_NEWNET
        } else {
            0
        }
        | if spec.policy.process_isolation == ProcessIsolation::Full {
            libc::CLONE_NEWPID
        } else {
            0
        };
    let pid = checked(unsafe {
        libc::clone(
            child_boot,
            stack.as_mut_ptr().add(stack.len()).cast(),
            flags,
            (&mut boot as *mut Boot).cast(),
        )
    } as _)? as i32;
    let pidfd_result = checked(unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) });
    drop(error_write);
    drop(pinned);
    drop(boot);
    let pidfd = match pidfd_result {
        Ok(fd) => unsafe { OwnedFd::from_raw_fd(fd as _) },
        Err(e) => {
            unsafe {
                libc::kill(pid, libc::SIGKILL);
                libc::waitpid(pid, std::ptr::null_mut(), 0);
            }
            return Err(e);
        }
    };
    let mut error = Vec::new();
    std::fs::File::from(error_read).read_to_end(&mut error)?;
    if !error.is_empty() {
        unsafe {
            libc::waitpid(pid, std::ptr::null_mut(), 0);
        }
        return Err(io::Error::other(format!(
            "jail boot stage {}: {}",
            error[0],
            io::Error::from_raw_os_error(i32::from_ne_bytes(
                error
                    .get(1..5)
                    .and_then(|s| s.try_into().ok())
                    .unwrap_or([0; 4])
            ))
        )));
    }
    Ok(Child {
        pid,
        pidfd,
        control,
        packets: packet_pair.map(|(parent, _)| parent),
    })
}

extern "C" fn child_boot(data: *mut libc::c_void) -> i32 {
    let boot = unsafe { &mut *(data as *mut Boot) };
    unsafe { boot.run() }
}
impl Boot {
    unsafe fn fail(&self, stage: u8) -> ! {
        let errno = unsafe { *libc::__errno_location() }.to_ne_bytes();
        let bytes = [stage, errno[0], errno[1], errno[2], errno[3]];
        unsafe {
            libc::write(self.error, bytes.as_ptr().cast(), bytes.len());
            libc::_exit(crate::REFUSAL);
        }
    }
    unsafe fn check(&self, stage: u8, result: libc::c_long) {
        if result < 0 {
            unsafe { self.fail(stage) }
        }
    }
    unsafe fn write(&self, path: *const libc::c_char, bytes: &[u8]) {
        let fd = unsafe { libc::open(path, libc::O_WRONLY | libc::O_CLOEXEC) };
        unsafe {
            self.check(1, fd as _);
            self.check(1, libc::write(fd, bytes.as_ptr().cast(), bytes.len()) as _);
            libc::close(fd);
        }
    }
    unsafe fn run(&mut self) -> i32 {
        unsafe {
            for fd in &self.close_first {
                libc::close(*fd);
            }
            self.check(0, libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) as _);
            // Close the brain-death race before any effects in the jail.
            let mut poll = libc::pollfd {
                fd: self.fds[0],
                events: 0,
                revents: 0,
            };
            self.check(0, libc::poll(&mut poll, 1, 0) as _);
            if poll.revents & libc::POLLHUP != 0 {
                self.fail(0);
            }
            self.write(c"/proc/self/setgroups".as_ptr(), b"deny");
            self.write(c"/proc/self/uid_map".as_ptr(), &self.uid_map);
            self.write(c"/proc/self/gid_map".as_ptr(), &self.gid_map);
            // A UID-map change resets PDEATHSIG; install again after mapping.
            self.check(0, libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) as _);
            self.check(
                2,
                libc::mount(
                    c"".as_ptr(),
                    c"/".as_ptr(),
                    std::ptr::null(),
                    libc::MS_REC | libc::MS_PRIVATE,
                    std::ptr::null(),
                ) as _,
            );
            // Clone retained trees while the original paths still belong to the
            // current mount namespace. A pre-clone O_PATH fd refers to the
            // parent's mount and cannot itself be used by mount(MS_BIND).
            for index in 0..self.mounts.len() {
                if self.mounts[index].ensure {
                    let fd = libc::syscall(
                        libc::SYS_open_tree,
                        libc::AT_FDCWD,
                        self.mounts[index].target.as_ptr(),
                        1u32 | libc::O_CLOEXEC as u32 | 0x8000u32,
                    );
                    self.check(2, fd);
                    self.trees[index] = fd as i32;
                    if self.mounts[index].flags & libc::MS_RDONLY != 0 {
                        let attr = libc::mount_attr {
                            attr_set: libc::MOUNT_ATTR_RDONLY,
                            attr_clr: 0,
                            propagation: 0,
                            userns_fd: 0,
                        };
                        self.check(
                            2,
                            libc::syscall(
                                libc::SYS_mount_setattr,
                                fd,
                                c"".as_ptr(),
                                libc::AT_EMPTY_PATH | 0x8000,
                                &attr,
                                std::mem::size_of::<libc::mount_attr>(),
                            ),
                        );
                    }
                }
            }
            let mut children_fd = -1;
            for (mount_index, mount) in self.mounts.iter().enumerate().skip(1) {
                if mount.mask {
                    continue;
                }
                if mount.ensure {
                    let bytes = mount.target.as_bytes();
                    let mut path = [0 as libc::c_char; libc::PATH_MAX as usize];
                    for (index, byte) in bytes.iter().enumerate() {
                        if *byte == b'/' && index > 0 {
                            self.check(
                                2,
                                if libc::mkdir(path.as_ptr(), 0o700) < 0
                                    && *libc::__errno_location() != libc::EEXIST
                                {
                                    -1
                                } else {
                                    0
                                },
                            );
                        }
                        path[index] = *byte as libc::c_char;
                    }
                    if mount.file {
                        let fd = libc::open(
                            path.as_ptr(),
                            libc::O_CREAT | libc::O_EXCL | libc::O_WRONLY | libc::O_CLOEXEC,
                            0o600,
                        );
                        if fd < 0 {
                            if *libc::__errno_location() != libc::EEXIST {
                                self.fail(2);
                            }
                        } else {
                            libc::close(fd);
                        }
                    } else {
                        self.check(
                            2,
                            if libc::mkdir(path.as_ptr(), 0o700) < 0
                                && *libc::__errno_location() != libc::EEXIST
                            {
                                -1
                            } else {
                                0
                            },
                        );
                    }
                }
                if self.trees[mount_index] >= 0 {
                    self.check(
                        2,
                        libc::syscall(
                            libc::SYS_move_mount,
                            self.trees[mount_index],
                            c"".as_ptr(),
                            libc::AT_FDCWD,
                            mount.target.as_ptr(),
                            4u32,
                        ),
                    );
                    libc::close(self.trees[mount_index]);
                    continue;
                }
                self.check(
                    (20 + mount_index).min(255) as u8,
                    libc::mount(
                        mount.source.as_ptr(),
                        mount.target.as_ptr(),
                        mount.kind.as_ref().map_or(std::ptr::null(), |s| s.as_ptr()),
                        mount.flags,
                        std::ptr::null(),
                    ) as _,
                );
                if !self.command
                    && mount
                        .kind
                        .as_ref()
                        .is_some_and(|kind| kind.as_bytes() == b"proc")
                {
                    let fd = libc::open(
                        c"/proc/self/task/1/children".as_ptr(),
                        libc::O_RDONLY | libc::O_CLOEXEC,
                    );
                    self.check(2, fd as _);
                    children_fd = libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 16);
                    self.check(2, children_fd as _);
                    libc::close(fd);
                }
            }
            if self.command {
                let fd = libc::open(
                    c"/proc/thread-self/children".as_ptr(),
                    libc::O_RDONLY | libc::O_CLOEXEC,
                );
                self.check(2, fd as _);
                children_fd = libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 16);
                self.check(2, children_fd as _);
                libc::close(fd);
            }
            let mut fds = self.fds;
            if self.net.mediated() {
                // TUN captures the opener's netns. Open here, never in the brain.
                let fd = libc::open(c"/dev/net/tun".as_ptr(), libc::O_RDWR | libc::O_CLOEXEC);
                self.check(3, fd as _);
                fds[2] = libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 16);
                self.check(3, fds[2] as _);
                libc::close(fd);
            }
            if self.net != NetMode::Host {
                self.check(3, configure_net(fds[2], self.net.mediated()) as _);
            }
            // Network bootstrap needs the fresh proc sysctls; apply user masks
            // afterwards, before Landlock and all model-requested work.
            for mount in self.mounts.iter().filter(|m| m.mask) {
                self.check(
                    2,
                    libc::mount(
                        mount.source.as_ptr(),
                        mount.target.as_ptr(),
                        mount.kind.as_ref().map_or(std::ptr::null(), |s| s.as_ptr()),
                        mount.flags,
                        std::ptr::null(),
                    ) as _,
                );
            }

            for rule in &self.rules {
                self.check(
                    4,
                    libc::syscall(
                        libc::SYS_landlock_add_rule,
                        self.ruleset.as_raw_fd(),
                        1,
                        rule,
                        0,
                    ),
                );
            }
            // Fresh mounts have new inodes; their grants must bind the final layout.
            for (path, rights) in &self.fresh_rules {
                let fd = libc::open(path.as_ptr(), libc::O_PATH | libc::O_CLOEXEC);
                self.check(4, fd as _);
                let rule = Rule {
                    rights: *rights,
                    fd,
                    _padding: 0,
                };
                self.check(
                    4,
                    libc::syscall(
                        libc::SYS_landlock_add_rule,
                        self.ruleset.as_raw_fd(),
                        1,
                        &rule,
                        0,
                    ),
                );
                libc::close(fd);
            }
            self.check(6, libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) as _);
            self.check(
                4,
                libc::syscall(
                    libc::SYS_landlock_restrict_self,
                    self.ruleset.as_raw_fd(),
                    0,
                ),
            );
            // Drop all bounding capabilities before clearing CAP_SETPCAP itself.
            for cap in 0..=40 {
                self.check(5, libc::prctl(libc::PR_CAPBSET_DROP, cap, 0, 0, 0) as _);
            }
            self.check(
                5,
                libc::prctl(
                    libc::PR_CAP_AMBIENT,
                    libc::PR_CAP_AMBIENT_CLEAR_ALL,
                    0,
                    0,
                    0,
                ) as _,
            );
            #[repr(C)]
            struct Header {
                version: u32,
                pid: i32,
            }
            let header = Header {
                version: 0x20080522,
                pid: 0,
            };
            let caps = [0u32; 6];
            self.check(5, libc::syscall(libc::SYS_capset, &header, caps.as_ptr()));
            let prog = libc::sock_fprog {
                len: self.filter.len() as u16,
                filter: self.filter.as_ptr() as *mut _,
            };
            self.check(7, libc::syscall(libc::SYS_seccomp, 1, 0, &prog));
            if children_fd >= 0 {
                self.check(
                    8,
                    libc::dup2(children_fd, if self.command { 7 } else { 0 }) as _,
                );
            }
            for (index, fd) in fds.iter().enumerate() {
                if *fd >= 0 {
                    self.check(8, libc::dup2(*fd, 3 + index as i32) as _);
                } else {
                    libc::close(3 + index as i32);
                }
            }
            // Preserve the standalone child table on fd 7 when requested,
            // plus the bootstrap error pipe (CLOEXEC) above the reserved range.
            self.check(
                8,
                libc::syscall(
                    libc::SYS_close_range,
                    if self.command { 8u32 } else { 7u32 },
                    (self.error - 1) as u32,
                    0,
                ),
            );
            self.check(
                8,
                libc::syscall(libc::SYS_close_range, (self.error + 1) as u32, u32::MAX, 0),
            );
            self.check(8, libc::chdir(self.directory.as_ptr()) as _);
            // Tool runners expose no stdio. The standalone command explicitly
            // inherits its caller's three streams, including terminals/pipes.
            if !self.command {
                let null = libc::open(c"/dev/null".as_ptr(), libc::O_RDWR | libc::O_CLOEXEC);
                self.check(8, null as _);
                for fd in 0..3 {
                    if fd == 0 && children_fd >= 0 {
                        continue;
                    }
                    self.check(8, libc::dup2(null, fd) as _);
                }
                libc::close(null);
            }
            libc::execve(
                self.executable.as_ptr(),
                self.arguments.as_ptr(),
                self.environment.as_ptr(),
            );
            self.fail(9);
        }
    }
}

fn stmt(code: u16, k: u32) -> libc::sock_filter {
    libc::sock_filter {
        code,
        jt: 0,
        jf: 0,
        k,
    }
}
fn jump(k: u32, jt: u8, jf: u8) -> libc::sock_filter {
    libc::sock_filter {
        code: 0x15,
        jt,
        jf,
        k,
    }
}
pub fn filter(unix: UnixSockets, io_uring: bool) -> Vec<libc::sock_filter> {
    #[cfg(target_arch = "x86_64")]
    let arch = 0xc000003e;
    #[cfg(target_arch = "aarch64")]
    let arch = 0xc00000b7;
    #[cfg(target_arch = "arm")]
    let arch = 0x40000028;
    let mut b = vec![
        stmt(0x20, 4),
        jump(arch, 1, 0),
        stmt(0x06, 0x80000000),
        stmt(0x20, 0),
    ];
    #[cfg(target_arch = "x86_64")]
    {
        b.push(libc::sock_filter {
            code: 0x35,
            jt: 0,
            jf: 1,
            k: 0x40000000,
        });
        b.push(stmt(0x06, 0x80000000));
    }
    let errno = |value| stmt(0x06, 0x00050000 | value);
    for nr in [
        libc::SYS_mount,
        libc::SYS_umount2,
        libc::SYS_pivot_root,
        libc::SYS_chroot,
        libc::SYS_unshare,
        libc::SYS_setns,
    ] {
        b.push(jump(nr as u32, 0, 1));
        b.push(errno(libc::EPERM as u32));
    }
    b.push(jump(libc::SYS_clone3 as u32, 0, 1));
    b.push(errno(libc::ENOSYS as u32));
    b.push(jump(libc::SYS_clone as u32, 0, 4));
    b.push(stmt(0x20, 16));
    b.push(libc::sock_filter {
        code: 0x45,
        jt: 0,
        jf: 1,
        k: (libc::CLONE_NEWUSER
            | libc::CLONE_NEWNS
            | libc::CLONE_NEWNET
            | libc::CLONE_NEWPID
            | libc::CLONE_NEWIPC
            | libc::CLONE_NEWUTS
            | libc::CLONE_NEWCGROUP
            | 0x80) as u32,
    });
    b.push(errno(libc::EPERM as u32));
    b.push(stmt(0x06, 0x7fff0000));
    if !io_uring {
        for nr in [
            libc::SYS_io_uring_setup,
            libc::SYS_io_uring_enter,
            libc::SYS_io_uring_register,
        ] {
            b.push(jump(nr as u32, 0, 1));
            b.push(errno(libc::EPERM as u32));
        }
    }
    for nr in [libc::SYS_socket, libc::SYS_socketpair] {
        let families: Vec<_> = [libc::AF_INET, libc::AF_INET6, libc::AF_NETLINK]
            .into_iter()
            .chain((!matches!(unix, UnixSockets::Deny)).then_some(libc::AF_UNIX))
            .collect();
        b.push(jump(nr as u32, 0, (2 + families.len() * 2) as u8));
        b.push(stmt(0x20, 16));
        for family in families {
            b.push(jump(family as u32, 0, 1));
            b.push(stmt(0x06, 0x7fff0000));
        }
        b.push(errno(libc::EPERM as u32));
    }
    b.push(stmt(0x06, 0x7fff0000));
    b
}

unsafe fn configure_net(tun: RawFd, mediated: bool) -> i32 {
    unsafe {
        let socket = libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0);
        if socket < 0 {
            return -1;
        }
        let mut req: libc::ifreq = std::mem::zeroed();
        req.ifr_name[..2].copy_from_slice(&[b'l' as _, b'o' as _]);
        if libc::ioctl(socket, libc::SIOCGIFFLAGS as libc::Ioctl, &mut req) < 0 {
            libc::close(socket);
            return -1;
        }
        req.ifr_ifru.ifru_flags |= libc::IFF_UP as i16;
        if libc::ioctl(socket, libc::SIOCSIFFLAGS as libc::Ioctl, &req) < 0 {
            libc::close(socket);
            return -1;
        }
        if !mediated {
            libc::close(socket);
            return 0;
        }
        let ping = libc::open(
            c"/proc/sys/net/ipv4/ping_group_range".as_ptr(),
            libc::O_WRONLY | libc::O_CLOEXEC,
        );
        if ping < 0 {
            libc::close(socket);
            return -1;
        }
        let result = libc::write(ping, b"0 0".as_ptr().cast(), 3);
        libc::close(ping);
        if result != 3 {
            libc::close(socket);
            return -1;
        }
        let mut req: libc::ifreq = std::mem::zeroed();
        for (a, b) in req.ifr_name.iter_mut().zip(b"octun") {
            *a = *b as _;
        }
        req.ifr_ifru.ifru_flags = (libc::IFF_TUN | libc::IFF_NO_PI) as i16;
        if libc::ioctl(tun, 0x400454ca as libc::Ioctl, &req) < 0 {
            libc::close(socket);
            return -1;
        }
        req.ifr_ifru.ifru_flags = libc::IFF_UP as i16;
        if libc::ioctl(socket, libc::SIOCSIFFLAGS as libc::Ioctl, &req) < 0 {
            libc::close(socket);
            return -1;
        }
        req.ifr_ifru.ifru_mtu = wire::MTU as i32;
        if libc::ioctl(socket, libc::SIOCSIFMTU as libc::Ioctl, &req) < 0 {
            libc::close(socket);
            return -1;
        }
        libc::close(socket);
        if netlink_setup() < 0 {
            return -1;
        }
        for (path, value) in [
            (
                c"/proc/sys/net/ipv4/conf/octun/route_localnet",
                b"1" as &[u8],
            ),
            (c"/proc/sys/net/ipv4/conf/octun/rp_filter", b"2"),
        ] {
            let fd = libc::open(path.as_ptr(), libc::O_WRONLY | libc::O_CLOEXEC);
            if fd < 0 {
                return -1;
            }
            let result = libc::write(fd, value.as_ptr().cast(), value.len());
            libc::close(fd);
            if result != value.len() as isize {
                return -1;
            }
        }
        0
    }
}
// Fixed-size rtnetlink messages keep the post-clone child allocation-free.
unsafe fn netlink_setup() -> i32 {
    unsafe {
        let fd = libc::socket(
            libc::AF_NETLINK,
            libc::SOCK_RAW | libc::SOCK_CLOEXEC,
            libc::NETLINK_ROUTE,
        );
        if fd < 0 {
            return -1;
        }
        let index = libc::if_nametoindex(c"octun".as_ptr());
        if index == 0 {
            libc::close(fd);
            return -1;
        }
        for (family, address, prefix, gateway) in [
            (
                libc::AF_INET as u8,
                &[198, 18, 0, 1][..],
                15,
                &[198, 18, 0, 2][..],
            ),
            (
                libc::AF_INET6 as u8,
                &[
                    0xfd, 0, 0x6f, 0x63, 0x6d, 0x69, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1,
                ][..],
                64,
                &[
                    0xfd, 0, 0x6f, 0x63, 0x6d, 0x69, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2,
                ][..],
            ),
        ] {
            let mut message = [0u8; 128];
            let mut len = 24;
            message[16] = family;
            message[17] = prefix;
            message[20..24].copy_from_slice(&index.to_ne_bytes());
            attribute(&mut message, &mut len, 1, address);
            attribute(&mut message, &mut len, 2, address);
            if nl_send(fd, &mut message, len, 20) < 0 {
                libc::close(fd);
                return -1;
            }
            message.fill(0);
            len = 28;
            message[16] = family;
            message[20] = 254;
            message[21] = 3;
            message[23] = 1;
            attribute(&mut message, &mut len, 5, gateway);
            attribute(&mut message, &mut len, 4, &index.to_ne_bytes());
            if nl_send(fd, &mut message, len, 24) < 0 {
                libc::close(fd);
                return -1;
            }
        }
        libc::close(fd);
        0
    }
}
fn attribute(message: &mut [u8], len: &mut usize, kind: u16, payload: &[u8]) {
    let size = payload.len() + 4;
    message[*len..*len + 2].copy_from_slice(&(size as u16).to_ne_bytes());
    message[*len + 2..*len + 4].copy_from_slice(&kind.to_ne_bytes());
    message[*len + 4..*len + size].copy_from_slice(payload);
    *len += (size + 3) & !3;
}
unsafe fn nl_send(fd: i32, message: &mut [u8], len: usize, kind: u16) -> i32 {
    message[0..4].copy_from_slice(&(len as u32).to_ne_bytes());
    message[4..6].copy_from_slice(&kind.to_ne_bytes());
    message[6..8].copy_from_slice(&0x605u16.to_ne_bytes());
    message[8..12].copy_from_slice(&1u32.to_ne_bytes());
    unsafe {
        let mut address: libc::sockaddr_nl = std::mem::zeroed();
        address.nl_family = libc::AF_NETLINK as u16;
        if libc::sendto(
            fd,
            message.as_ptr().cast(),
            len,
            0,
            (&address as *const libc::sockaddr_nl).cast(),
            std::mem::size_of_val(&address) as _,
        ) < 0
        {
            return -1;
        }
        let mut answer = [0u8; 128];
        let read = libc::recv(fd, answer.as_mut_ptr().cast(), answer.len(), 0);
        if read < 20 {
            return -1;
        }
        let err = i32::from_ne_bytes(answer[16..20].try_into().unwrap());
        if err != 0 {
            *libc::__errno_location() = -err;
            return -1;
        }
        0
    }
}

pub fn inherited_spec() -> io::Result<RuntimeSpec> {
    read_inherited_spec(false)
}

pub fn inherited_command_spec() -> io::Result<RuntimeSpec> {
    checked(unsafe { libc::fcntl(7, libc::F_SETFD, libc::FD_CLOEXEC) } as _)?;
    read_inherited_spec(true)
}

fn read_inherited_spec(command: bool) -> io::Result<RuntimeSpec> {
    for fd in [3, 4, 5, 6] {
        if unsafe { libc::fcntl(fd, libc::F_GETFD) } >= 0 {
            checked(unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } as _)?;
        }
    }
    let file = unsafe { std::fs::File::from_raw_fd(6) };
    let spec: RuntimeSpec =
        serde_json::from_reader(std::io::BufReader::new(file)).map_err(io::Error::other)?;
    if !command && spec.policy.process_isolation == ProcessIsolation::Full {
        checked(unsafe { libc::fcntl(0, libc::F_SETFD, libc::FD_CLOEXEC) } as _)?;
    }
    Ok(spec)
}

/// Tokio's trusted signal driver creates its CLOEXEC self-pipe during runtime
/// construction. Complete the UNIX-constructor gate before dispatching tools.
pub fn finish_runtime(policy: &Policy) -> io::Result<()> {
    // The trusted runner holds control/TUN/init descriptors. Prevent its tool
    // children from extracting them through ptrace or pidfd_getfd, even when
    // the host allows same-uid tracing. Ordinary tool execs regain dumpability.
    checked(unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) } as _)?;
    if policy.unix_sockets != UnixSockets::Deny {
        return Ok(());
    }
    let filters = filter(policy.unix_sockets, policy.io_uring);
    let prog = libc::sock_fprog {
        len: filters.len() as u16,
        filter: filters.as_ptr() as *mut _,
    };
    let result = checked(unsafe { libc::syscall(libc::SYS_seccomp, 1, 1, &prog) })?;
    if result != 0 {
        return Err(io::Error::other("could not synchronize syscall policy"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reused_scratch_files_cannot_overwrite_symlink_or_hard_link_targets() {
        let root = tempfile::tempdir().unwrap();
        let directory = std::fs::File::open(root.path()).unwrap();
        let victim = root.path().join("victim");
        std::fs::write(&victim, "KEEP").unwrap();
        std::os::unix::fs::symlink(&victim, root.path().join("resolv.conf")).unwrap();
        assert!(scratch_file(&directory, c"resolv.conf", b"replace").is_err());
        std::fs::hard_link(&victim, root.path().join("mask-file")).unwrap();
        assert!(scratch_file(&directory, c"mask-file", b"").is_err());
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "KEEP");
        scratch_file(&directory, c"new-file", b"long original").unwrap();
        scratch_file(&directory, c"new-file", b"new").unwrap();
        assert_eq!(std::fs::read(root.path().join("new-file")).unwrap(), b"new");
    }
}
