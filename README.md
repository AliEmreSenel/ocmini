# ocmini

A minimal, fast, drop-in replacement for `opencode serve` (v1 API) built for T3 Code.

`opencode serve` needs ~9s to start, ~265 MB RSS idle, and grows a 28 GB SQLite store on heavy use.
`ocmini` implements the exact API surface T3 Code consumes (21 SDK methods + 4 CLI probes):

| metric | ocmini | opencode 1.19.2 |
|---|---|---|
| startup to health | ~0.75 s | ~9.3 s |
| idle RSS | ~5 MB | ~265 MB |
| binary size | 8 MB | 186 MB |
| session store growth | SQLite WAL, 30-day prune | unbounded |

## Compatibility (contract-verified)

- Spawn: `ocmini serve --hostname=127.0.0.1 --port=P` prints `opencode server listening on http://H:P`
- Auth: `OPENCODE_SERVER_PASSWORD` → Basic `opencode:<pw>`; config: `OPENCODE_CONFIG_CONTENT` (pure passthrough), reads `~/.config/opencode/opencode.json` (JSONC), auth from `~/.local/share/opencode/auth.json`
- `GET /global/health` → `{"healthy":true,"version":"1.19.2"}`; `GET /api/info` → 404 (forces T3's v1 client)
- Endpoints: session CRUD/list, `/message`, `prompt_async`, `command`, `abort`, `summarize`, `fork`, `children`, `status`, SSE `/event` (server.connected + heartbeats), `/permission`, `/question`, `/mcp`, `/provider`, `/agent`, `/skill`, `/command`
- CLI: `--version`, `models --verbose`, `agent list`, `debug skill` (T3 parser formats)
- Providers: OpenAI-compatible streaming (baseURL/apiKey from config; OpenRouter, vllm, ollama, self-hosted), GitHub Copilot (oauth bearer + intent headers), native Anthropic
- Tools: bash, read, write, edit, glob, grep, webfetch, todowrite, question, task (subagents); MCP stdio JSON-RPC (initialize/tools/list/tools/call) exposed as `mcp_<server>_<tool>`
- Usage widget: writes T3-compatible `~/.local/share/opencode/opencode-ocmini.db` (never touches `opencode.db`)

Full endpoint/event contract: [SPEC-CONTRACT.md](SPEC-CONTRACT.md)

## Verify

```sh
cargo test --test contract          # 8 integration tests (spawn, auth, sessions, SSE, MCP)
cargo run --bin ocmini-fuzz         # differential shape fuzzer: spawns real opencode + ocmini, compares 17 scenarios
```

## TUI

Run `ocmini` with no arguments for a minimal chat TUI (in-process engine, streaming deltas,
tool lines, `/model <prov>/<id>`, `/new`, `/abort`, `/quit`, Ctrl+C aborts a running turn).

## Use with T3 Code

1. `cargo install --path .`
2. T3 Code → Settings → Providers → OpenCode → Binary path → `~/.cargo/bin/ocmini` → Refresh provider status
3. Revert by clearing the field — ocmini never writes any opencode config/data files.

## Storage

- Sessions: `$XDG_DATA_HOME/ocmini/ocmini.db` (WAL, messages+parts JSON, 30-day retention)
- Usage stats: `$XDG_DATA_HOME/opencode/opencode-ocmini.db` (T3 usage widget schema)
