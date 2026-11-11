# ocmini sandbox plan

Sandboxing for ocmini's model tools on Linux. This document is the design.

## Architecture

Two processes:

- **Brain** — the ocmini main process: LLM, TUI, config, storage, hooks. Unjailed, runs no model-requested code.
- **Runner** — a satellite process, `ocmini __runner`, spawned eagerly at session start, one per session **and per subagent** (the brain spawns every runner directly, §Subagents), dies at session end; a runner that dies mid-session is **never respawned** (§Runner transport). Not a daemon.

Every model-requested side effect (shell, file read/write, `webfetch`) runs in the runner. The runner is spawned by self-exec (`current_exe()`), and at `execve` it enters:

- **Landlock** (the `landlock` crate, `CompatLevel::HardRequirement`, ABI v6 floor) — the read/write/exec fence.
- **Raw seccomp BPF** (hand-written, no crate) — syscall deny-list (mount/namespace class, §Jail boot), socket-family and io_uring gates (§UNIX sockets).

A sandbox that cannot be enforced is refused at startup (exit 78), never silently degraded.

**Invariant:** the kernel enforces filesystem, syscall, and **direct-network** isolation — the fence, the mount masks, the seccomp gate, the netns route. The trusted **brain-side network broker** enforces destination policies for **explicitly mediated** connections (rule-checked modes). **Neither the runner nor its children can widen the effective policy:** unmediated **IP** traffic dies at the missing netns route; mediated traffic passes only the broker's rule check; local and non-IP socket families are governed by §UNIX sockets and the uniform family allow-list (§Jail boot). The brain runs no app-side path pre-checks — the kernel is the only judge of fs/syscall; mediation exists only for destinations, only in rule-checked modes, and is invisible to the model (model-blind).

The sandbox lives in a **separate crate** (`ocmini-sandbox`), wired into ocmini behind a **default-enabled** cargo feature (`sandbox`): shipped binaries ship with the jail code in; a `--no-default-features` build removes the sandbox entirely. `off` (the default preset) = feature in, jail off.

## Implementation layout

- **`ocmini-sandbox`** (feature `sandbox`, default-on) — permission sandboxing and network relay, nothing else: the sandbox config schema + validation, the jail builder (Landlock, seccomp BPF, boot choreography, netns/TUN/pidns, mount masks), the fd-3/fd-4 frame codec, the TUN relay loop, and the brain-side broker (smoltcp stack, rule engine, SNI parser, DNS handler, **preview publisher** incl. loopback address leasing, §Automatic previews). **No tool code.**
- **`ocmini-tools`** — the tool implementations (fs, shell, webfetch) as plain functions, no sandbox dependency. The brain calls them directly (preset `off` / feature off); the runner calls them to serve frames.
- **ocmini binary** hosts both sides: the brain's runner manager (registry with **terminal failure states**, broker task) and the `__runner` subcommand. The sandbox crate exposes the runner loop (transport + dispatch); ocmini implements the handlers over `ocmini-tools` — the sandbox crate stays tool-agnostic.
- **The router is one point:** the tool-call entry. Feature on + preset ≠ `off` → frame to the runner; else → direct `ocmini-tools` call. Nothing above it knows.
- `--no-default-features`: the sandbox crate is not compiled, the router has no frame arm, `__runner` does not exist — zero sandbox code.

The application is split into `ocmini-core` (LLM lifecycle and shared domain
logic), `ocmini-tools` (all built-in tools and file transactions), `ocmini-tui`,
and `ocmini-server`, composed by the `ocmini` binary. Root `src` holds only the
binary entry point. The sandbox crate remains independent of tool handlers.

History storage is selected **at compile time**, using `Store<B>` and the
`SelectedBackend` type alias. SQLite is the shipped backend; there is no runtime
backend selector or trait-object dispatch. Backend-neutral fork remapping,
recovery transformations, turn results and compaction policy remain in core,
alongside hook and prompt-admission orchestration. The storage contract exposes
queries and atomic commits: message/part history, admission receipts, ownership,
session forks, recovery and durable hook intents keep their existing transaction
boundaries. SQLite keeps its database format, migrations, WAL reader/writer
connections, paging and recovery predicates. A new backend must supply the same
atomicity and ordering without implementing the shared lifecycle again.

## Runner transport

Two `AF_UNIX` socketpairs connect brain and runner: the **control** socketpair (the runner inherits its end as **fd 3**) and the **packet** socketpair — the TUN relay loop (**fd 4**, rule-checked modes only). The brain keeps the far end of each; the runner inherits its ends across the self-exec (`CLOEXEC` cleared for the exec, re-set right after boot). Nothing is on disk: no path, no symlink/race surface, and no host process can connect to a socketpair endpoint it does not hold — the kernel guarantees the peer is the brain. The split is isolation: control traffic (requests, kills, terminals) never queues behind packet floods, and each socket carries its own backpressure.

- **Wire encoding:** every frame = **u32 big-endian length** (payload bytes) + **1-byte type tag** + payload. **`C`** control frames (JSON: brain→runner `exec`/fs/net requests with monotonic ids; runner→brain result chunks + one terminal frame per request; **256 KiB cap**, larger tool output spills to `tool-output/<session_id>/` and is referenced) ride the **control socketpair**; **`P`** frames (one TUN packet, ≤ MTU 1500, rule-checked modes) ride the **packet socketpair** — JSON and packet bytes share no stream at all. The 5-byte header on KB-sized payloads costs nothing measurable.
- **No handshake:** both sockets exist at exec; the first control frame is the first request.
- **Backpressure is socket-level with bounded queues:** no application-layer flow control. AF_UNIX queue capacity is set by the **sender's** `SO_SNDBUF` — `SO_RCVBUF` is **not supported** on AF_UNIX (socket(7)) — and each endpoint sizes `SO_SNDBUF` to the documented constant (default 256 KiB — sizing, not caps; the kernel doubles the requested value for internal bookkeeping, so it is a sizing-class value, not an exact allocation). The kernel buffer bounds the socket, not userspace, so **writer discipline** bounds the processes: **one serialized writer per socket**; pending output is a **bounded queue** (frame-count bound, a sizing constant) or frames are written directly with no queue; a producer facing a full bound is **paused before it allocates the next frame** — the TUN read / host-socket read simply does not start; partial write + `EAGAIN` → bytes **stay unwritten**, the writer parks on writability, resumes, and **completes the partial frame before the next frame begins**. Downstream: an undrained TCP producer → its flow control / retransmission slows delivery; **UDP overload → drop** excess datagrams, never grow memory; runner death → socket EOF/error terminates the channel and every pending operation. Kernel buffers + bounded queues: **neither process ever buffers without bound** — applied per socket independently, so packet backpressure never stalls the control socketpair.
- **Single-tenant:** the runner serves exactly its session's requests (`current_thread` tokio). The frames are the whole RPC surface — the brain exposes no general privileged callback into itself.
- **Failure detection:** fd-3 (control) EOF = the runner died; the brain marks it **failed** — a terminal state for its session: pending requests terminate with failure, broker state, published preview listeners, and the loopback address lease are torn down, and no further tool execution is accepted through it. **No automatic respawn, no command retry, no replay of potentially non-idempotent operations** — new runners come only from normal creation of a new session or subagent. `PR_SET_PDEATHSIG` gives the runner the mirror signal; a jail change kills the runner (the new runner arrives with a new session) — a live jail is never mutated.

## Jail boot

- **Spawn:** the brain calls `clone()` (in `ocmini-sandbox`, `libc`, no new dependency) with `CLONE_NEWUSER` + `CLONE_NEWNS` (always — the mountns carries the `/tmp`, `/etc/resolv.conf` and `/proc` mounts) + `CLONE_NEWNET` (all net-isolated modes) + `CLONE_NEWPID` (only `process_isolation: "full"`) — so the runner is **born PID 1 of its namespaces**, the brain's direct child. Death chain: brain dies → `PR_SET_PDEATHSIG` → runner dies → (pidns) the kernel wipes the jail tree. The clone plumbing (child stack, `pidfd`, reaping) is sandbox-crate code.
- **fd pinning across the exec:** fd 3 = control socketpair; fd 4 = packet socketpair (rule-checked modes); fd 5 = `/dev/net/tun` (rule-checked modes); fd 6 = **read-only memfd runtime spec** (JSON: frozen effective policy, working directory, scratch/spill paths). 3/4/5/6 are `CLOEXEC`-cleared for the exec and re-set at runner startup: fd 6 is **read once at runner startup and closed** — it survives the exec precisely so the runner can read it, and its fields partition into pre-exec bootstrap and post-exec runtime (§Jail boot step 8); fd 3/4/5 stay open for the runner's lifetime — control channel, packet loop, and TUN device are the runner's **trusted part** — and are `CLOEXEC`, so tool children exec **without them**: no model-executed process ever holds a privileged descriptor.
- **Choreography** (the cloned child, before `execve`; unchecked by seccomp — the filter installs last):
  1. uid/gid self-map (deny `setgroups`, `0 <uid> 1`) — uid 0 inside: CAP_NET_ADMIN in the namespace, no privilege outside;
  2. mounts (mountns): propagation **private** (`MS_REC|MS_PRIVATE` on the tree — masks never propagate to the host, host events cannot open reveal windows); `resolv.conf` bind to the scratch copy; `tmp: true` → fresh tmpfs at `/tmp`; `process_isolation: "full"` → fresh `procfs`; **hide masks** — an empty tmpfs dir bind-mounted over every hidden directory, an empty file over every hidden file, every target verified (§Config schema);
  3. netns: bring `lo` up, `TUNSETIFF` on fd 5, addresses + default route; netns-local sysctls (checked): `net.ipv4.conf.<tun>.route_localnet = 1`, `rp_filter = 2`, `ping_group_range = 0 0` (unprivileged in-jail ping sockets after capability drop) (loose) on the TUN iface (§Guest-localhost delivery — the **host's** settings are never touched); write the scratch `resolv.conf`;
  4. Landlock **after the final mount layout**: `add_rule` per (pre-opened fd, rights) → `landlock_restrict_self`;
  5. capabilities drop: clear effective/permitted/inheritable, **empty the bounding set**, clear ambient — CAP_NET_ADMIN from steps 2–3 is gone;
  6. `prctl(PR_SET_NO_NEW_PRIVS)` — capabilities are unrecoverable across `execve`;
  7. seccomp: `seccomp(SECCOMP_SET_MODE_FILTER, LOAD)` — the mask/namespace class: `mount`, `umount2`, `pivot_root`, `chroot`, `unshare`, `setns`, **plus namespace-flagged `clone`** — cBPF reads `clone`'s flags argument directly and every `CLONE_NEW*` bit lives in its **low 32 bits** (readable on every supported arch, armv6 included), so clone with any namespace bit → `EPERM`; **`clone3` → `ENOSYS`**: its flags ride a struct pointer that cBPF cannot read, so the filter removes the syscall and glibc falls back to inspectable `clone` (the Chromium-sandbox model). Plain `clone` (threads, `fork`, `vfork`) is untouched. The jail can never unmask a hidden path or create a namespace that reveals the underlying filesystem — masks are permanent for the life of the jail; the filter carries the same deny-list in every mode, **plus** the socket-family gate per `unix_sockets` mode (with `io_uring: false`: `io_uring_setup`/`io_uring_enter`/`io_uring_register` → `EPERM`, §UNIX sockets), **plus a uniform socket-family allow-list** for model-executed code — `socket`/`socketpair` succeed only for **AF_UNIX** (per the final mode gate), **AF_INET/AF_INET6**, and **AF_NETLINK** (netns-scoped where one exists); **AF_VSOCK is denied by the socket/socketpair gate in every mode** — its *global* mode crosses netns, so inside a VM host the missing IP route is not protection; AF_PACKET needs CAP_NET_RAW, which step 5 removes → `EPERM`; every remaining family (AF_KEY, AF_XDP, AF_NFC, …) is denied. The domain argument is read directly by cBPF on every supported arch; the list is mode-independent (uniform behavior). **Accepted io_uring gap:** `IORING_OP_SOCKET` bypasses this family gate, including AF_VSOCK, while io_uring remains enabled. `io_uring: false` closes the gap; other modes intentionally preserve io_uring compatibility;
  8. fd whitelist: keep `{0, 1, 2, 3, 4, 5, 6}` — bootstrap-only descriptors (Landlock ruleset fds, fence fds) are closed and reach nothing past `execve`; fd 6 (the **read-only** memfd spec) survives for the runner, `CLOEXEC`: the runner reads it once at startup, closes it immediately, and it is never passed to a tool. Pre-exec bootstrap uses the clone-copied, preallocated boot structure (fence fds, environment, mounts, namespaces, and syscall policy); the read-only memfd carries the frozen runtime policy and paths across exec, with no wire-config;
  9. `execve` — filesystem, capability, namespace, io_uring, and uniform-family restrictions are applied before runner code runs. Trusted runner startup clears dumpability so tool children cannot retrieve its control, TUN or init descriptors through ptrace/pidfd access. Ordinary tool executables regain normal dumpability. For `unix_sockets: deny`, trusted runner startup first initializes Tokio’s CLOEXEC signal-driver socketpair, then applies the final UNIX-constructor gate with seccomp TSYNC **before dispatching any request**. No tool runs during this bootstrap.
- The Landlock ruleset fd and every fence path are **pre-opened in the brain** before the clone — Landlock binds fds, not paths, so nothing resolves paths inside the half-built jail.
- **Every step is checked; any failure = `exit(78)`** — enforce-or-refuse at boot.

## Requirements

- Linux, kernel **≥ 6.12** (Landlock **ABI v6** at the floor), runtime-probed. **Uniform baseline, fixed feature set:** the ABI v6 feature set (fs, TCP, abstract-unix scopes) **plus** ABI v6 signal scoping — no version-dependent behavior, no fallback mechanism requiring a newer kernel (v9 socket rights: not adopted).
- Unprivileged user namespaces, runtime-probed — **required for every sandboxed preset**: boot always creates the user+mount namespace pair (§Jail boot), in every mode; `off` is unaffected.
- `/dev/net/tun` openable, runtime-probed — required for rule-checked modes (the TUN relay); a rule-checked request without TUN = startup refusal, `isolated`/`host` unaffected.
- New crates, the complete ledger: **`landlock`** (ABI probe, `HardRequirement`) and **`smoltcp`** (the userspace TCP/IP stack) — both pure Rust, MSRV-compatible, armv6/armv7 builds. Everything else is hand-rolled on `libc`/std: seccomp BPF, TUN ioctls, the SNI parser, the DNS resolver.

Probes decide, per boot. No distro policy. macOS/Windows: requesting the sandbox is a startup refusal. 32-bit ARM (armv6/armv7) is a hard build target — raw BPF stays hand-written for it.

## Presets

**Presets are files, not built-ins.** The preset folder — `<ocmini config dir>/sandbox/presets/` — holds one JSON file per preset and is the **only** source of presets at runtime; there is no built-in preset library in the binary. **First boot writes the defaults** into the folder (atomically, via the existing storage durability rules): `off` (unjailed), `read-only` (**`isolated`** net, preview `off`), `workspace-write` (**`firewall`** net — outbound **and** inbound default-allow over the broker, **automatic previews**: dev servers and package fetches work out of the box, §Automatic previews), `strict` (**`outbound-only`** net, `process_isolation: "full"`, ships preview `auto-loopback` — the inbound-deny default gates it off, §Automatic previews), `locked-down` (**`isolated`** net, `process_isolation: "full"`, preview `off`). Every shipped sandboxed preset carries `unix_sockets: "abstract"` (§UNIX sockets). Users edit, add, and delete files; a deleted preset is simply gone. The file's stem is the preset's name; `extends` names a file in the folder.

Every sandboxed preset **fence-reads**: `readable` = system base ∪ toolchain roots ∪ project ∪ session scratch + spill. No default preset reads the whole filesystem; `readable: ["/"]` exists only as an explicit custom preset and means **no read fence** (documented). Writes/exec are fenced per preset.

The session spill dir (tool output, `tool-output/<session_id>/`) is readable+writable in every sandboxed preset — the one always-writable data-dir carve-out.

There is **no built-in secret list**. The fence is a positive allow-list: a path is readable iff it sits under a `readable` root — fencing `~/.ssh`, `auth.json` or anything else is `readable`/`hide` **user configuration**, never a special case in the design.

**UNIX sockets: a three-mode policy plus independent io_uring control (`unix_sockets`), uniform at the baseline.** Landlock v6 scopes **abstract** sockets only — `connect()` on a **pathname** AF_UNIX socket is **not** governed by Landlock file rights; it follows ordinary OS permissions (socket inode mode/ownership, uid) and the namespace boundaries. The key picks the isolation level, independent of the IP network mode (§Network):

- **`off`** — no UNIX-socket isolation: normal AF_UNIX. Pathname and abstract sockets are subject to OS permissions and namespaces (net isolation removes host **abstract** names — names live in the netns — while pathname sockets visible under the fence stay connectable as the uid). The fully permissive compatibility option.
- **`abstract`** (default) — applies `LANDLOCK_SCOPE_ABSTRACT_UNIX_SOCKET` (v6): the kernel stamps every abstract socket with its creator's Landlock domain; the jail reaches abstract sockets created **inside its own domain** — all jail-internal abstract IPC works — while host names (X11, D-Bus: stamped outside) are `EACCES` on every supported kernel. **Pathname sockets remain unscoped** (documented): file-socket daemons stay reachable per OS permissions.
- **`deny`** — no AF_UNIX for model-executed code: seccomp denies `socket()`/`socketpair()` with `AF_UNIX` (BPF cannot read `connect()`'s pathname argument — the gate is the constructor, never the path), and the fd whitelist (§Jail boot) ensures no usable AF_UNIX descriptor is inherited by any model-executed process. **Documented gap (Linux 6.12):** seccomp BPF cannot filter the address family of io_uring's `IORING_OP_SOCKET`, so with io_uring enabled a jailed process can create AF_UNIX sockets through a ring — `deny` is a **compatibility/security tradeoff, not an absolute AF_UNIX denial guarantee**. The trusted brain↔runner socketpairs are established **before** the filter and kept by the runner — their descriptors never enter a tool.
- **`io_uring: false`** — independent of `unix_sockets`, seccomp denial of `io_uring_setup`/`io_uring_enter`/`io_uring_register`: closes the io_uring creation path **at the cost of breaking legitimate programs that require io_uring** — the explicit tradeoff for presets that harden AF_UNIX absolutely. Inherited-FD protections unchanged. The gate covers `socket`/`socketpair` on every supported architecture (armv6/armv7 have direct syscalls; the legacy `socketcall` multiplexer exists on i386/x86-32, which are not supported targets — documented).

The key in an `off` preset → startup refusal (no jail). v9 socket rights: **not adopted** (§Requirements — uniform baseline).

## Config schema

The config file selects the preset; the **preset file** carries the policy.

Config file (ocmini's existing config):

```json
"sandbox": { "preset": "my-dev" }
```

`sandbox/presets/my-dev.json` (one preset per file — §Presets):

```json
{
  "extends": "workspace-write",
  "fs": {
    "readable": ["/usr", "/etc", "/home/me/proj"],
    "writable": ["/home/me/proj"],
    "exec":     ["/usr/bin", "/home/me/.cargo/bin"],
    "hide":     ["/home/me/.ssh"]
  },
  "tmp": true,
  "process_isolation": "full",
  "unix_sockets": "abstract",
  "env": { "clear": true, "allow": ["LANG", "LC_*"], "deny": ["LD_PRELOAD"] },
  "network": {
    "mode": "firewall",
    "ech": "allow",
    "preview": { "mode": "auto-loopback" },
    "inbound":  { "default": "deny", "allow": [], "deny": [] },
    "outbound": { "default": "deny",
                  "allow": [ { "protocol": "tcp", "destination": "*.github.com", "port": 443 } ],
                  "deny":  [ { "destination": "10.0.0.0/8" } ] }
  },
  "buffers": { "tcp_recv": 4194304, "tcp_send": 4194304, "udp_ring": 64 }
}
```

**Keys** (all violations = startup refusal, §Config validation):

- **`sandbox.preset`** (config file) — string, default `"off"`; must name a preset **file** in the folder (the file's stem).
- **`extends`** (preset file) — optional parent = a file in the folder; cycles refused by name. Merge: `fs.*` lists **union** with the parent; `env`, `buffers`, `network` — the child's value **overrides** the parent's wholesale. Widening between presets is legal (library definitions compose into the session's **frozen** effective policy — no runtime merge exists, §Subagents).
- **`fs.readable` / `fs.writable` / `fs.exec`** — **absolute paths only; directories cover their subtree; no globs** (Landlock binds fds at boot — a future file matching a glob would silently be ungranted; a promise that can't be kept is a validation error). `"/"` = no fence (documented). Symlinks resolve at pre-open — the target is bound, pinned at boot. `exec` roots must also be `readable` (no implicit widening). Landlock rights.
- **`fs.hide`** — absolute paths; each must sit under a granted root (a hide outside the fence is a no-op — refused). **Mount masking**, not fence subtraction (Landlock v6 cannot subtract a subtree from a broader directory grant): in the jail's mountns, propagation-private (§Jail boot), an **empty tmpfs dir** is bind-mounted over each hidden directory and an **empty file** over each hidden file, **before** Landlock is applied to the final mount layout; every mask is verified at boot — a missing target or failed mount = **startup refusal**. **Requires `process_isolation: "full"`** (fresh `procfs`, §Process isolation). No inherited fd and no procfs handle carries a hidden inode (fd whitelist, §Jail boot); the runner can never unmount a mask or create a namespace that reveals the underlying filesystem — the mount/namespace class, **incl. namespace-flagged `clone` and `clone3`**, is seccomp-denied and all capabilities are dropped (§Jail boot). **Alias paths are configuration, not mechanism:** a hidden file reachable through a hard link or bind path that the user's `readable`/`writable` grants explicitly expose stays readable — alias tracking (inode-wide, hard-link scanning) is out of scope. Model-blind: masked dirs read **empty**, masked files read **empty**.
- **`tmp`** — bool, default `false`; `true` = fresh **tmpfs at `/tmp`** in the jail's mountns (tmpfs default sizing, no artificial cap).
- **`process_isolation`** — `"none"` | `"signal"` | `"full"`, default `"none"` (§Process isolation): `full` = PID namespace + fresh procfs + PID-1 duties + signal scoping (`strict`/`locked-down`, **required by `fs.hide`**); `signal` = signal scoping with host visibility preserved, no pidns; `none` = neither. Migration: `pidns: true` → `full`, `pidns: false` → `none`.
- **`io_uring`** — bool, default `true`; `false` denies `io_uring_setup`/`io_uring_enter`/`io_uring_register` independently of UNIX-socket policy. This closes the family-gate bypass for every `unix_sockets` value.
- **`unix_sockets`** — string, default `"abstract"`: `off` | `abstract` | `deny` (§UNIX sockets); any other value refused; the key in an `off` preset → refusal. Independent of `network.mode`.
- **`env`** — `clear` **required** (bool, no default); `allow`/`deny` are **name globs** (`LC_*`), deny beats allow; `PATH` rebuilt (§Environment).
- **`network.mode`** — one of the five shorthands, **expanded at load** to `{inbound, outbound}`: `host` → allow/allow, rules refused (no netns to enforce); `outbound-only` → out-allow/in-deny; `inbound-only` → in-allow/out-deny; `isolated` → deny/deny; `firewall` → the user's blocks, `default` **required** each direction.
- **`network.ech`** — `"allow"` (default) | `"deny"` — ClientHello ECH handling in rule-checked modes: `deny` rejects ClientHello messages carrying the ECH extension (0xfe0d, GREASE included); enforced only where the broker inspects TLS (TCP/443); **no QUIC/UDP coverage** (§Enforcement).
- **`network.preview`** — `{ "mode": "auto-loopback" | "off" }` — automatic discovery + publication of jailed dev servers (§Automatic previews); **refused in `host` mode** (apps bind the host network directly). Shorthand expansion: net-isolated modes default `auto-loopback` (the inbound policy gates publication); `read-only`/`locked-down` ship `off`. No per-agent / per-project / per-port configuration.
- **`network.inbound` / `network.outbound`** — `{ default, allow[], deny[] }`; rule fields per §Rules (protocol `tcp|udp|icmp|any` default `any`; IP/CIDR/domain; `icmp` address-only; domains tcp/443-only).
- **`buffers`** — §Buffer sizing: positive integers; `tcp_recv`/`tcp_send` are per-connection **window capacities**, ≥ MTU (1500); **no upper bound** (no arbitrary limits).

## Config validation

The sandbox config is **hard-validated** at startup: unknown keys are refused, `env.clear` is required (no default), `extends` cycles are refused by name, every list value (paths, globs, ports) is validated, buffer sizing values (§Buffer sizing) are range-checked, mode/key combinations are checked at expansion (§Network, §Presets): a `unix_sockets` value outside `off|abstract|deny`, an `ech` value outside `allow|deny`, **any policy key in an `off` preset**, **`fs.hide` without `process_isolation: "full"`**, a `preview` key (or any network rule) in `host` mode, `ech` in `host`/`isolated` (no broker to inspect). The preset folder is validated as config: an unparseable file, a non-object preset, a duplicate preset name (stem collision across files), a `preset`/`extends` naming a file that isn't there — all startup refusals. Any violation = startup refusal (exit 78). **Nothing is silently ignored**, files included.

## Network

The real surface is the **per-direction rule set** — `inbound` and `outbound`, each with a `default` and `allow`/`deny` rules (protocol, source/destination as IP/CIDR/domain, port). **Modes are shorthands** for common shapes:

- `host` — inbound + outbound default-allow: **intentionally unisolated networking**. No netns, no TUN relay, no firewall, **no preview machinery, no address allocation, no port remapping** — programs use the host network directly. Two agents binding the same host port is the normal OS conflict (`EADDRINUSE`): the model observes the occupied port and chooses another, exactly as outside a sandbox — accepted, not solved.
- `outbound-only` — outbound default-allow, inbound default-deny.
- `inbound-only` — inbound default-allow, outbound default-deny: the jail serves, never dials.
- `isolated` — both directions default-deny: private netns, loopback only.
- `firewall` — the **rule-set shorthand**: the §Rules decide, the TUN relay enforces. **Rule-checked modes** = `outbound-only`, `inbound-only`, `firewall` — the TUN relay's domain.

`webfetch` is jailed like everything else.

### Rules

Rules are per-direction — protocol (`tcp` | `udp` | `icmp` | `any`), address, port (`icmp` is address-only; a port on an `icmp` rule is a validation error); **domain addresses are valid only on outbound `tcp` rules with port 443** — the one place a name can be checked and pinned (§Enforcement); a domain on any other rule is a validation error; hostnames normalized (case, IDN, trailing dot); `*.suffix` matches subdomains only (`*.github.com` never matches `evilgithub.com`). **Evaluation is deterministic:**

1. matching **deny** rule → **Deny**
2. matching **allow** rule → **Allow**
3. no match → the direction's `default`

**Deny always beats allow** — a narrower allow never overrides a matching deny; express intent in the rule set, no heuristics. Example:

```json
{
  "network": {
    "inbound":  { "default": "deny",
                  "allow": [ { "protocol": "tcp", "source": "127.0.0.1/32", "port": 8080 } ],
                  "deny": [] },
    "outbound": { "default": "deny",
                  "allow": [ { "protocol": "tcp", "destination": "0.0.0.0/0", "port": 443 } ],
                  "deny":  [ { "destination": "10.0.0.0/8" }, { "destination": "169.254.0.0/16" } ] }
  }
}
```

### Enforcement: TUN packet relay

Rule-checked modes run a **transparent packet relay** — no proxy, no listener, no proxy env vars:

- The jail netns has a **TUN device as its only route** (the cloned child opens `/dev/net/tun` + `TUNSETIFF` in its netns, §Jail boot; CAP_NET_ADMIN exists in the self-mapped userns; `/dev/net/tun` is a named machinery rw carve-out). IP traffic that tries to bypass the policy has **no path** — the proxy-bypass class is gone by construction (non-IP families: the §Jail boot family allow-list).
- The runner relays IP packets between the TUN (fd 5) and the brain over the packet socketpair (fd 4, `P` frames, §Wire encoding). The **brain's userspace TCP/IP stack** (pure-Rust) owns the jail's transport: every segment/datagram passes the rule engine before any real socket sends; inbound connections terminate in the jail's stack via the relay.
- **Policy arithmetic:** IP/CIDR + port + direction are **exact** — packets carry the destination. **Domain** rules apply to outbound TLS (TCP/443): the stack parses the **SNI** from the ClientHello, and the rule admits the connection only if **the SNI matches and the destination IP is in the DNS cache of that SNI** (the brain's resolver fills it; an authorized SNI cache miss is resolved before connection establishment, covering NSS/hosts lookups made locally by clients). SNI alone proves nothing — it is client-supplied — so **pinning is the boundary**: connecting to an arbitrary IP wearing `github.com`'s SNI is denied. **Authorization ordering (invariant):** for outbound TCP/443 with domain-based authorization, the broker inspects the ClientHello and completes the SNI/pinning checks — and any `ech` policy — **before opening the real outbound destination socket**; the guest-side connection may be **provisionally** accepted while the decision is made (the §Buffer sizing window absorbs the delay — the configured window accommodates ClientHello data with demand-paged backing), and a flow that cannot satisfy the policy is rejected (RST) with **no bytes forwarded**. Design-level requirement: no general HTTP proxy, no extra protocol machinery, SNI scope not extended to other protocols. With ECH, pinning applies to the **outer** SNI — the name the broker's resolver can see — and the **inner SNI is encrypted**: the outer name does not prove the real destination (the fronting class, documented below); ECH is **not rejected by default** (`ech: "allow"`), and **`ech: "deny"`** makes the parser reject ClientHello messages carrying the ECH extension — **GREASE ECH rejected too**, **TLS-over-TCP only: QUIC/UDP is not covered** (no universal-ECH-disablement claim). Non-TLS traffic carries no name. UDP rules are per-datagram ip/port. **ICMP** is a rule protocol: echo is relayed through the stack like everything else (address rules, no ports), and **PMTUD feedback** ("fragmentation needed" for an admitted connection) is always relayed — transport correctness, not reachability. Inbound IPv6 fragments are dropped (stack limitation, documented).
- **DNS:** the jail's `resolv.conf` names the brain's resolver (DNS packets ride the TUN); resolved IPs are subject to the outbound rules.
- **Documented classes:** an allowed destination is a full tunnel to that destination (IP-level policy); relayed inbound is bidirectional; **domain fronting** — a server at a pinned IP serving an ECH-hidden hostname — is the residual domain-rule bypass (requires the allowed server to cooperate, rare); TLS content is not inspected.
- Landlock net rights are **unhandled** outside `host` (the jail owns its stack; the kernel's role is the missing route). `host` = host stack, no enforcement.
- **Addressing (constants):** loopback `127.0.0.1`/`::1`; the TUN subnet is **`198.18.0.0/15`** — the RFC 2544 benchmarking range: not publicly routable, disjoint from every private range rules address (`10/8`, `172.16/12`, `192.168/16`, `169.254/16` stay clean). Jail `198.18.0.1`, brain/TUN peer `198.18.0.2`; IPv6 ULA `fd00:6f63:6d69::/64` (jail `::1`, peer `::2`). TUN: `IFF_NO_PI` (raw IP frames), MTU 1500, IPv4 + IPv6. Preview leases are **host-side** (`127.80.0.0/16`, §Automatic previews) — never jail addresses.
- **DNS rides the TUN, policy-checked:** the jail's `resolv.conf` names the peer address (`198.18.0.2`) as its nameserver; the relay intercepts DNS requests to peer:53 and forwards them to the brain's resolver. **Before calling `getaddrinfo`, the broker checks the requested hostname against the frozen effective outbound policy** (§Subagents): resolution is permitted when outbound defaults to allow and no matching domain deny rule applies, or when an explicit outbound domain allow rule matches and no domain deny rule matches. Denied queries receive a local refusal **without invoking the host resolver**. Under outbound default-deny, IP/CIDR-only allow rules **do not** authorize arbitrary hostname resolution — the destination IP is unknown before resolution. Authorized hostnames are normalized (§Rules) and resolved as **absolute names**, no search-domain expansion. Resolution runs on a blocking thread using host `getaddrinfo` for NSS-compatible address lookup (systemd-resolved, `nsswitch`, `/etc/hosts`); `getaddrinfo` exposes no DNS TTLs, so the pin cache uses **synthetic 600-second lifetimes** (sizing-class tunable), re-resolved on expiry; established flows remain governed by the flow table (§Flow tracking). **DNS authorization does not authorize connections:** resolved IPs must still pass the normal outbound firewall and SNI/IP pinning checks (§Enforcement). DNS requests to the internal peer are permitted for broker processing **independently of external outbound rules** — this does not authorize external DNS resolution. The resolver provides **A/AAAA address-lookup behavior, not general-purpose, byte-identical DNS forwarding**. Oversized replies fragment (IPv4 reassembly is handled; IPv6-fragment caveat above).
- **NAT is emergent:** outbound packets carry the jail address as source; the brain's real host sockets NAT naturally — no raw sockets, no NAT table. Inbound injection uses the real peer IP as source (§Flow tracking).

### Flow tracking (rule-checked modes)

Rule-checked modes are a **stateful firewall, in userspace in the brain, per jail** (netfilter in the kernel is untouched). Rules gate **new flows only**; the flow table carries established traffic:

- A packet matching **no flow** = new flow: judged by its direction's rules (deny → drop; TCP: RST). Permitted → a flow entry is created.
- A packet **matching an established flow** passes in its direction **without rule re-evaluation** — server replies under `outbound-only`, responses under `inbound-only`.
- **Semantics mirror Linux conntrack** — the host's own model; the numbers below are Linux's shipped defaults (kernel.org conntrack sysctl documentation):
  - **TCP** — states with conntrack's per-state timeouts: `syn_sent` 120s, `syn_recv` 60s, `established` 432000s (5 days), `fin_wait` 120s, `time_wait` 120s, `close` 10s, `close_wait` 60s, `last_ack` 30s, `unacknowledged` 300s, `max_retrans` 300s. RST deletes the entry immediately. Timeouts are sizing constants (sysctl-class, tunable) — never caps.
  - **UDP** — 5-tuple flows, conntrack's model: **30s** unidirectional, promoted to **120s stream** once answered; peer-pinned (only the reverse tuple matches — spoofed replies die).
  - **ICMP** — 30s echo flows; other protocols: 600s (`generic`). **ICMP errors associate with their originating flow** — conntrack's documented behavior (the kernel's `nf_ct_icmpsatelite`); the PMTUD relay (§Enforcement) rides this rule.
- **Inbound listeners:** a TUN cannot accept connections — each inbound rule (`ip`, `port`) makes the **brain open a real listener on the host**; on accept the brain injects a connection into the jail's stack with the **real peer IP** as source (no packet exists; the stack fabricates it, the jailed app `accept()`s normally), bytes spliced host↔jail over the packet socketpair (fd 4). UDP inbound: brain binds the real port, datagrams injected, replies relayed per-flow. The inbound rule is checked **once, at accept**, against the real peer IP. **Automatic previews reuse this exact machinery** — the brain publishes discovered guest listeners as additional host-side listeners with the same accept-time check (§Automatic previews).
- **Per jail:** flow tables are per-runner, judged under the session's **frozen effective policy** (§Subagents).

### Automatic previews (isolated modes)

The main inbound use case is **development servers**. Automatic previews are a **standard feature** of the sandbox — not experimental, not opt-in — enabled by default in the shipped net-isolated presets that permit inbound (§Presets). Users select **one preset for every project and agent**; no per-agent, per-project, per-port configuration, and **no privileged networking API for the model**. Retained machinery: one netns + TUN per isolated runner, the brain-side smoltcp broker, fd-4 packet transport, the stateful firewall and destination policy. No new runner networking RPC, no application-level relay in the runner, no privileged host networking setup, no separate proxy.

- **Config:** `network.preview.mode`: `auto-loopback` = discover + publish; `off` = plain firewall. Meaningless (refused) in `host` mode (§Network).
- **Per-runner host-loopback identity:** the brain **leases a unique host loopback IP per active runner** from the `127.80.0.0/16` band (`127.80.0.2`, `127.80.0.3`, …) — every runner uses the same guest TUN address `198.18.0.1` without conflict. Publishing a guest listener on port N = the brain binds `127.80.0.<runner>:N` → `127.80.0.2:5173 → Agent A:5173`. Leasing is **cross-process-safe** (independent ocmini brain processes must not lease the same identity). A socket reserves an **(address, port) pair, not an address** — a bind on some arbitrary port claims no identity — so the lease is a **marker listener**: the brain binds `127.80.0.N:32765` (the **`LEASE_PORT`** constant: rootless-bindable, below the ephemeral range) and **holds it for the runner's lifetime**; every cooperating ocmini instance runs this same protocol, so contention (`EADDRINUSE` on the marker) → next candidate address. **Crash cleanup is automatic** (the kernel releases the marker socket with the process); released addresses are reused. Preview publication is **verified by its own bind**: an occupied port (`EADDRINUSE`, incl. a host wildcard `0.0.0.0` listener squatting it) → publish on a free port **where practical** and report the **actual preview URL** — a non-cooperating host process can squat an address, never an ocmini lease. Host listeners bind **only** the leased `127/8` addresses — never a public interface.
- **Listener discovery:** the brain reads `/proc/<runner-host-pid>/net/tcp` + `tcp6` — the runner's netns table, a read-only mechanism; runs automatically for presets with preview enabled. Ports are published **as listeners appear**, removed **as they close**; each listener's **bound address is preserved** — it is the broker's destination (§Guest-localhost delivery). Only the brain manages publication.
- **Inbound transport:** a host TCP connection accepted on the preview address → the brain opens a smoltcp TCP connection **into that runner's netns**, traversing the existing packet transport + TUN; payloads spliced host↔jail with socket-level backpressure (§Wire encoding) and **half-close** support. Ordinary TCP dev protocols — HTTP, HTTPS, WebSockets, hot-module reload — work over the raw splice: **no HTTP reverse proxy, no application-protocol inspection**. The guest app sees the virtual broker peer, not the host client's original IP (documented).
- **Authorization (preview ⊥ firewall):** a preview connection is permitted **only when the effective inbound policy allows it**. `auto-loopback` does not override `inbound.default: deny`; explicit inbound deny rules stay authoritative; a port the policy forbids publishes **no usable endpoint**. `workspace-write` (allow/allow) works out of the box; `strict` (outbound-only) ships `auto-loopback` gated off by its inbound default; publication **never** overrides firewall rules.
- **Security model:** the sandbox protects the host and resources outside the jail from model-executed code. **Inbound access from the host into a deliberately published jailed dev server is not a security boundary** — no redundant inbound-restriction mechanisms are added for previews (Not goals). The essential guarantee: enabling previews **never creates an unmediated jail→host route** and never bypasses the broker's outbound restrictions.

### Guest-localhost delivery (`route_localnet`)

Dev servers habitually bind `127.0.0.1` **exclusively**. Linux's existing `route_localnet` delivers broker connections to them — no second forwarding implementation:

- **Bootstrap:** **inside the runner's private netns only**, on the jail's TUN iface: `net.ipv4.conf.<tun>.route_localnet = 1`, `rp_filter = 2`, `ping_group_range = 0 0` (unprivileged in-jail ping sockets after capability drop) (loose — the **reverse path must work**, not just SYN delivery), §Jail boot step 3; boot-checked; the **host's settings are untouched**. The jail's regular loopback stays enabled.
- **Connection path** (guest server at `127.0.0.1:5173`): brain accepts on `127.80.0.<runner>:5173` → smoltcp connects `198.18.0.2 → 127.0.0.1:5173` → the SYN rides fd-4/`P`/TUN into the jail → with `route_localnet` it is delivered to the guest's localhost-bound socket → the guest replies from `127.0.0.1` toward `198.18.0.2` through the TUN → smoltcp **associates the 127.0.0.1-sourced reply with the established preview connection** and completes it.
- Listeners bound to `198.18.0.1`: destination = that address; IPv4 wildcard (`0.0.0.0`) listeners: an appropriate reachable guest address. The outbound firewall is **unchanged** — every such connection passes the normal rule checks.
- **IPv6 limitation (documented):** Linux has **no IPv6 equivalent** of `route_localnet` — a service bound **exclusively to `::1`** is not published through this mechanism, and no second privileged forwarding path is added for it. IPv6 services bound to a reachable guest interface address work through normal IPv6 forwarding.
- A jailed process dialing `127/8` hits **its own netns loopback** (its own services): `route_localnet` grants jailed code **no new outbound route to host-local services** — the jail netns has no route to the host's loopback (verified, §Test plan).

### Buffer sizing

Upstream smoltcp fixes TCP buffer capacity at socket creation and exposes no supported resize operation. smoltcp is **not vendored or patched**. Anonymous demand-paged mappings back the configured windows: virtual capacity is reserved at creation, physical pages become resident only when traffic touches them. This preserves the full configured advertised window and throughput on every machine without hardware detection. It replaces the earlier proposal for an 8 KiB base and runtime buffer resizing; there is no claim that smoltcp auto-tunes its logical window.

```json
"buffers": { "tcp_recv": 4194304, "tcp_send": 4194304, "udp_ring": 64 }
```

- `tcp_recv` / `tcp_send`: per-connection **window capacities** (defaults **4 MiB** each). Anonymous backing is demand-paged; idle connections do not eagerly touch the full allocation. Full windows sustain approximately 200 MB/s at 20 ms RTT. Configured capacities reserve virtual address space per connection, including on 32-bit targets; resident pages track data touched.
- `udp_ring`: buffered datagrams per UDP socket path (default 64).
- Hard-validated: positive integers; `tcp_recv`/`tcp_send` ≥ MTU (1500); defaults documented; inherited via `extends`. The relay's iface packet pool (MTU-sized, burst-absorbing) stays an implementation constant.
- **Sizing, not caps:** configured windows are backed on demand; no connection-count or throughput cap is added. TCP’s protocol window-scaling maximum of 2^30 bytes applies to the advertised receive window.

## Process isolation (`process_isolation`)

Three-mode key — `none` | `signal` | `full` (`full` in the default `strict` and `locked-down`, **required by `fs.hide`**). Every mode shares the fence/mount/net machinery; the key controls process **visibility** and **signaling**:

- **`none`** — no additional isolation beyond the shared mechanisms: the jail sees the host process table and signals per OS permissions (host-visible, documented).
- **`signal`** — **no PID namespace**: host process visibility is **intentionally preserved** (documented, not a gap); `LANDLOCK_SCOPE_SIGNAL` (ABI v6, our 6.12 floor) confines signaling to the jail's Landlock domain — jail processes signal **only each other**; a signal to a host process outside the domain → `EPERM`. No PID-1 duties.
- **`full`** — `signal` plus the PID namespace: the runner is **born** in its PID namespace via `clone()` (§Jail boot) and mounts a **fresh `procfs`** at startup — PID 1 from birth; `unshare` never enters a PID namespace (it does not move the caller). §PID-1 duties covers what init must do.

With `full`:

- The jail sees **only its own process tree**: no host `ps`, no `/proc/<pid>` of anything outside. The host still sees the jail (one-way).
- The runner is PID 1 of the namespace: killing the runner's host PID **kills the entire jail tree** — no jailed process outlives its runner.
- The fresh `/proc` mount makes the jail's `/proc/net` its own (loopback in netns, the jail's view otherwise) and is part of the hide-mask construction (§Config schema).

All modes run under the user+mount namespace pair (§Requirements).

## PID-1 duties (`process_isolation: "full"`)

The runner is init of its namespace, and init has jobs:

- **Reaping:** background jobs and orphaned grandchildren reparent to PID 1 — un-reaped zombies fill the pidns and the jail's `ps` starts lying (model-blind breakage). The runner runs a `SIGCHLD` → `waitpid(WNOHANG)` drain (tokio `Signal`): tracked jobs propagate exit codes to the brain (existing bg-job machinery), untracked children are reaped silently. The trusted init pins its fresh procfs children table before hide masks (fd 0, made CLOEXEC after exec); tools receive null stdin. Reaping therefore works even when the configured read fence excludes `/proc` or masks hide it.
- **Signals:** the brain's exec-kill requests relay as `kill(pid, …)` from the runner — identical to the unjailed tool. SIGTERM to the runner = graceful (stop accepting, drain, exit); `PR_SET_PDEATHSIG` (brain death) = SIGKILL.
- **Session end:** the brain closes fd 3 → runner exits → **pid 1 exits → the kernel wipes the tree.** Background jobs pinned to their launch jail die with the session by kernel guarantee, not app logic.
- **`none`/`signal`:** no PID-1 duties — plain processes; children reparent to host init at runner death (host-visible, documented).

## Model-blind jail

The model never learns the jail exists. Denials reach it as raw OS errno, byte-identical to the unjailed format. Permission prompts run **unchanged** under the sandbox; approvals (including `--auto`) never widen the jail — approval means permission to attempt, the kernel decides.

Future extension (user-side only, model-blind unchanged): the brain can map its own issued requests to the list that lacks the grant (positive-list lookup on the effective spec) as a denial view; kernel-side denial logging is not offered.

## Environment

Environment policy is config, per preset: `{ "env": { "clear": true, "allow": [...], "deny": [...] } }` — **clearing is optional**, not forced; `allow`/`deny` glob lists shape the inherited environment; **deny beats allow**. `clear: true` starts from an empty env and `allow` re-adds; `clear: false` inherits and `deny` removes. `PATH` is **rebuilt** from the declared exec/toolchain roots, never inherited. `clear` is a **required key** — no default: every preset (default and custom) states it explicitly, missing = config validation failure at startup. The default presets ship `clear: true` with a base allow-list (terminal basics, locale).

## Subagents

**The policy is frozen at session start.** The brain resolves the preset (extends merge + validation, §Config schema) into the **effective policy** and freezes it. Every runner — the session's and every subagent's — receives the **identical frozen copy**: no LLM-selected sandbox policies, no subagent policy overrides, no runtime policy intersection, no parent/child grant merging (no meet arithmetic anywhere — not in the brain, not in the runner, not in the broker).

**Spawner:** the **brain spawns every runner directly** (§Jail boot): each runner creates its **own** namespaces and installs its **own** Landlock/seccomp restrictions from the frozen copy, and speaks on its **own fd-3/fd-4 pair**. No parent-runner spawning, no privileged persistent supervisor, no relayed channels — every runner's chain to the brain is direct.

**Logical tree in the brain:** the runner registry keeps the parent-child relationships (a subagent launch registers under the requesting session). A logical parent terminates → **the brain terminates its registered descendants**. Each full-mode runner stays **PID 1 of its own namespace**, reaping its children (§PID-1 duties); brain death terminates every runner via `PR_SET_PDEATHSIG`. Background jobs stay pinned to their launch jail (die with it, §PID-1 duties).

## Not goals (for now)

Resource limits, quotas, caps, throttling, fleet/managed policy, distro support matrices, phasing, TLS inspection, multi-agent host-port-conflict solving in `host` mode (§Network), inbound protection of published dev servers from host clients (§Automatic previews), UNIX-socket alias/inode tracking (§Config schema).

## Test plan

Feature-off build: `--no-default-features` compiles with zero sandbox code and runs the sandbox-off suite. Jail suite on standard CI runners — no special infra, no skip machinery: on a kernel below the §Requirements floor the suite fails, and that is the signal. Contract suite for the wire protocol (fd-3/fd-4 socketpairs, §Wire encoding). Sandbox-off jobs for macOS/Windows. Runner-failure suite: a crashed runner's pending requests terminate with failure, broker/preview/lease state is torn down, and the session gets **no** respawn and **no** command replay (a new session brings a new runner). Buffer backing suite: idle connections do not eagerly touch their full mapping; a sustained bulk flow retains the configured window and data integrity.

Config validation suite: unknown-key refusal, required `clear`, rule evaluation order (deny → allow → default), `extends` cycle naming, the `unix_sockets` refusals (bad value, `off` preset), the `ech` refusals (bad value, `host`/`isolated`), the `preview` refusal in `host` mode, the `fs.hide`-without-`full` refusal, the hide-outside-fence refusal, the exit-78 strings for every startup refusal.

**Preview integration (implementation acceptance):** a real TCP server bound **exclusively** to guest `127.0.0.1` is published on the runner's leased host address — full TCP handshake, request, response, listener teardown, routing in **both directions** verified end to end; the listener publishes on open and unbinds on close; `preview: off` / inbound-deny policy → no endpoint; and **with `route_localnet` on, jailed code has no outbound route to host-local services**; two brain processes racing to lease never land on the same address (marker-port contention), and a crashed brain's lease is immediately re-usable. Mask suite: hidden dirs read empty, hidden files read empty, no fd and no procfs handle exposes a hidden inode, and the runner's `mount`/`umount2`/`unshare`/`setns` attempts are denied, `clone` with any `CLONE_NEW*` bit → `EPERM`, `clone3` → `ENOSYS` with the glibc fallback working (thread/fork creation unaffected). UNIX-socket suite: `deny` — `socket()`/`socketpair()` with `AF_UNIX` from a tool → EPERM, brain↔runner transport unaffected, `io_uring_setup` succeeds (the documented io_uring gap is present); `io_uring: false` — `io_uring_setup`/`io_uring_enter`/`io_uring_register` → EPERM, no AF_UNIX via `IORING_OP_SOCKET`; `abstract` — host-abstract connect from the jail → `EACCES`, jail-internal abstract pair works; `off` — both reachable. Socket-family suite: `socket(AF_VSOCK)` → `EPERM` in every mode; AF_PACKET → `EPERM` (no CAP_NET_RAW); AF_NETLINK works, netns-scoped in net-isolated modes (`getifaddrs` lists only the jail's interfaces); AF_INET/AF_INET6 usable. Signal suite (`signal`/`full`): a signal to a host process outside the domain → `EPERM`; in-jail signaling (own tree, session end) works. ECH suite (rule-checked modes): ClientHello with ECH (0xfe0d) under `ech: deny` → rejected **before the outbound socket opens**; GREASE ECH rejected; `ech: allow` with outer-SNI pinning works; authorization ordering asserted — with the broker's decision stalled, no bytes reach the destination socket.

**DNS policy tests:** with outbound default-deny + an explicit domain allow rule, matching domains resolve and unrelated domains are refused **before** `getaddrinfo` is invoked; domain deny rules override matching allows; under default-allow, resolution succeeds unless a matching domain deny applies; under default-deny, IP/CIDR-only allow rules do not implicitly authorize hostname resolution; internal-broker DNS stays reachable for policy evaluation; resolved addresses remain subject to outbound IP/CIDR restrictions and SNI pinning; restricted resolution introduces no search-domain suffix lookups.

**Toolchain smoke is config-relative:** a toolchain command **whose toolchain roots are in the preset's fence** must build; a command whose toolchain is not declared must fail with fence denials — a fenced-out toolchain failing is the fence working, tested as such. Which commands pass in which preset is a function of the config, not a fixed pass-list.
