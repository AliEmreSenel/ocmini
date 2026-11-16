# Sandbox readiness review

Reviewed 2026-11-16 against the sandbox worktree based on `75f7dc8`, including
its local path-variable and CLI-override changes.

The review covered preset inheritance and validation, process-local CLI
precedence, tool routing, namespace/Landlock/seccomp setup, runner cancellation
and failure, network mediation, storage paths, and hook output handling.

## Findings fixed

- **P1 — Scoped sandbox requirements in a sandbox-free binary.** A server started
  with the sandbox off accepted a later workspace requiring an active preset.
  Startup and directory scoping now check support and reject malformed settings.
  The regression reproduced a successful session creation before the fix and
  verifies refusal afterward.
- **P1 — Writes through stale scratch links.** Reusing `resolv.conf` or
  `mask-file` could follow a link planted by a previous runner. Host writes now
  use a pinned directory, refuse symlinks, and check for a regular file with one
  link before truncating. Regression coverage verifies that linked targets stay
  unchanged and ordinary files remain reusable.
- **P1 — Session IDs used as storage paths.** Caller-supplied v2 IDs could contain
  path traversal. IDs must now be a single path component before scratch or
  spill storage is created. Message IDs and their compatibility rules are
  unaffected.
- **P2 — Ignored CLI overrides.** Overrides preceding inspection commands could
  be accepted without applying them. Such combinations now fail argument
  validation; overrides apply to the default TUI, `run`, `tui`, and `serve`.
- **P2 — Hook output spills outside the jail's read fence.** Large rewritten
  output was saved in the global output directory. Tool results and hook
  replacements now use the session's granted spill directory. The existing
  output transformation test also covers a sandboxed session.
- **P2 — Empty PATH after a formatting error.** An executable root containing a
  path-list separator could erase the complete rebuilt PATH. These settings now
  produce an explicit startup error.
- **P1 — Private `/tmp` widened host write grants.** The writable temporary
  filesystem also granted writes into read-only host trees restored beneath it.
  Restored trees now use recursive read-only mount attributes unless covered by
  a write grant; explicitly writable descendants get separate restored mounts.
  Kernel coverage checks overwrite/create denial, nested directory/file grants,
  and ordinary private temporary files.
- **P1 — CLI resume selected the launch workspace's policy.** Resume could freeze
  an `off` policy before entering a saved workspace requiring `read-only`.
  Opening a saved session stays read-only; activation initializes its sandbox
  after entering the saved scope. A subprocess regression reproduced the write
  bypass and now verifies denial.
- **P1 — Saved-input wake bypassed runner initialization.** Waking queued work
  after restart without a new admission could route tools to the host. Each
  executing turn now initializes its sandbox, including recovery and explicit
  wake paths. A deferred-input/restart/revert-clear regression verifies denial.
- **P1 — Restored children split a session tree's policy.** Starting a saved
  child before its parent allowed each to load different preset contents.
  Startup walks saved ancestry and freezes the root policy for the tree without
  spawning unused parent runners. A restart regression edits the preset between
  child and parent execution and verifies that both retain the original fence.
- **P2 — V2 moves left relative effects in the original directory.** Tool
  transport now carries the actual session directory for each request. The
  existing runner checks destination readability before the saved location
  changes, preserving frozen grants. Coverage checks a successful in-fence
  move and failure outside the accessible filesystem.
- **P2 — Missing environment-expanded hide targets were dropped.** Missing
  read/write/exec search entries may still be skipped; missing hide targets now
  refuse startup. Both expansion and CLI exit-78 coverage exercise this case.
- **P2 — `off.json` accepted an explicit `off:false`.** Explicit false flags now
  fail validation consistently for named and custom off presets.
- **P2 — V2 interrupt left runner background jobs alive.** Interrupt now sends
  the same runner cleanup used by abort while retaining queued input. A live
  background-process regression reproduced the leak and verifies process exit.

## Saved settings and revised presets

The follow-up adds `read-all` (whole-filesystem reads, workspace writes, isolated
network), `read-all-net` (same filesystem grants, broker-mediated inbound and
outbound networking), and `locked-read` (restricted reads, no workspace writes,
isolated network, full PID isolation). `strict` is no longer shipped. The other
four presets retain their behavior.

Session JSON now stores `ocminiSandbox` with the preset name and effective
unexpanded settings. Resume/recovery prefer it over configuration defaults;
additional CLI settings update it, while choosing a different preset replaces
it. Choosing the same preset preserves additions. Runtime policies stay frozen,
children inherit their root, and forks copy the root's latest saved settings.
Snapshots survive preset edits and public metadata replacement; metadata cannot
forge them. Failed persistence shuts down the new runner rather than leaving an
unregistered process. Active saved settings refuse execution in feature-off
builds unless explicitly disabled with `--sandbox-preset off`.

Coverage includes eight consecutive CLI launches with changing defaults,
additions, same/different preset selection and saved `off`; server restart after
a preset edit or removal; same-preset additions that require earlier settings;
child forks after a root selection change; actual read/write
enforcement for all three new presets; and feature-off saved-policy refusal.

The local preset directory was updated explicitly. Its previous contents are
backed up at
`/home/aliemre/.config/opencode/sandbox/presets.backup-20261116T094905328652Z`.
Existing unrelated preset settings were preserved.

The final readiness audit reproduced a stale-policy case: a running server
retained its writable jail after a separate CLI launch saved a read-only policy.
Cached startup now checks the root's saved snapshot and refuses execution with
an explicit restart message. The regression covers the root, existing children,
and new child creation before any stale tool executes, and verifies read-only
enforcement after restarting the server. Applying
an externally changed policy to a retained jail requires a server restart.

## Validation

Passed on x86_64 Linux, kernel `7.2.14-arch1-1`:

- `cargo +stable fmt --all -- --check`
- `cargo +stable clippy --workspace --all-targets --all-features --locked -- -D warnings`
- `cargo +stable test --workspace --all-targets --locked`: **623 passed**,
  including 16 real kernel-enforcement tests.
- `cargo +stable test --locked --test contract official_v2_client_and_schema_conformance -- --ignored`:
  **passed** with the pinned OpenCode 2.2.5 and older T3 clients.
- `cargo +stable test --workspace --exclude ocmini-sandbox --no-default-features --all-targets --locked`:
  **462 passed**.
- `cargo +1.85.0 check --workspace --all-targets --all-features --locked`: **passed**.
- `cargo check -p ocmini-sandbox --target arm-unknown-linux-gnueabihf --locked`:
  **passed**, using the installed nightly ARM toolchain.

- `cargo +stable build --release --locked`: **passed**.
- The optimized x86_64 Linux binary completed an isolated provider/tool turn:
  CLI overrides selected the policy and network mode, a jailed shell wrote to
  its explicitly granted folder, and reading a file outside the read fence was
  denied. The fixture and mock provider were removed afterward.
- The rebuilt release also passed the follow-up smoke for read-only host trees
  beneath private `/tmp`, ordinary temporary writes, generic environment lists,
  CLI overrides, and outside-read denial.
- The saved-settings release smoke passed five consecutive launches using
  `locked-read` and `read-all-net`, including resume after removal of the preset
  file, a missing configured default, additive grants, and preset replacement.
- The final rebuilt release also passed an actual server/CLI smoke: after an
  external read-only selection, the retained writable jail was refused; server
  restart loaded the saved policy and a shell write was denied.

No blocking findings remain in the reviewed scope for the documented Linux
requirements. The release artifact is built locally; the changes remain in the
sandbox worktree.

The generic environment-list follow-up also passed strict lint, Rust 1.85 and
ARM compilation checks, and the rebuilt native release smoke. A custom variable
containing multiple tool directories was expanded into read/exec grants; both
tools executed after the variable itself was filtered out of the runner's
environment. Tests cover duplicate entries, literal path characters, invalid
list entries, and refusal of embedded multi-path variables.

The defaults now store `${env:PATH}` instead of discovered toolchain or
executable roots. Preset generation performs no host discovery. The minimal
Linux runtime prerequisites remain explicit; `read-only` has no broad `/proc`,
`/sys`, or `/dev` read grants. Environment lists preserve priority order and
ignore missing read/write/exec entries, with no toolchain guesses; missing hide
targets refuse startup. The string form
`"sandbox": "preset-name"` and the legacy object form use the same parser in
both feature configurations. Kernel execution, directory denial, priority,
string-selection and release smoke coverage all passed. The existing local
preset files were backed up and their read/exec grants updated explicitly.

The unchanged 121-second hook timeout smoke test remains ignored by the normal
suite; no runtime hook timeout policy changed. ARM execution and macOS/Windows
execution were not performed in this local review. The ARM check covers the
sandbox crate, not a fully linked ARM binary.

The documented Linux requirements and accepted networking limitations in the
[README](../README.md#sandboxed-environments) still apply. No new dependencies,
runtime resource caps, worker limits, hardware detection, or benchmark phases
were introduced.
