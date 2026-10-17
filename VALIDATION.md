# Patch scope and validation record

Prepared on 2026-10-17 against the three supplied source archives. `SOURCE-PROVENANCE.json` records their exact hashes; no current upstream branch or live T3 installation was assumed to match these snapshots.

## What this patch changes

The patch replaces the incomplete OpenCode-compatible shim with a durable headless execution loop and the OpenCode 1.x HTTP/SSE surface consumed by the supplied T3 integration. Its protocol target is documented with source paths in `SPEC-CONTRACT.md`.

Major changes include durable and idempotent prompt admission, steering without concurrent loops, real history forks, compaction that supplies subsequent model context, interruption recovery, exact event/part ordering, full tool permissions/questions, real task children, usable coding tools, stdio/Streamable HTTP MCP and native provider streaming through Chat Completions, Responses and Anthropic Messages.

OpenCode's UI applications, hosting/account services, package/plugin installation, updater, LSP fleet and 2.x service protocol are excluded. The terminal UI and old shape fuzzer have been deleted. `chrono` and `crossterm` are removed. Libraries added for subprocess cleanup, ignore-aware search, Markdown frontmatter and HTML conversion implement runtime behavior described in the README.

## Executed checks

| Check | Result | What it establishes |
| --- | --- | --- |
| Source contract audit | Completed | Compared T3's 1.x adapter, runtime, CLI parsers, usage reader and text generation call sites with the implemented routes and event fields |
| Independent source review | Completed | Reviewed provider/engine, tool/MCP, server/store interfaces and likely ownership, async lock and cancellation issues; this is not compiler evidence |
| SQL extraction and preparation | Passed | All 39 unique SQL statements extracted from `src/store.rs` prepare in SQLite |
| SQLite behavior checks | Passed | 11 checks covering stable upsert order, unfinished response selection, inbox claims, recovery, orphan cleanup, guarded descendant deletion and unrelated-session preservation |
| Incomplete implementation macro scan | Passed | No `todo!` or `unimplemented!` macros in runtime source; unsupported protocols return explicit errors |
| Patch whitespace check | Passed during packaging | `git diff --check` accepts the final source changes |
| Patch application | Passed during packaging | `git apply --check` and `git apply` succeed against a fresh extraction of the supplied original ocmini archive |
| Applied tree comparison | Passed during packaging | Applying the patch reproduces the supplied updated source tree byte for byte |

The SQL check is reproducible with:

```sh
python3 scripts/verify_store_sql.py
```

Its captured output is in `validation/store-sql-results.json`. It runs the actual extracted SQL, but does not execute Rust transaction boundaries, serializers, the engine or HTTP handlers. SQL success must not be presented as end-to-end runtime success.

## Authored Rust tests: not executed here

The tree contains 75 Rust tests: 15 process/HTTP integration scenarios and 60 unit/module tests. They use loopback scripted peers and temporary state rather than paid-provider credentials.

The integration scenarios cover process and CLI compatibility, Basic auth, scope isolation, client IDs and admission retries, streamed-part ordering, attachments/usage, active-turn steering, permission and question continuation, actual task child execution, stalled-stream abort, fork integrity and restart, commands, default agents/models, compaction reuse, failed MCP connection/reconnection, scoped SSE, durable no-reply prompts and truncated provider failures. Individual scenarios can cover more than one behavior.

Module tests cover configuration/JSONC and metadata, auth parsing, provider protocol selection and variants, SSE framing, usage accounting, signed/encrypted reasoning replay, MCP lifecycle and tool calls, patch/file tools, process cleanup, skill parsing, web conversion, permissions and persistence invariants.

Required build gate:

```sh
cargo test --all-targets
cargo build --release
```

`.github/workflows/ci.yml` runs these commands and the SQL checker on Linux. The workflow is part of the patch; no successful CI run is claimed.

## Checks that could not run

The construction environment has no `cargo`, `rustc`, `rustfmt` or Rust toolchain cache. Attempts to obtain a toolchain through the allowed network path were denied by the environment's proxy. Consequently:

- The Rust source has not been compiled or linked here.
- None of the 75 Rust tests has been executed here.
- No Rust formatting or Clippy result is available.
- No built binary, generated `Cargo.lock`, benchmark or live-provider transcript is included.
- No native T3 SDK session or full T3 application workflow has run against this implementation.

Static review can miss syntax/type errors, incorrect crate APIs, timing bugs and semantic integration errors. A passing Rust build and test suite is still necessary. Live-provider and T3 application validation is also necessary for a deployment claim.

## Deployment boundaries

The implementation targets the supplied T3 OpenCode 1.x integration. It does not promise compatibility with an unspecified future T3 release or every provider SDK in OpenCode. Unsupported vendor-native protocols, browser authorization, legacy MCP HTTP+SSE, audio on transports that do not accept it and unsupported credentials fail explicitly. Manual compaction is implemented; automatic token-budget compaction is not.

The original OpenCode config and credentials remain read-only. ocmini owns its transcript store, refreshed-token cache and usage mirror. Existing ocmini state receives additive schema changes and startup recovery; preserve a database copy when upgrading an existing deployment. Reverting the T3 binary setting selects the original backend again, but does not migrate ocmini conversations into OpenCode.
