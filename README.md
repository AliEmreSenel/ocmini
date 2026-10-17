# ocmini

A minimal OpenCode replica in Rust. One binary for terminal chat, coding tools, saved sessions and an OpenCode 1.x HTTP/SSE server.

## Quick start

Requires Rust 1.85+ and a C compiler for bundled SQLite.

```sh
cargo install --path . --locked
ocmini                                    # terminal chat
ocmini --model local/my-model --directory ~/project
ocmini serve --hostname 127.0.0.1 --port 4096
```

Terminal controls: Enter sends, bracketed paste inserts multiline text, Ctrl+C aborts the current turn, Ctrl+D exits.

To use it with T3 Code, set the OpenCode provider's binary path to the installed `ocmini` executable.

## Resume a session

```sh
ocmini --continue                         # latest session in this project
ocmini session list                       # saved IDs, titles and directories
ocmini --session ses_…                    # resume by ID
ocmini session list --directory ~/project # filter by project
```

`--continue` and `--session` also work with `ocmini tui`. Resuming shows the saved conversation and keeps its last model and agent; `--model` overrides the model. An explicit session ID uses the saved project directory. A normal launch starts a new session.

## What stays

- **Coding loop:** streaming replies and reasoning, file attachments, cancellation, steering, permissions and questions.
- **Tools:** `bash`, `read`, `write`, `edit`, `apply_patch`, `glob`, `grep`, `webfetch`, `todowrite`, `question`, `task` and `skill`.
- **Sessions:** SQLite history, resume, forks, child tasks, context compaction and recovery after interruption.
- **Providers:** OpenAI Chat Completions, OpenAI Responses and Anthropic Messages, including compatible endpoints.
- **Project setup:** OpenCode JSON/JSONC configuration, agents, commands, skills and `AGENTS.md` instructions.
- **MCP:** stdio and Streamable HTTP servers.
- **API:** OpenCode 1.x HTTP/SSE, model inventory and usage reporting.

## What gets cut

Desktop/web apps, hosted accounts and billing, plugin marketplaces and installers, auto-updates, telemetry, LSP management and the OpenCode 2.x service layer.

Provider support is limited to the three HTTP protocols above. Native Gemini, Vertex, Bedrock, Azure-specific authentication, arbitrary npm provider SDKs and WebSocket transports are excluded. Compatible HTTP endpoints can still be configured. MCP browser OAuth and legacy HTTP+SSE transport are also excluded.

## Configuration

Uses OpenCode-shaped `opencode.json` or `opencode.jsonc`. Global configuration lives in `$XDG_CONFIG_HOME/opencode`; project configuration lives in the project or `.opencode` directory. Environment overrides include `OPENCODE_CONFIG`, `OPENCODE_CONFIG_DIR` and `OPENCODE_CONFIG_CONTENT`.

Example for a local compatible server:

```json
{
  "model": "local/my-model",
  "provider": {
    "local": {
      "npm": "@ai-sdk/openai-compatible",
      "options": { "baseURL": "http://127.0.0.1:8000/v1" },
      "models": { "my-model": { "name": "My model" } }
    }
  }
}
```

Use the model ID served by your endpoint. Hosted providers can use `options.apiKey: "{env:YOUR_API_KEY}"`. Set `options.endpoint` to `chat`, `responses` or `messages` when needed.

Credentials come from `$XDG_DATA_HOME/opencode/auth.json`, `OPENCODE_AUTH_CONTENT`, provider configuration or supported environment variables. Existing OpenAI/Codex OAuth tokens can be refreshed; interactive login flows are excluded. Original OpenCode configuration and credentials are left untouched.

## Storage and server

Sessions are saved in `$XDG_DATA_HOME/ocmini/ocmini.db`; refreshed credentials use `ocmini/auth.json`. With XDG variables unset, the usual `~/.config` and `~/.local/share` defaults apply. OpenCode's transcript database is not imported.

History is kept unless `OCMINI_RETENTION_DAYS` enables pruning. Recovery marks interrupted work and preserves queued prompts without replaying potentially completed tool calls.

Set `OPENCODE_SERVER_PASSWORD` to require Basic authentication with username `opencode`. The server defaults to `127.0.0.1:4096`; `--port 0` chooses a free port. `ocmini --help` lists commands. The reported version, `1.19.3-ocmini.0.2.0`, identifies API compatibility.

## Development

```sh
cargo fmt --all -- --check
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo test --all-targets --locked
cargo build --release --locked
```

Tests use temporary state and local provider/MCP fixtures; no paid credentials are needed. Unit tests stay inline beside the code they cover. Integration tests are grouped by behavior under `tests/contract/`.

See [SPEC-CONTRACT.md](SPEC-CONTRACT.md) for API details and [VALIDATION.md](VALIDATION.md) for checks and coverage.
