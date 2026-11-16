#![cfg(all(feature = "sandbox", target_os = "linux"))]
use ocmini_sandbox::{
    config::{self},
    jail::{self, RuntimeSpec},
    wire::{self, Control},
};
use serde_json::{json, Value};
use std::path::PathBuf;
struct Jail {
    _root: tempfile::TempDir,
    project: PathBuf,
    hidden: PathBuf,
    child: jail::Child,
    socket: tokio::net::UnixStream,
    spill: wire::Spill,
    broker: Option<tokio::task::JoinHandle<std::io::Result<()>>>,
    next: u64,
}
impl Drop for Jail {
    fn drop(&mut self) {
        if let Some(task) = self.broker.take() {
            task.abort();
        }
    }
}
impl Jail {
    async fn new(network: Value, mut extra: Value) -> Self {
        let root = tempfile::tempdir().unwrap();
        let rootpath = root.path().canonicalize().unwrap();
        let project = rootpath.join("project");
        let hidden = rootpath.join("outside");
        std::fs::create_dir(&project).unwrap();
        std::fs::write(&hidden, "HOST_SECRET").unwrap();
        let readable: Vec<_> = [
            "/usr", "/bin", "/lib", "/lib64", "/etc", "/dev", "/proc", "/sys",
        ]
        .iter()
        .map(PathBuf::from)
        .filter(|p| p.exists())
        .chain(std::iter::once(project.clone()))
        .collect();
        let exec: Vec<_> = ["/usr/bin", "/usr/lib", "/bin", "/lib", "/lib64"]
            .iter()
            .map(PathBuf::from)
            .filter(|p| p.exists())
            .collect();
        let mut preset = json!({"fs":{"readable":readable,"writable":[project],"exec":exec},"tmp":false,"process_isolation":"full","unix_sockets":"abstract","env":{"clear":true,"allow":["LANG"],"deny":[]},"network":network});
        if extra.as_object_mut().unwrap().remove("readOnlyFixtures") == Some(Value::Bool(true)) {
            std::fs::write(project.join("read-only"), "ORIGINAL").unwrap();
            std::fs::create_dir(project.join("writable")).unwrap();
            std::fs::write(project.join("writable-file"), "ORIGINAL").unwrap();
            preset["fs"]["writable"] =
                json!([project.join("writable"), project.join("writable-file")]);
        }
        if extra.as_object_mut().unwrap().remove("maskFixtures") == Some(Value::Bool(true)) {
            let dir = project.join("secret-dir");
            std::fs::create_dir(&dir).unwrap();
            std::fs::write(dir.join("secret"), "SECRET").unwrap();
            let file = project.join("secret-file");
            std::fs::write(&file, "SECRET").unwrap();
            preset["fs"]["hide"] = json!([dir, file]);
        }
        if extra.as_object_mut().unwrap().remove("hideProc") == Some(Value::Bool(true)) {
            preset["fs"]["hide"] = json!(["/proc"]);
        }
        for (k, v) in extra.as_object().unwrap() {
            preset[k] = v.clone();
        }
        let folder = rootpath.join("presets");
        std::fs::create_dir(&folder).unwrap();
        std::fs::write(folder.join("test.json"), preset.to_string()).unwrap();
        let context = config::PathContext::current(&project).unwrap();
        let policy = config::load_for(&folder, "test", &context)
            .unwrap()
            .unwrap();
        let child = jail::spawn_at(
            RuntimeSpec {
                policy: policy.clone(),
                scratch: rootpath.join("scratch"),
                spill: rootpath.join("spill"),
                directory: project.clone(),
            },
            std::path::Path::new(env!("CARGO_BIN_EXE_ocmini")),
        )
        .expect("kernel jail must be supported; no skipped enforcement tests");
        let socket = child.control.try_clone().unwrap();
        socket.set_nonblocking(true).unwrap();
        let socket = tokio::net::UnixStream::from_std(socket).unwrap();
        let broker = child.packets.as_ref().map(|p| {
            let packets = p.try_clone().unwrap();
            let pid = child.pid;
            tokio::spawn(async move {
                ocmini_sandbox::broker::run(packets, pid, policy, Default::default()).await
            })
        });
        let spill = wire::Spill::new(rootpath.join("spill")).unwrap();
        Self {
            spill,
            _root: root,
            project,
            hidden,
            child,
            socket,
            broker,
            next: 1,
        }
    }
    async fn request(&mut self, method: &str, args: Value) -> Result<Value, String> {
        let id = self.next;
        self.next += 1;
        wire::write_control(
            &mut self.socket,
            &Control::Request {
                id,
                method: method.into(),
                args,
                directory: None,
            },
        )
        .await
        .unwrap();
        match tokio::time::timeout(
            std::time::Duration::from_secs(15),
            wire::read_control(&mut self.socket),
        )
        .await
        .expect("runner response deadline")
        .unwrap()
        {
            Control::Result { id: got, value } => {
                assert_eq!(id, got);
                Ok(self.spill.unpack(value).unwrap())
            }
            Control::Error { message, .. } => Err(self
                .spill
                .unpack(message)
                .unwrap()
                .as_str()
                .unwrap()
                .to_owned()),
            frame => panic!("unexpected frame: {frame:?}"),
        }
    }
    async fn bash(&mut self, command: String) -> Value {
        self.request(
            "run",
            json!({"name":"bash","args":{"command":command,"timeout":8000}}),
        )
        .await
        .unwrap()
    }
}
#[tokio::test]
async fn private_tmp_preserves_read_only_host_trees_and_nested_write_grants() {
    let mut jail = Jail::new(
        json!({"mode":"isolated","preview":{"mode":"off"}}),
        json!({"tmp":true,"readOnlyFixtures":true}),
    )
    .await;
    let output = jail.bash("cat read-only; if printf escaped > read-only; then printf OVERWRITE_ALLOWED; fi; if touch forbidden; then printf CREATE_ALLOWED; fi; printf allowed > writable/new; printf allowed > writable-file; printf private > /tmp/private-file; cat writable/new writable-file /tmp/private-file".into()).await;
    let output = output["output"].as_str().unwrap();
    assert!(output.contains("ORIGINAL"), "{output}");
    assert!(!output.contains("_ALLOWED"), "{output}");
    assert!(output.contains("allowedallowedprivate"), "{output}");
    assert_eq!(
        std::fs::read_to_string(jail.project.join("read-only")).unwrap(),
        "ORIGINAL"
    );
    assert_eq!(
        std::fs::read_to_string(jail.project.join("writable/new")).unwrap(),
        "allowed"
    );
    assert_eq!(
        std::fs::read_to_string(jail.project.join("writable-file")).unwrap(),
        "allowed"
    );
    assert!(!jail.project.join("forbidden").exists());
}

#[tokio::test]
async fn all_file_effects_and_shell_are_kernel_fenced() {
    let mut jail = Jail::new(
        json!({"mode":"isolated","preview":{"mode":"off"}}),
        json!({}),
    )
    .await;
    let hidden = jail.hidden.display().to_string();
    let out = jail
        .bash(format!(
            "printf okay > allowed; cat '{hidden}'; printf '\\nEND\\n' "
        ))
        .await;
    assert!(
        out["output"]
            .as_str()
            .unwrap()
            .contains("Permission denied"),
        "{out}"
    );
    assert_eq!(
        std::fs::read_to_string(jail.project.join("allowed")).unwrap(),
        "okay"
    );
    let read = jail
        .request("run", json!({"name":"read","args":{"filePath":hidden}}))
        .await
        .unwrap_err();
    assert!(read.contains("Permission denied"), "{read}");
    let write = jail
        .request(
            "prepare",
            json!({"name":"write","args":{"filePath":hidden,"content":"bad"}}),
        )
        .await;
    assert!(write.is_err());
    let out=jail.bash("test $$ -gt 1; cat /proc/self/status | head -6; printf SOCKETS; python3 -c 'import socket;\ntry: socket.socket(socket.AF_VSOCK)\nexcept OSError as e: print(e.errno)'".into()).await;
    assert!(
        out["output"].as_str().unwrap().contains("SOCKETS1"),
        "{out}"
    );
}
#[tokio::test]
async fn rule_checked_outbound_uses_the_broker_and_denies_other_destinations() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let route = std::net::UdpSocket::bind("0.0.0.0:0").unwrap();
    route.connect("192.0.2.1:9").unwrap();
    let ip = route.local_addr().unwrap().ip();
    let listener = tokio::net::TcpListener::bind("0.0.0.0:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
        let (mut s, _) = listener.accept().await.unwrap();
        let mut q = [0; 4];
        s.read_exact(&mut q).await.unwrap();
        assert_eq!(&q, b"PING");
        s.write_all(b"PONG").await.unwrap();
    });
    let mut jail=Jail::new(json!({"mode":"firewall","preview":{"mode":"off"},"inbound":{"default":"deny"},"outbound":{"default":"deny","allow":[{"protocol":"tcp","destination":ip.to_string(),"port":port}]}}),json!({})).await;
    let out=jail.bash(format!("python3 -c 'import socket; s=socket.create_connection((\"{ip}\",{port}),3); s.sendall(b\"PING\"); print(s.recv(4).decode())'")).await;
    assert_eq!(out["output"], "PONG\n", "{out}");
    server.await.unwrap();
    let out=jail.bash("python3 -c 'import socket; s=socket.socket(); s.settimeout(1); print(s.connect_ex((\"192.0.2.1\",23456)))'".into()).await;
    assert!(
        !out["output"].as_str().unwrap().trim().starts_with('0'),
        "{out}"
    );
}
#[tokio::test]
async fn independent_io_uring_and_unix_policy_and_no_privileged_child_fds() {
    let mut jail = Jail::new(
        json!({"mode":"isolated","preview":{"mode":"off"}}),
        json!({"unix_sockets":"off","io_uring":false}),
    )
    .await;
    let out=jail.bash("python3 -c 'import socket,ctypes,os; socket.socketpair(); c=ctypes.CDLL(None,use_errno=True); print(c.syscall(425,2,ctypes.create_string_buffer(256)),ctypes.get_errno()); print(sorted(os.listdir(\"/proc/self/fd\")))'".into()).await;
    let text = out["output"].as_str().unwrap();
    assert!(text.contains("-1 1"), "{text}");
    assert!(!text.contains("'4'"), "{text}");
}
#[tokio::test]
async fn runner_death_is_terminal_for_its_channels() {
    let mut jail = Jail::new(
        json!({"mode":"isolated","preview":{"mode":"off"}}),
        json!({}),
    )
    .await;
    assert_eq!(jail.bash("printf alive".into()).await["output"], "alive");
    unsafe {
        libc::kill(jail.child.pid, libc::SIGKILL);
    }
    assert!(wire::read_control(&mut jail.socket).await.is_err());
}

#[tokio::test]
async fn fresh_tmp_keeps_declared_project_and_spill_mounts() {
    let mut jail = Jail::new(
        json!({"mode":"isolated","preview":{"mode":"off"}}),
        json!({"tmp":true}),
    )
    .await;
    let out = jail
        .bash(
            "printf kept > mounted; test -f /tmp/ocmini-host-marker-does-not-exist; printf DONE"
                .into(),
        )
        .await;
    assert_eq!(out["output"], "DONE");
    assert_eq!(
        std::fs::read_to_string(jail.project.join("mounted")).unwrap(),
        "kept"
    );
}
#[tokio::test]
async fn localhost_preview_completes_handshake_and_tears_down() {
    let mut jail = Jail::new(
        json!({"mode":"firewall","inbound":{"default":"allow"},"outbound":{"default":"deny"}}),
        json!({}),
    )
    .await;
    std::fs::write(jail.project.join("preview-sentinel"), "PREVIEW_WORKS").unwrap();
    let out=jail.request("run",json!({"name":"bash","args":{"command":"python3 -m http.server 4173 --bind 127.0.0.1","background":true}})).await.unwrap();
    let pid = out["metadata"]["pid"].as_u64().unwrap();
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(std::time::Duration::from_millis(500))
        .build()
        .unwrap();
    let mut url = None;
    for _ in 0..40 {
        for index in 2..12 {
            let candidate = format!("http://127.80.0.{index}:4173/preview-sentinel");
            if let Ok(response) = client.get(&candidate).send().await {
                if response.text().await.unwrap_or_default() == "PREVIEW_WORKS" {
                    url = Some(candidate);
                    break;
                }
            }
        }
        if url.is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let url =
        url.expect("localhost-only guest server must be reachable through TUN in both directions");
    let out = jail.bash(format!("kill {pid}")).await;
    assert_eq!(out["metadata"]["exit"], 0);
    for _ in 0..40 {
        if client.get(&url).send().await.is_err() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("closed listener must unpublish");
}

#[tokio::test]
async fn masks_are_empty_and_namespace_creation_is_denied() {
    let mut jail = Jail::new(
        json!({"mode":"isolated","preview":{"mode":"off"}}),
        json!({"maskFixtures":true}),
    )
    .await;
    let out=jail.bash("python3 -c 'import os,ctypes,threading; print(os.listdir(\"secret-dir\")); print(open(\"secret-file\").read()); c=ctypes.CDLL(None,use_errno=True); print(c.unshare(0x10000000),ctypes.get_errno()); t=threading.Thread(target=lambda:print(\"THREAD_OK\"));t.start();t.join()'".into()).await;
    let text = out["output"].as_str().unwrap();
    assert!(text.starts_with("[]\n\n-1 1\nTHREAD_OK"), "{out}");
    assert_eq!(
        std::fs::read_to_string(jail.project.join("secret-file")).unwrap(),
        "SECRET"
    );
    assert_eq!(
        std::fs::read_to_string(jail.project.join("secret-dir/secret")).unwrap(),
        "SECRET"
    );
}
#[tokio::test]
async fn signal_scope_denies_host_signals_but_allows_own_process() {
    let mut jail = Jail::new(
        json!({"mode":"host"}),
        json!({"process_isolation":"signal"}),
    )
    .await;
    let pid = std::process::id();
    let out=jail.bash(format!("python3 -c 'import os;os.kill(os.getpid(),0);\ntry: os.kill({pid},0)\nexcept OSError as e: print(e.errno)' ")).await;
    assert_eq!(out["output"], "1\n", "{out}");
}
#[tokio::test]
async fn unix_socket_deny_is_independent_of_network_mode() {
    let mut jail = Jail::new(json!({"mode":"host"}), json!({"unix_sockets":"deny"})).await;
    let out=jail.bash("python3 -c 'import socket;\ntry: socket.socketpair()\nexcept OSError as e: print(e.errno)'".into()).await;
    assert_eq!(out["output"], "1\n", "{out}");
}
#[tokio::test]
async fn udp_outbound_is_peer_pinned_and_bidirectional() {
    let route = std::net::UdpSocket::bind("0.0.0.0:0").unwrap();
    route.connect("192.0.2.1:9").unwrap();
    let ip = route.local_addr().unwrap().ip();
    let socket = tokio::net::UdpSocket::bind("0.0.0.0:0").await.unwrap();
    let port = socket.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
        let mut bytes = [0; 4];
        let (n, peer) = socket.recv_from(&mut bytes).await.unwrap();
        assert_eq!(&bytes[..n], b"PING");
        socket.send_to(b"PONG", peer).await.unwrap();
    });
    let mut jail=Jail::new(json!({"mode":"firewall","preview":{"mode":"off"},"inbound":{"default":"deny"},"outbound":{"default":"deny","allow":[{"protocol":"udp","destination":ip.to_string(),"port":port}]}}),json!({})).await;
    let out=jail.bash(format!("python3 -c 'import socket;s=socket.socket(socket.AF_INET,socket.SOCK_DGRAM);s.settimeout(3);s.sendto(b\"PING\",(\"{ip}\",{port}));print(s.recv(4).decode())'")).await;
    assert_eq!(out["output"], "PONG\n", "{out}");
    server.await.unwrap();
}
#[tokio::test]
async fn explicit_udp_inbound_allows_only_reverse_flow_replies() {
    let reserve = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let port = reserve.local_addr().unwrap().port();
    drop(reserve);
    let mut jail=Jail::new(json!({"mode":"firewall","preview":{"mode":"off"},"inbound":{"default":"deny","allow":[{"protocol":"udp","source":"127.0.0.1","port":port}]},"outbound":{"default":"deny"}}),json!({})).await;
    jail.request("run",json!({"name":"bash","args":{"command":format!("python3 -c 'import socket;s=socket.socket(socket.AF_INET,socket.SOCK_DGRAM);s.bind((\"0.0.0.0\",{port}));open(\"ready\",\"w\").close();p,a=s.recvfrom(65535);s.sendto(p,a)'"),"background":true}})).await.unwrap();
    for _ in 0..60 {
        if jail.project.join("ready").exists() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(jail.project.join("ready").exists());
    let socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    socket
        .send_to(b"INBOUND", ("127.0.0.1", port))
        .await
        .unwrap();
    let mut bytes = [0; 32];
    let (n, _) = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        socket.recv_from(&mut bytes),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(&bytes[..n], b"INBOUND");
}

#[tokio::test]
async fn icmp_echo_reaches_the_host_and_reassembles_fragmented_payloads() {
    let route = std::net::UdpSocket::bind("0.0.0.0:0").unwrap();
    route.connect("192.0.2.1:9").unwrap();
    let ip = route.local_addr().unwrap().ip();
    let mut jail=Jail::new(json!({"mode":"firewall","preview":{"mode":"off"},"inbound":{"default":"deny"},"outbound":{"default":"deny","allow":[{"protocol":"icmp","destination":ip.to_string()}]}}),json!({})).await;
    for length in [4, 4000] {
        let out=jail.bash(format!(r#"python3 -c 'import socket,struct;s=socket.socket(socket.AF_INET,socket.SOCK_DGRAM,socket.IPPROTO_ICMP);s.settimeout(3);p=struct.pack("!BBHHH",8,0,0,123,7)+b"x"*{length};s.sendto(p,("{ip}",0));r=s.recv(65535);assert r[0]==0;assert r[8:]==b"x"*{length};print(len(r)-8)'"#)).await;
        assert_eq!(out["output"], format!("{length}\n"), "{out}");
    }
}

#[tokio::test]
async fn pid_one_reaps_orphans_even_when_proc_is_hidden() {
    let mut jail = Jail::new(
        json!({"mode":"isolated","preview":{"mode":"off"}}),
        json!({"hideProc":true}),
    )
    .await;
    let out=jail.bash(r#"python3 -c 'import os,time; p=os.fork(); os._exit(0) if p==0 else time.sleep(0.02)'"#.into()).await;
    assert_eq!(out["metadata"]["exit"], 0, "{out}");
    for _ in 0..50 {
        let children = std::fs::read_to_string(format!(
            "/proc/{}/task/{}/children",
            jail.child.pid, jail.child.pid
        ))
        .unwrap();
        if children.trim().is_empty() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("PID 1 must reap unknown children without opening masked proc paths");
}

#[tokio::test]
async fn large_attachments_and_errors_spill_without_truncation_or_channel_failure() {
    let mut jail = Jail::new(
        json!({"mode":"isolated","preview":{"mode":"off"}}),
        json!({}),
    )
    .await;
    let mut image = b"\x89PNG\r\n\x1a\n".to_vec();
    image.resize(300_000, 7);
    std::fs::write(jail.project.join("image.png"), &image).unwrap();
    let value = jail
        .request(
            "run",
            json!({"name":"read","args":{"filePath":"image.png"}}),
        )
        .await
        .unwrap();
    use base64::Engine;
    assert_eq!(
        base64::engine::general_purpose::STANDARD
            .decode(
                value["metadata"]["attachments"][0]["data"]
                    .as_str()
                    .unwrap()
            )
            .unwrap(),
        image
    );
    std::fs::write(jail.project.join("diagnostic.txt"), "x".repeat(300_000)).unwrap();
    let error=jail.request("prepare",json!({"name":"edit","args":{"filePath":"diagnostic.txt","oldString":"absent","newString":"change"}})).await.unwrap_err();
    assert!(error.len() > 300_000);
    assert_eq!(
        jail.bash("printf HEALTHY".into()).await["output"],
        "HEALTHY"
    );
}

#[tokio::test]
async fn tools_cannot_inspect_trusted_runner_descriptors() {
    let mut jail = Jail::new(json!({"mode":"isolated"}), json!({})).await;
    let result = jail.bash("python3 - <<'PY'\nimport os\ntry:\n print(os.readlink('/proc/1/fd/3'))\nexcept PermissionError:\n print('RUNNER_FDS_PROTECTED')\nPY".into()).await;
    assert!(
        result["output"]
            .as_str()
            .unwrap()
            .contains("RUNNER_FDS_PROTECTED"),
        "{result}"
    );
}
