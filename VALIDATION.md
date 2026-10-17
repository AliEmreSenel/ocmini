# Validation

The runtime targets the OpenCode 1.x contract consumed by the supplied T3 source snapshot. [SPEC-CONTRACT.md](SPEC-CONTRACT.md) documents that contract; `SOURCE-PROVENANCE.json` retains the source archive hashes.

## Reproduce the checks

Use Rust 1.85.0 or newer and a C compiler/linker for bundled SQLite:

```sh
cargo fmt --all -- --check
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo test --all-targets --locked
cargo build --release --locked
```

The Linux CI workflow runs tests, Clippy and release builds on both Rust 1.85.0 and current stable, with `RUSTFLAGS="-D warnings"` and Clippy's `-D warnings` enforcing warning-free code. Formatting runs on stable. Dependencies are pinned in `Cargo.lock`; `rust-version = "1.85"` and resolver version 3 keep dependency resolution aware of the minimum supported compiler. JSONC uses `serde_json_lenient`, and YAML frontmatter uses the YAML organization's `yaml_serde`, both compatible with the minimum compiler. No Python tooling is required.

To reproduce the minimum-version checks:

```sh
rustup toolchain install 1.85.0 --profile minimal --component clippy
RUSTFLAGS="-D warnings" cargo +1.85.0 test --all-targets --locked
RUSTFLAGS="-D warnings" cargo +1.85.0 clippy --all-targets --all-features --locked -- -D warnings
RUSTFLAGS="-D warnings" cargo +1.85.0 build --release --locked
```

## Executed checks (2026-10-17)

On Linux with Rust 1.85.0 and stable Rust 1.99.1, all 88 tests passed on each compiler: 68 unit tests and 20 process/HTTP integration tests. Both runs used `RUSTFLAGS="-D warnings"`. Clippy passed on both compilers with `--all-targets --all-features --locked -- -D warnings`, with no warning suppressions added. Formatting passed on stable.

This uses the actual minimum compiler, rather than relying only on crate metadata: `ignore` 0.4.30 uses newer let-chain syntax without declaring a higher minimum compiler, so the package explicitly selects 0.4.29.

The preceding cleanup also passed interactive pseudo-terminal checks for Unicode, multiline paste, arrow keys, Ctrl+C abort, Ctrl+D exit and terminal restoration after both exit and SIGTERM.

The optimized release build passed on Rust 1.85.0 with `RUSTFLAGS="-D warnings"`, with no compiler warnings. The resulting executable passed version/help smoke checks. CI runs the same warning-free release build on Rust 1.85.0 and current stable.

## Coverage

Unit tests exercise configuration/JSONC, ordered permissions, YAML frontmatter, credentials, provider protocols and usage, SSE framing, MCP lifecycle, coding tools, process cleanup and persistence. Atomic-write tests verify symlink handling, executable permissions, private credential-cache permissions and temporary-file cleanup. Glob tests include more than 32 brace alternatives to detect silent search omissions.

Persistence tests execute the actual Rust store with bundled SQLite. They cover stable message/part upsert ordering, idempotent admission, prompt claims, recovery of interrupted assistant/tool work, retention of queued prompts and completed responses, live-instance leases, legacy orphan cleanup, guarded recursive deletion and unrelated-session preservation. The former Python script extracted SQL from Rust source using regexes; it and its captured result have been removed in favor of these runtime tests.

Process/HTTP integration tests use isolated temporary state and loopback provider/MCP fixtures. They cover CLI help/version/error behavior, startup and Basic authentication, directory scope, model/agent inventory, prompt admission and retries, streaming order, attachments/usage, steering, approvals/questions, child tasks, cancellation, forks/restart, commands, compaction and provider/MCP failures. They do not require paid-provider credentials.

Terminal smoke checks use an interactive pseudo-terminal to verify Unicode input, bracketed multiline paste, ignored arrow sequences, Ctrl+C abort, Ctrl+D exit and restoration of terminal attributes. Terminal input uses `crossterm`; shutdown signals use Tokio so the terminal guard restores state on normal signal-driven exit.

## Boundaries

Local fixture tests do not establish successful live-provider calls or full T3 application workflows. No startup, memory, binary-size or throughput benchmarks are claimed. Native Google/Gemini, Vertex, Bedrock, Azure authentication, arbitrary npm provider SDKs, MCP browser OAuth and legacy MCP HTTP+SSE are outside the implemented protocol surface.

The original OpenCode config and credentials remain read-only. ocmini owns its transcript store, refreshed-token cache and usage mirror. Preserve a database copy when upgrading an existing deployment. Reverting T3's binary setting selects the original backend; it does not migrate ocmini conversations into OpenCode.
