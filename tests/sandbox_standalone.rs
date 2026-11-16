#![cfg(all(feature = "sandbox", target_os = "linux"))]

use std::{
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, UdpSocket},
    os::unix::{ffi::OsStringExt, process::CommandExt},
    process::{Command, Stdio},
};

struct Sandbox(tempfile::TempDir);
impl Sandbox {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("project")).unwrap();
        std::fs::write(root.path().join("secret"), "HOST_SECRET").unwrap();
        Self(root)
    }
    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ocmini-sandbox"));
        command
            .env("XDG_CONFIG_HOME", self.0.path().join("config"))
            .env("TMPDIR", self.0.path())
            .env_remove("OCMINI_SANDBOX_PRESET")
            .env("OCMINI_STANDALONE_SECRET", "MUST_NOT_LEAK")
            .args(["-C".as_ref(), self.0.path().join("project").as_os_str()]);
        command
    }
}

#[test]
fn stdio_arguments_exit_codes_and_cleanup_work_in_each_process_mode() {
    for preset in ["read-only", "locked-read"] {
        let sandbox = Sandbox::new();
        let mut child = sandbox.command()
            .args(["--preset", preset, "--sandbox-set", "unix_sockets=deny", "--", "sh", "-c",
                "read -r line; printf '%s|%s|%s|%s' \"$line\" \"$1\" \"$2\" \"${OCMINI_STANDALONE_SECRET-unset}\"; printf STDERR >&2; sh -c 'sleep 60 &' ; exit 23",
                "sh", "argument with spaces", "--help"])
            .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(b"piped input\n")
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert_eq!(output.status.code(), Some(23), "{output:?}");
        assert_eq!(
            output.stdout,
            b"piped input|argument with spaces|--help|unset"
        );
        assert_eq!(output.stderr, b"STDERR");
        assert!(!sandbox.0.path().join("project/ocmini.db").exists());
        assert!(!sandbox.0.path().join("config/ocmini").exists());
        assert!(std::fs::read_dir(sandbox.0.path())
            .unwrap()
            .all(|entry| !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("ocmini-sandbox-")));
    }
}

#[test]
fn filesystem_policies_overrides_and_command_failures_are_enforced() {
    let sandbox = Sandbox::new();
    let output = sandbox
        .command()
        .args([
            "--",
            "sh",
            "-c",
            "if cat ../secret; then exit 90; fi; if touch denied; then exit 91; fi; pwd",
        ])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(!String::from_utf8_lossy(&output.stdout).contains("HOST_SECRET"));
    assert!(!sandbox.0.path().join("project/denied").exists());
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        sandbox.0.path().join("project").to_str().unwrap()
    );

    let output = sandbox
        .command()
        .args([
            "--preset",
            "workspace-write",
            "--sandbox-network",
            "isolated",
            "--sandbox-read",
            "${workspace}/../secret",
            "--",
            "true",
        ])
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(78),
        "traversing grants must refuse startup: {output:?}"
    );
    let output = sandbox
        .command()
        .args([
            "--preset",
            "workspace-write",
            "--sandbox-network",
            "isolated",
            "--",
            "sh",
            "-c",
            "printf saved > allowed; head -c 1048576 /dev/zero",
        ])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(output.stdout.len(), 1048576);
    assert_eq!(
        std::fs::read(sandbox.0.path().join("project/allowed")).unwrap(),
        b"saved"
    );

    let output = sandbox
        .command()
        .args(["--", "ocmini-no-such-command"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(127), "{output:?}");
    let output = sandbox
        .command()
        .args([
            "--sandbox-set",
            "fs.exec=[\"/lib\"]",
            "--",
            "/bin/sh",
            "-c",
            "echo ESCAPED",
        ])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(126), "{output:?}");
    assert!(output.stdout.is_empty());
    let output = sandbox
        .command()
        .args(["--preset", "no-such-preset", "--", "touch", "escaped"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(78), "{output:?}");
    assert!(!sandbox.0.path().join("project/escaped").exists());
}

#[test]
fn command_does_not_inherit_supervisor_or_callers_extra_descriptors() {
    let sandbox = Sandbox::new();
    let mut command = sandbox.command();
    // fd 99 must be removed by the jail even though the caller passes it in.
    unsafe {
        command.pre_exec(|| {
            if libc::dup2(0, 99) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let output = command.args(["--sandbox-set", "process_isolation=full", "--", "python3", "-c",
        "import os\nfor fd in [3,4,5,6,7,99]:\n try: os.fstat(fd)\n except OSError: continue\n raise Exception('leaked fd %d' % fd)"]).output().unwrap();
    assert!(output.status.success(), "{output:?}");
}

#[test]
fn signals_are_forwarded_and_signal_exit_status_is_preserved() {
    let sandbox = Sandbox::new();
    let output = sandbox
        .command()
        .args(["--", "sh", "-c", "kill -TERM $$"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(143), "{output:?}");
    let mut child = sandbox
        .command()
        .args([
            "--preset",
            "locked-read",
            "--",
            "sh",
            "-c",
            "trap 'exit 42' TERM; echo READY; while :; do sleep 60; done",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut output = BufReader::new(child.stdout.take().unwrap());
    let mut ready = String::new();
    output.read_line(&mut ready).unwrap();
    assert_eq!(ready, "READY\n");
    assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGTERM) }, 0);
    let result = child.wait_with_output().unwrap();
    assert_eq!(result.status.code(), Some(42), "{result:?}");
}

#[test]
fn off_preserves_non_utf8_arguments_and_the_host_environment() {
    let sandbox = Sandbox::new();
    let argument = std::ffi::OsString::from_vec(b"raw\xff --help".to_vec());
    let output = sandbox
        .command()
        .args([
            "--preset",
            "off",
            "--",
            "sh",
            "-c",
            "printf '%s|%s' \"$1\" \"$OCMINI_STANDALONE_SECRET\"",
            "sh",
        ])
        .arg(argument)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(output.stdout, b"raw\xff --help|MUST_NOT_LEAK");
}

#[test]
fn mediated_networking_runs_without_ocmini() {
    let sandbox = Sandbox::new();
    let server = TcpListener::bind("0.0.0.0:0").unwrap();
    let port = server.local_addr().unwrap().port();
    let route = UdpSocket::bind("0.0.0.0:0").unwrap();
    route.connect("192.0.2.1:9").unwrap();
    let address = route.local_addr().unwrap().ip();
    let mut child = sandbox
        .command()
        .args([
            "--preset",
            "read-all-net",
            "--sandbox-set",
            "network.preview.mode=off",
            "--",
            "curl",
            "--noproxy",
            "*",
            "--max-time",
            "5",
            "-fsS",
            &format!("http://{address}:{port}/"),
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    server.set_nonblocking(true).unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let mut connection = loop {
        match server.accept() {
            Ok((stream, _)) => break stream,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if let Some(status) = child.try_wait().unwrap() {
                    panic!("sandbox exited before connecting: {status}");
                }
                assert!(std::time::Instant::now() < deadline, "connection deadline");
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(error) => panic!("{error}"),
        }
    };
    connection
        .set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .unwrap();
    let mut request = [0u8; 4096];
    assert!(connection.read(&mut request).unwrap() > 0);
    connection
        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 7\r\nConnection: close\r\n\r\nNETWORK")
        .unwrap();
    drop(connection);
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(output.stdout, b"NETWORK");
}

#[test]
fn unreachable_destinations_are_refused_before_the_guest_handshake() {
    // A destination the host cannot connect to must fail the guest connect, not
    // reset an already established connection, so clients can try another
    // address family.
    let sandbox = Sandbox::new();
    let route = UdpSocket::bind("0.0.0.0:0").unwrap();
    route.connect("192.0.2.1:9").unwrap();
    let address = route.local_addr().unwrap().ip();
    let port = TcpListener::bind((address, 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let output = sandbox
        .command()
        .args([
            "--preset",
            "read-all-net",
            "--sandbox-set",
            "network.preview.mode=off",
            "--",
            "curl",
            "--noproxy",
            "*",
            "--max-time",
            "5",
            "-sS",
            &format!("http://{address}:{port}/"),
        ])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(7), "{output:?}");
}

#[test]
fn terminal_input_interrupts_and_foreground_restoration_work() {
    let sandbox = Sandbox::new();
    let script = r#"
import errno, os, pty, select, signal, subprocess, sys, time
binary, config, project = sys.argv[1:]
for preset in ['read-only', 'locked-read']:
 for interrupt in [False, True]:
  sys.stdout.flush()
  pid, fd = pty.fork()
  if pid == 0:
   env = dict(os.environ, XDG_CONFIG_HOME=config)
   env.pop('OCMINI_SANDBOX_PRESET', None)
   command = 'echo READY; while :; do sleep 60; done' if interrupt else 'echo READY; read -r line; echo TTY:$line; exit 19'
   result = subprocess.run([binary, '-C', project, '--preset', preset,
       '--sandbox-set', 'unix_sockets=deny', '--', 'sh', '-c', command], env=env)
   restored = os.tcgetpgrp(0) == os.getpgrp()
   print('RESULT:%d:%s' % (result.returncode, restored), flush=True)
   os._exit(0)
  data, sent = b'', False
  deadline = time.monotonic() + 10
  try:
   while True:
    assert time.monotonic() < deadline, repr(data)
    if not select.select([fd], [], [], 0.1)[0]: continue
    try: chunk = os.read(fd, 4096)
    except OSError as error:
     if error.errno == errno.EIO: break
     raise
    if not chunk: break
    data += chunk
    if b'READY' in data and not sent:
     os.write(fd, b'\x03' if interrupt else b'input\n')
     sent = True
   assert (b'RESULT:130:True' if interrupt else b'RESULT:19:True') in data, repr(data)
   if not interrupt: assert b'TTY:input' in data, repr(data)
  finally:
   if os.waitpid(pid, os.WNOHANG)[0] == 0:
    os.kill(pid, signal.SIGKILL)
    os.waitpid(pid, 0)
   os.close(fd)
"#;
    let output = Command::new("python3")
        .args([
            "-c",
            script,
            env!("CARGO_BIN_EXE_ocmini-sandbox"),
            sandbox.0.path().join("config").to_str().unwrap(),
            sandbox.0.path().join("project").to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
}
