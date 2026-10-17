# ocmini

A Rust coding runtime for T3 Code, implementing the OpenCode **1.x HTTP/SSE contract consumed by the supplied T3 snapshot**. One executable provides a terminal chat, an HTTP server, a durable session engine, three provider transports, coding tools and MCP clients. T3 supplies the application interface and orchestration when using the server.

Package version: `0.2.0`. Reported compatibility version: `1.19.2-ocmini.0.2.0`. The latter selects T3's OpenCode 1.x adapter and identifies an independent implementation; it is not an OpenCode release. The supplied newer `opencode-dev` sources also contain a different 2.x protocol. ocmini deliberately returns 404 at `/api/info` so T3 does not select that protocol.

Build and validation instructions are in [VALIDATION.md](VALIDATION.md). Tests use deterministic local provider/MCP peers and temporary state; paid-provider credentials are not required.

## Build and use with T3

Use Rust **1.85.0 or newer** with a C compiler/linker; SQLite is built from the bundled source. Linux is the initial validation target. The package declares its minimum Rust version and uses Cargo's Rust-version-aware resolver to keep dependency updates compatible.

```sh
cargo test --all-targets --locked
cargo build --release --locked
./target/release/ocmini --version
./target/release/ocmini models --verbose
```

Set **T3 Code → Settings → Providers → OpenCode → Binary path** to the absolute path of `target/release/ocmini`, then refresh provider status. Alternatively, install it with `cargo install --path . --locked` and use the installed binary path. The committed `Cargo.lock` pins dependency resolution.

T3 launches the server itself. For a manual server:

```sh
./target/release/ocmini serve --hostname 127.0.0.1 --port 4096
```

Both `--hostname=...`/`--port=...` and separated flags work. Port `0` requests an available port. The server prints the exact banner T3 expects after binding, with the actual address. With `OPENCODE_SERVER_PASSWORD` set, all routes require Basic authentication as user `opencode`.

Running with no arguments starts terminal chat. Use `ocmini tui --model provider/model --directory DIR` to select a model and project, or `ocmini --help` for command help. Terminal chat streams the same engine events as T3, including tool activity, permissions and questions. Bracketed paste inserts complete multiline prompts; Enter sends, Ctrl+C aborts the current turn and Ctrl+D exits. Terminal input and restoration use `crossterm` with Tokio shutdown signals.

## What the runtime implements

| Area | Behavior |
| --- | --- |
| T3 process probes | Version, server banner/health, model metadata, agent list and skill inventory in T3's parser formats |
| Prompts | Preserve client message IDs, system instructions, agent, variant, text/file parts and tool overrides; commit admission before HTTP 204 |
| Execution | One loop per session, durable prompt queue, idempotent retries, steering at tool boundaries, bounded agent steps and cancellation during network/tool/approval waits |
| Streaming | Persist and announce parts before deltas; text, reasoning, tool states, step usage, errors and final idle events |
| Sessions | Creation, resume, metadata updates, chronological messages, exclusive-boundary forks, actual child sessions and dependent-data deletion |
| Compaction | A model-generated continuation summary becomes the context boundary for future prompts; the original transcript remains available |
| Permissions | Ordered last-match rules, session/agent configuration, persisted always grants, external-directory checks and canonical file-path checks |
| Questions | Full question arrays, multiple selections, custom answers, rejection and cancellation |
| Commands | Config/Markdown command templates, quoted positional arguments, `$ARGUMENTS`, permission-controlled shell interpolation and attachment forwarding |
| MCP | Real stdio and Streamable HTTP initialization, paginated tool discovery, request correlation, notifications, headers, connection state, tool calls, cancellation and media results |
| Usage | Per-step provider counts, separate cache/reasoning categories, configured costs and a T3-readable SQLite usage mirror |

The full route/event specification, source references and field shapes are in [SPEC-CONTRACT.md](SPEC-CONTRACT.md).

### Provider transports

The runtime implements these protocols directly:

- **OpenAI Chat Completions**, including compatible endpoints, streaming tool arguments and reasoning fields.
- **OpenAI Responses**, including function calls, reasoning items, encrypted reasoning replay, attachments and existing OpenAI/Codex OAuth credentials.
- **Anthropic Messages**, including tool use/results, thinking/signatures, images/documents and cache usage.

GitHub Copilot uses the appropriate transport from discovered/configured model metadata, with provider-specific request headers and existing credentials. Configured OpenRouter, local model servers and other compatible services use the declared transport. Unknown SDK-specific protocols fail with an explicit error instead of sending an unrelated request format.

Provider/model options, API model aliases, headers, variants and explicit `maxTokens`/`maxOutputTokens` settings flow into requests. Anthropic also uses the declared model output limit when no explicit limit is set; Codex OAuth requests omit `max_output_tokens` for backend compatibility. Providers with no explicit model catalog can be queried at `/models`; discoveries are cached for five minutes. Configured metadata is preserved, and discovery normalizes supported limit, price and capability fields without inventing absent values. An explicit model catalog avoids discovery requests. For Codex or other servers without a usable `/models` endpoint, declare `model` or `provider.<id>.models`; discovery errors are reported.

Media support depends on the protocol and model. Images and supported file attachments are translated for all three protocols. Responses and Anthropic accept remote file URLs; Chat Completions requires local/base64 file attachments through the prompt interface. Chat Completions forwards wav/mp3 input audio when supplied by a tool; Anthropic and Responses reject unsupported audio parts explicitly. Other audio protocols and audio output are not implemented.

Provider streams are parsed across arbitrary UTF-8 and SSE chunk boundaries. Missing terminal events, malformed JSON, provider error events and stalled streams produce failures. Initial retryable request failures receive bounded retries; partially executed tools are not automatically replayed as a retry strategy.

### Coding tools

Built-ins are `bash`, `read`, `write`, `edit`, `apply_patch`, `glob`, `grep`, `webfetch`, `todowrite`, `question`, `task` and `skill`.

File tools return actual output, diffs and errors. Patch operations validate the complete patch before applying changes and preserve file permissions. Search respects ignore rules and reports truncation. Bash captures stdout/stderr and the actual exit code, terminates process groups on Unix during timeout/cancellation, and bounds captured output. Web fetch checks HTTP status, limits response size and converts HTML when requested.

`task` starts a real child session, publishes its session/model metadata while running and waits for its result. It supports configured subagents and continuation through `task_id`. Child permissions inherit the parent policy. `plan` and `explore` enforce read-only operation: execution/mutation tools and MCP tools are excluded and also rejected if called anyway.

System instructions include applicable `AGENTS.md` files, configured instruction files/URLs, the chosen agent prompt, skill inventory and T3's supplied system text. Skills are loaded from their actual Markdown files.

## Configuration and credentials

Configuration uses OpenCode-shaped JSON/JSONC, parsed by `serde_json_lenient` with comments and trailing commas enabled. Permission-rule order is preserved. Layers are merged in this order, with later values taking precedence:

1. Global `$XDG_CONFIG_HOME/opencode/opencode.json` and `opencode.jsonc`, plus global agent/command Markdown.
2. `OPENCODE_CONFIG`, when set.
3. Ancestor/project `opencode.json`, `opencode.jsonc` and `.opencode` configuration/agent/command files, unless `OPENCODE_DISABLE_PROJECT_CONFIG` is enabled.
4. `OPENCODE_CONFIG_DIR`, when set.
5. `OPENCODE_CONFIG_CONTENT`, including T3's overlay. An empty `{}` retains earlier configuration.

Configuration supports environment and file substitutions, provider/agent/command metadata, `default_agent`, provider allow/deny lists, tool permissions and custom skill paths. Invalid configuration reports an error instead of silently disappearing. Directory-scoped requests load configuration for that project; MCP hubs are scoped to the same directory.

For example, a local Chat Completions-compatible endpoint can use:

```json
{
  "model": "local/my-model",
  "provider": {
    "local": {
      "npm": "@ai-sdk/openai-compatible",
      "name": "Local model server",
      "options": {
        "baseURL": "http://127.0.0.1:8000/v1"
      },
      "models": {
        "my-model": { "name": "My model" }
      }
    }
  }
}
```

Replace `my-model` with the actual served model ID. Hosted endpoints can use `options.apiKey: "{env:YOUR_API_KEY}"` and their real base URL. Set `options.endpoint` to `chat`, `responses` or `messages` when transport selection needs to be explicit. Model-specific options and variants can override provider options.

Credentials are read from `$XDG_DATA_HOME/opencode/auth.json`, `OPENCODE_AUTH_CONTENT`, provider configuration or supported provider environment variables. Existing OpenAI/Codex OAuth tokens can be refreshed; rotated credentials are stored atomically in ocmini's own credential cache, tied to the original source credential. The original OpenCode credential and configuration files are not rewritten. Interactive browser/device authorization and non-OpenAI vendor OAuth refresh flows are outside this runtime; expired unsupported credentials produce a reconnect/API-key error. Well-known discovery credentials are not treated as API tokens.

### Limits

Limits are explicit so a failed or truncated operation is distinguishable from success:

| Limit | Default / setting |
| --- | --- |
| HTTP JSON request body | 128 MiB; `OCMINI_MAX_REQUEST_BYTES` |
| Native local prompt attachment | 20 MiB per file, matching the supplied T3 adapter's native-file threshold |
| Provider response headers | 60 seconds; `provider.<id>.options.headerTimeout` in milliseconds, or `false` |
| Provider interval between chunks | 120 seconds; `options.chunkTimeout` in milliseconds |
| Provider total request timeout | Optional `options.timeout` in milliseconds |
| MCP request | 30 seconds; server `timeout` in milliseconds, up to 600000 |
| Agent steps | 100; agent `steps` or `maxSteps`, capped at 1000 |
| Nested task sessions | 8 levels |

Built-in tools also bound individual file reads, search results, fetched response bodies and captured shell output, and expose truncation metadata. These limits do not silently truncate the durable transcript. Automatic token-budget compaction is not implemented; use T3's compaction action before exceeding the selected model's context limit.

## Persistence and recovery

| File | Purpose |
| --- | --- |
| `$XDG_DATA_HOME/ocmini/ocmini.db` | Sessions, messages, parts, durable prompt inbox and todos; SQLite WAL |
| `$XDG_DATA_HOME/ocmini/auth.json` | Refreshed credential cache, with restrictive permissions on Unix |
| `$XDG_DATA_HOME/opencode/opencode-ocmini.db` | Usage-only mirror discovered by T3; separate from `opencode.db` |

When XDG variables are unset, the conventional home-directory defaults apply. The schema extends the original ocmini database in place. It does not import OpenCode's transcript database. As with any schema update, preserve a copy of existing state before moving a production deployment to this version.

Startup recovery marks interrupted assistant/tool work as interrupted, clears stale busy flags and resumes only admitted prompts that had not started. It does not replay possibly completed shell or file side effects. Forks receive new message/part IDs, remapped references and retained history, without duplicating usage in the mirror. Session deletion cleans messages, parts, queued prompts, todos and descendant sessions.

Transcript retention is off by default. `OCMINI_RETENTION_DAYS=N` opts into pruning eligible inactive sessions older than N days at startup; `0` disables pruning. Queued sessions and sessions referenced by children are protected. Usage totals have independent retention so deleting a transcript does not erase recorded usage. No fixed-size database growth claim is made.

## What is intentionally omitted

There is no desktop/web UI, hosted account/billing service, marketplace, automatic plugin/package installation, updater, telemetry pipeline, LSP fleet or OpenCode 2.x background-service layer. Those features are not consumed by the supplied T3 1.x integration.

The provider boundary is the three implemented HTTP protocols above. Native Google/Gemini, Vertex, Bedrock, Azure-specific authentication, the native Ollama API, arbitrary npm provider SDKs and provider WebSocket transports are not implemented. Services exposing a supported compatible endpoint can use explicit configuration. Legacy MCP HTTP+SSE transport and MCP browser OAuth are also excluded. T3's Streamable HTTP tool bridge with explicit headers and `oauth: false` is implemented. MCP registrations follow OpenCode's directory/name scope; registering the same name again in the same directory replaces that registration, rather than inventing a session scope absent from the API.

## Verification

```sh
cargo fmt --all -- --check
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo test --all-targets --locked
cargo build --release --locked
```

Rust tests use local deterministic provider/MCP peers and temporary state. Coverage includes the HTTP/CLI contract, authentication, scoping, exact client IDs, admission, steering, abort, fork boundaries, part-before-delta ordering, commands, attachments, approvals/questions, compaction reuse, provider parsing, MCP failures and tool behavior. They do not require paid-provider credentials.

Persistence checks execute the actual Rust store with bundled SQLite, including transactions, serializers, crash recovery, legacy migration and descendant deletion. CI runs tests, Clippy and a release build on Rust 1.85.0 and current stable, with compiler and lint warnings treated as errors. Formatting is checked on stable. Successful live-provider calls, full T3 application sessions and performance measurements still require validation in the target environment.
