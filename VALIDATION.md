# Validation

Requires Rust 1.85+ and a C compiler for bundled SQLite. Dependencies are pinned in `Cargo.lock`.

## Checks

```sh
cargo fmt --all -- --check
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo test --all-targets --locked
cargo build --release --locked
```

CI runs tests, Clippy and release builds on Rust 1.85.0 and stable with warnings treated as errors. Formatting runs on stable. To check the minimum compiler locally, install it with `rustup toolchain install 1.85.0 --profile minimal --component clippy` and use `cargo +1.85.0`.

Checked on Linux on 2026-10-17: all 100 tests and Clippy passed on Rust 1.85.0 and stable. The four process tests also passed 25 consecutive runs. Pseudo-terminal checks verified history and context reuse, saved model/agent/directory, model overrides, Unicode, terminal restoration and immediate resume after exiting during a stalled request.

## Test layout

- Unit tests stay inline in their implementation modules. MCP retains its existing `src/mcp/tests.rs` module.
- `tests/contract/`: CLI, sessions, streaming, permissions, commands, compaction, provider failures and MCP, with shared local fixtures in `support.rs`.

Tests use temporary state, bundled SQLite and local HTTP/MCP peers. They cover prompt admission, streaming order, attachments, cancellation, steering, approvals, child tasks, forks, restart recovery, session resume and compaction. Process tests allow bounded time for descendant termination after SIGKILL.

## Manual checks

Use a terminal to check Unicode input, multiline paste, Ctrl+C abort, Ctrl+D exit and terminal restoration. Send a prompt, exit, then resume with `--continue` or `--session ID` and verify the next response uses the earlier conversation. Exercise the session selector with `--resume` and `Ctrl+R`: search by title/ID/directory, Tab through sort orders, arrow/page navigation, Enter resume, Esc cancel.

Live-provider calls and full client-application workflows require validation in the target environment. Local tests do not establish performance or compatibility with unsupported provider protocols. See [SPEC-CONTRACT.md](SPEC-CONTRACT.md) for the API surface.
