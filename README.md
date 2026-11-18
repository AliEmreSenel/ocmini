# ocmini

A minimal OpenCode replica in Rust. One binary for terminal chat, a browser interface, coding tools, saved sessions and an OpenCode v2 HTTP/SSE server. Legacy v1 routes remain available during the transition.

The lightweight runtime supports use on single-core machines with roughly 500 MB of total system RAM, including embedded Linux devices using SD cards. V2 work should preserve the current approach and performance, using the same behavior across machines.

## Quick start

Prebuilt binaries are available on the [GitHub Releases page](https://github.com/AliEmreSenel/ocmini/releases) for Linux (x86_64 glibc/musl, ARM64, ARMv7 and ARMv6 hard-float), macOS (Apple silicon) and Windows (x86_64). For ARMv6, use the `arm-unknown-linux-gnueabihf` archive; ARMv7 uses `armv7-unknown-linux-gnueabihf`.

Building requires Rust 1.85+ and a C compiler for bundled SQLite. Search tools require ripgrep (`rg`) on `PATH`.

```sh
cargo install --path . --locked
ocmini                                    # terminal chat
ocmini --model local/my-model --directory ~/project
ocmini serve --hostname 127.0.0.1 --port 4096
ocmini web                                # same server; open http://127.0.0.1:4096/
```

The input shows a compact status row that updates in place. Type `/help` or press F1 for the full shortcut list; F1 keeps your current draft. Help is handled locally.

Terminal controls: Enter sends, bracketed paste inserts multiline text, Ctrl+V (or Ctrl+Shift+V when forwarded by the terminal) attaches a clipboard image, and pasting an image file path attaches it. Alt+Backspace removes the latest attachment; Backspace also removes it when the text is empty. Ctrl+U clears text and attachments. Ctrl+C aborts the current turn, Ctrl+D exits, Ctrl+R opens the session selector. The draft wraps across rows and preserves pasted line breaks. Backspace removes whole Unicode graphemes and stays within the draft.

Undo records changes rather than copying the draft. Consecutive typing is one undo step, and sending a prompt or switching sessions clears undo history. History is limited to 1 MiB and 1,000 edits; a deletion too large to fit clears undo history. Large text pastes do not multiply memory use with each subsequent keystroke.

Questions appear one at a time with their numbered options. The answer line is the same full editor as the prompt - caret movement, selection, word motions, kill, yank, undo and multiline all work on the answer - and the prompt you were typing stays untouched underneath, returning when the question settles. A line of option numbers answers with those options; any other text is sent as a custom answer exactly as typed. Enter walks to the next question and wraps back to the first, and each answer is kept in the transcript. Tab and Shift+Tab walk the questions forward and back - each question keeps its own draft, so switching never loses a half-typed answer, and answered questions reopen prefilled so they can be changed. Backspace on an empty answer line also steps back, and Esc dismisses the request. When the last answer lands, a confirmation screen lists every question with its answer - Enter sends, Tab steps back to edit, Esc dismisses. Your prompt is never sent while a question is open.

Transcript scrolling is a config option, `ui.scroll_mode`:

- `native` (default): completed transcript rows commit to the terminal's own scrollback and only the prompt is redrawn. Wheel and terminal shortcuts scroll as usual; PageUp/PageDown page the draft.
- `fullscreen`: ocmini repaints the whole screen on the alternate screen and scrolls the transcript itself. PageUp/PageDown page, Ctrl+Home/Ctrl+End jump to the top and bottom, and the mouse wheel scrolls (Shift+wheel keeps the terminal's native selection scroll). Help lists these keys. On exit the visible transcript is restored to the terminal's scrollback, using the selected thinking/tool view. Completed rows are wrapped once per width and visibility setting, so ordinary repaints process only new rows and the live tail.

Set it in `opencode.json` before starting; a running TUI keeps the mode it started with:

```json
{ "ui": { "scroll_mode": "fullscreen" } }
```

Two transcript toggles work while you chat, and the status row shows the current state when space allows:

- In `fullscreen` mode, `Ctrl+T` collapses each thinking block to one row, such as `◦ Thought for 2.5s (42 tokens)`. Press it again to restore the text. Durations use the block’s timestamps; token counts use reported reasoning usage when available, or an estimate marked `~`. Native mode keeps its existing thinking visibility toggle.
- In `fullscreen` mode, `Ctrl+O` cycles through three views for every tool chain, including saved and running calls:
  1. **Grouped**: one summary, such as `▸ 100 tool calls · bash × 80, read × 20`, with pending/running counts and a visible failure count.
  2. **Brief** (default): every call with its command and a short result immediately underneath.
  3. **Full**: complete commands and results, with multiline output indented under each call. Each command or result is capped at 50 KiB and says how much was omitted.

A tool group contains only consecutive tool calls between messages. An intervening assistant message or thinking block ends the group, including when thinking is collapsed to its summary row. Calls stay in chronological order and update in place while the model generates the batch and the tools run. Streamed narrative newline runs render as a single blank separator; fenced code blocks and tool output keep their original blank lines.

Narrative and thinking text render markdown as they stream and when loaded from history: a fenced block becomes a `│` gutter with its language as the label above it, `#` headings go bold, `**bold**` and `` `code` `` spans take the app's styling, and a marker with no closing match prints as typed. Tool results keep their own formatting, so their markers stay verbatim. Tables print as typed, without column alignment: a stream has no widths to align to before its first row prints, and a committed row cannot be rewritten. Block-level rows - the `❯ user` prompt landmark, the `assistant` label, notices, tool chains and thinking blocks - get exactly one blank row before them, from one owner in the display, so a header carries no leading newline of its own. A prompt's text is indented two columns under its landmark, and one `assistant` label marks a whole answer rather than each part of it. Transcript text paints only the app's own styling: any other escape sequence from a model, tool, web page, MCP server, hook, title or filename is dropped, as documented in `SECURITY.md`.

In `fullscreen` mode these controls re-render the retained transcript at any scroll position. In `native` mode, `Ctrl+O` keeps the two brief/full views and affects new output; the terminal owns previously printed scrollback. Running calls use a bounded live area, with wrapped and multiline rows counted correctly, so generating a large batch does not accumulate blank rows. Each completed native call prints its header and result together.

Clipboard image paste needs `wl-paste` (Wayland), `xclip` (X11), `osascript` (macOS) or PowerShell (Windows). Many terminals handle Ctrl+Shift+V themselves: pasted text and image paths work, but a screenshot cannot reach the app if the terminal sends no paste event or image data. Use Ctrl+V for a direct clipboard read, or configure the terminal to forward Ctrl+Shift+V. PNG, JPEG, GIF and WebP images are supported. Quoted paths, shell-escaped spaces and percent-encoded `file://` URLs work too.

Over SSH, ordinary terminal paste carries text, not your local image clipboard. Copy the image to the remote host (for example with `scp`) and paste its **remote** path. X11 clipboard forwarding can also work when `DISPLAY` is forwarded and `xclip` is installed on the remote host. Clipboard images are stored with the message, so they remain available when resuming a session.

To use it with T3 Code, set the OpenCode provider's binary path to the installed `ocmini` executable.

## Browser interface

`ocmini serve` (also available as `ocmini web`) serves a browser interface at `/` alongside the API, so you can code from any device that can reach the server. It prints the address on stderr after the unchanged listening banner. The page is built into the binary: no Node.js, build step, CDN or extra process.

- **Chat:** every saved session (including terminal and T3 chats), grouped by project or filtered to the current one, with search, rename, archive and delete. Opening a chat switches to its project; streamed answers, reasoning and tool calls (with edit diffs); model, variant and agent selection; steer or queue delivery while the agent works; Stop (or Esc) interrupts; file attachments by picker, paste or drop; `/` lists configured commands and `/compact`; rewind to or fork from any of your messages.
- **Shared sessions:** any number of browsers or devices can open the same chat on one server. Each sees the live stream, the same pending inputs, approvals and questions, and can steer or queue while the agent works. Steers are taken at the next model step boundary. A chat that another ocmini process is running, such as T3's own server, is followed from its saved history every 2 seconds while it is open and running there. New input waits until that process finishes, since its live state, approvals and interrupts belong to it.
- **Approvals and questions** appear as cards above the composer and are answered in place.
- **Sandbox per session:** the chat header picks the sandbox preset. A new session starts directly under the chosen preset. Changing an existing root session's preset requires it to be idle: its runner and background jobs stop, and the next turn uses the new settings. Subagents keep their parent's sandbox, and `--sandbox-preset` locks the choice.
- **Settings (⚙):** default model, small and title models, default sandbox and compaction switches, saved to the global or project `opencode.json`. The merged configuration is validated after each save, and an invalid result restores the file. JSONC files with comments are opened in the editor instead of being rewritten. Everything else (providers, agents, permissions, MCP, hooks) is edited in the file.
- **Files:** a lazily loaded tree with new file/folder, rename/move and delete, and a tabbed editor with line numbers, indentation keys and Ctrl+S. Saves are atomic and refuse to overwrite a file that changed on disk since it was opened (for example by the agent) unless you confirm. Clean open files reload after the agent edits them. "Attach" adds the current file to the next prompt.

Click the project path in the header to open another directory or a known project. The layout switches between chat and editor on narrow screens. Text files are streamed to the browser rather than buffered on the server. The editor opens binary or non-UTF-8 files read-only or not at all.

The interface uses the same v2 HTTP/SSE API as other clients, plus `/web/api/*` routes for files, settings and sandbox selection. These do their work per request and keep no state between requests. Settings live only in the existing configuration files. The server adds no background work for the interface: an open page holds one event-stream connection. Browser file operations run as the server's OS user, like a local editor would. They are not routed through agent permissions or the sandbox, and they share the agent's per-file edit locks. Set `OPENCODE_SERVER_PASSWORD` before exposing the server beyond loopback; the browser then asks for the `opencode` user's password. Build with `--no-default-features --features sandbox` to leave the interface out.

## Resume a session

```sh
ocmini --continue                         # latest session in this project
ocmini --resume                           # interactive session selector
ocmini session list                       # saved IDs, titles and directories
ocmini --session ses_…                    # resume by ID
ocmini session list --directory ~/project # filter by project
```

The selector lists saved sessions with live search (title, ID, directory), a cycling sort order (updated/created/title, Tab) and arrow/page navigation; Enter resumes, Esc cancels. `Ctrl+R` opens it inside the chat to switch sessions.

A new session is named after the first line of its first prompt, so the list shows what each conversation was about instead of `tui session`. A title you set (`ocmini run --title …`, `PATCH /session`) is kept.

`--continue` and `--session` also work with `ocmini tui`. Resuming shows the saved conversation and keeps its last model and agent; `--model` overrides the model. An explicit session ID uses the saved project directory. A normal launch starts a new session.

## One-shot prompts

```sh
ocmini run "explain this project"            # new session, prints the answer
git diff | ocmini run "review this diff"     # piped stdin appends to the prompt
ocmini run --continue "and the tests?"       # keep the latest session here
ocmini run -s ses_… --fork "try another way" # fork a session by ID
ocmini run --format json "summarize"         # JSONL: step_start, text, step_finish
ocmini run -f screenshot.png "what changed?" # attach files (repeatable)
ocmini run --auto -m provider/model "…"      # allow tools without asking
```

`run` drives the same engine as the TUI: sessions, agents, models, tools, permissions and questions all behave the same. Text mode prints the agent/model header, tool lines and the streamed answer. `--thinking` also prints chain of thought. `--format json` emits one JSON object per line (`step_start`, `text`, `step_finish`, `tool_use`, and `reasoning` with `--thinking`) for scripts. In a non-interactive run, shell, file mutations, subagents, web fetches and MCP tool calls require `--auto`. Other tools follow their configured permissions. A terminal can answer approval prompts with `[y]es/[a]lways/[n]o`. With `--auto`, the CLI approves pending tool requests for that invocation. Explicit denies still apply. `--interactive` (`-i`) keeps prompting after the first answer (exit with `/exit` or Ctrl+D); `ocmini` without `run` is the full TUI. Flags: `--agent`, `--variant`, `--title`, `--dir`, `--command`, `--model`.

## What stays

- **Coding loop:** streaming replies and reasoning, file attachments, cancellation, steering, permissions and questions.
- **Tools:** `bash`, `read`, `write`, `edit`, `apply_patch`, `glob`, `grep`, `webfetch`, `todowrite`, `question`, `task` and `skill`.
- **Sessions:** SQLite history, resume, forks, child tasks, context compaction and recovery after interruption.
- **Providers:** OpenAI Chat Completions, OpenAI Responses and Anthropic Messages, including compatible endpoints.
- **Project setup:** OpenCode JSON/JSONC configuration, agents, commands, skills and `AGENTS.md` instructions.
- **MCP:** stdio and Streamable HTTP servers.
- **API:** OpenCode v2 HTTP/SSE (plus transitional v1 routes), model inventory and usage reporting.

Chat Completions models must declare `modalities.input` containing `"pdf"` to receive binary file attachments. Otherwise, ocmini sends an explicit unread-attachment notice instead of a `file` content part, including when replaying saved tool results. Text attachments still arrive as text; Responses and Anthropic retain their native file handling.

If a provider explicitly rejects an image as truncated or undecodable, ocmini retries once with image placeholders and displays a notice. Because the provider may not identify the broken image, all images in that rejected request are omitted from future model context, including after restart. Original attachments remain in history; reattach any images needed. New image attachments are sent normally. Other provider errors still surface normally.

Tool text over 50 KiB is saved under `$XDG_DATA_HOME/ocmini/tool-output` (default `~/.local/share/ocmini/tool-output`). The conversation keeps a preview of the beginning and end plus the saved path, so omitted content can be read or searched later. Large shell output is captured to disk with bounded memory. Edits and patches also apply the read policy before inspecting file contents. Existing file-read/search paging and media limits still apply. Full output files are retained by default so resumed sessions can still read them. Set `OCMINI_TOOL_OUTPUT_RETENTION_DAYS` to a positive integer to remove expired output files at startup; unset or `0` retains them. These files use disk space separately from session history.

A normal `bash` call ends its whole process group when the command exits, so `cmd &` and `nohup cmd &` do not survive it. For work that must outlive the call, pass `background: true`: the command starts in its own session, the tool returns its pid and log path at once, the job keeps running across tool calls, and `kill <pid>` stops it. A background job belongs to the session that started it: aborting the turn, deleting the session, or stopping ocmini ends it. Its log is not size-bounded and expires with `OCMINI_TOOL_OUTPUT_RETENTION_DAYS` like other output files. A command that reaches its `timeout` completes with the output it printed before the limit plus a `shell_metadata` notice, so a cancelled build still shows its work; aborting a turn discards the result.

Launching a subagent with `task` also accepts `background: true`: the child session starts and the tool returns its id at once; when the child finishes, its result is injected into the parent as a synthetic user message and wakes it for a follow-up turn (steered into the live turn while the parent is busy, a fresh turn when it is idle). Aborting the child's session cancels it and drops its result silently. Background tasks are process-local and do not survive a restart.

In the terminal, `/context` estimates the current projected instructions, tool schemas, messages and tool results, and shows recent observed usage and the model window when known. `/compact [instructions]` requests a summary with optional focus instructions; the web chat supports the same compact syntax. The v2 compact endpoint accepts the optional ocmini-specific `ocminiFocus` string. Estimates use four bytes per token, not a tokenizer. These commands are inspired by [Claude Code’s context and compact commands](https://code.claude.com/docs/en/commands).

Compaction can be requested manually, runs automatically with room reserved for model output, and recovers from provider context-overflow errors. It can repeat throughout a task, including within one model step while each checkpoint reduces the overflowing request. Recovery stops when compaction fails or no longer reduces that request. Subsequent requests use the latest valid summary, recent messages kept verbatim, and newer messages; the original history remains saved. The default recent-message budget is roughly a quarter of usable context, between 2,000 and 15,000 tokens. Complete tool call/result groups are retained when they fit, with the latest user message taking priority. Budgets use approximate token estimates. Unknown context windows rely on manual compaction or provider overflow recovery.

`grep` and `glob` stream ripgrep results, return up to 100 matches, and use ripgrep's ignore and glob rules. Explicit globs can override ignore rules. Search has no additional candidate-file ceiling. Text reads accept files of any size and default to 2,000 lines; `offset` and `limit` page through them without a separate 20,000-line ceiling. Each line is capped at 2,000 characters and each file page at 50 KiB of text. Reads validate UTF-8 and binary content while retaining a bounded line preview. Media attachment limits still apply.

## What gets cut

The upstream desktop and web apps (ocmini ships its own lightweight browser interface instead), hosted accounts and billing, plugin marketplaces and installers, auto-updates, telemetry, LSP management and upstream managed-service infrastructure.

Provider support is limited to the three HTTP protocols above. Native Gemini, Vertex, Bedrock, Azure-specific authentication, arbitrary npm provider SDKs and WebSocket transports are excluded. Compatible HTTP endpoints can still be configured. MCP browser OAuth and legacy HTTP+SSE transport are also excluded.

## Configuration

Uses OpenCode-shaped `opencode.json` or `opencode.jsonc`. Global configuration lives in `$XDG_CONFIG_HOME/opencode`; project configuration lives in the project or `.opencode` directory. Environment overrides include `OPENCODE_CONFIG`, `OPENCODE_CONFIG_DIR` and `OPENCODE_CONFIG_CONTENT`.

MCP configuration from a checkout or its ancestor directories requires `OCMINI_TRUST_PROJECT_MCP=1` in the launch environment. Global and explicitly supplied configuration can still register MCP servers. Set the opt-in only when you trust the project configuration, since a local MCP command runs with your OS permissions.

Set `"deferred_mcp_discovery": true` to hide MCP schemas until the model searches for them using `discover_mcp_tools`. Matching tools become available on subsequent steps of the same turn. Discovery searches names and descriptions from the hook-transformed catalog, and tool calls require the usual permissions. After a reconnect or a tool list change, the next step uses the updated catalog and tools from that server require discovery again. Each new turn also starts with discovery. The option defaults to false.

For example, set `read_only: true` in Serena’s `/path/to/project/.serena/project.yml` (create the project configuration with `serena project create` if needed). Then configure the single-project `ide` context in a trusted ocmini configuration. The permission rules allow only the listed navigation tools:

```json
{
  "deferred_mcp_discovery": true,
  "mcp": {"serena": {"type": "local", "command": ["uvx", "--from", "git+https://github.com/oraios/serena", "serena", "start-mcp-server", "--context", "ide", "--project", "/path/to/project"]}},
  "permission": {"serena_*": "deny", "serena_find_file": "allow", "serena_find_referencing_symbols": "allow", "serena_find_symbol": "allow", "serena_get_symbols_overview": "allow", "serena_list_dir": "allow", "serena_search_for_pattern": "allow"}
}
```

Replace the project path. Serena’s project read-only setting disables editing tools; ocmini permissions further restrict which Serena tools can run. MCP servers run with the server process's OS permissions.

`title_model` names new sessions with a model: `provider/model` picks one, `small` starts at `small_model`, and the default `""` writes titles from the first prompt's first line with no model call. A title model that is unavailable, fails or answers with nothing usable moves to the next (`small_model`, then the session's model), and the prompt's first line remains the last resort. Terminal chat shows `◦ generating title…` while a title is written.

Agent turns have no step limit by default. To cap model/tool rounds explicitly, set `agent.<name>.steps` (or the legacy `maxSteps`) to a positive integer. Ctrl+C or the abort endpoint stops an active turn.

`subagent_depth` sets the maximum nesting depth and defaults to **8**; `0` disables child tasks. The `compaction` object supports OpenCode's settings:

- `auto`: automatic proactive and overflow-triggered compaction, default `true`. Manual summaries remain available when disabled.
- `prune`: clear older completed tool results from model context, default `false`. The latest two user turns, roughly 40,000 tokens of tool results and skill outputs are protected; pruning runs when more than 20,000 older tokens can be cleared. Original results remain browsable.
- `reserved`: override the token reserve when the model supplies a separate `limit.input`; otherwise usable context is `limit.context` minus the output allowance.
- `preserve_recent_tokens`: override the adaptive verbatim-tail budget; `0` keeps only the summary.
- `tail_turns`: limit the number of recent user turns retained verbatim; `0` disables the verbatim tail. When omitted, only the token budget limits the tail.

Shell tools default to a 120,000 ms timeout and MCP operations to 30,000 ms; larger explicit positive timeouts are allowed. Web fetches default to 30 seconds, clamp larger requested timeouts to 120 seconds, and accept responses up to 5 MiB. Shell and web timeouts are tool arguments; MCP timeouts are set in each server's configuration. Transient provider failures allow five retries per model step.

To enable seven-day full-output expiry, start ocmini with `OCMINI_TOOL_OUTPUT_RETENTION_DAYS=7`. Expiry runs at startup; unset or `0` keeps outputs indefinitely.

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

A model can declare the reasoning controls it accepts with `provider.<id>.models.<id>.reasoning_options`: `{ "type": "effort", "values": ["low", "high"] }`, `{ "type": "budget_tokens", "min": 4000, "max": 12000 }` on a `messages` endpoint, or `[]` for none. These build the model's variants and replace the ones discovery derives; a `variants` object still merges on top. An effort maps to `reasoningEffort` on Chat Completions and Responses, and to adaptive thinking with an output effort on Messages.

Credentials come from `$XDG_DATA_HOME/opencode/auth.json`, `OPENCODE_AUTH_CONTENT`, provider configuration or supported environment variables. Existing OpenAI/Codex OAuth tokens can be refreshed; interactive login flows are excluded. Original OpenCode configuration and credentials are left untouched.

## Storage and server

Sessions are saved in `$XDG_DATA_HOME/ocmini/ocmini.db`; refreshed credentials use `ocmini/auth.json`. With XDG variables unset, the usual `~/.config` and `~/.local/share` defaults apply. OpenCode's transcript database is not imported.

History is kept unless `OCMINI_RETENTION_DAYS` enables pruning. Recovery marks interrupted work and preserves queued prompts without replaying potentially completed tool calls.

Set `OPENCODE_PASSWORD` (or `OPENCODE_SERVER_PASSWORD`) to require Basic authentication with username `opencode`. The server defaults to `127.0.0.1:4096`; `--port 0` chooses a free port. `ocmini --help` lists commands. The reported version, `2.2.5-ocmini.<package version>`, identifies API compatibility.

Async I/O uses one Tokio worker by default. Synchronous database operations hand the scheduler to another worker while they run, so database lock waits do not stall the async event loop. File operations and history loading also use the blocking pool. Set `TOKIO_WORKER_THREADS` to a positive integer to choose a different async worker count. SQLite checkpoints allow oversized journal files to shrink to 1 MiB on reuse; active transactions can temporarily exceed this size.

## Development

```sh
cargo fmt --all -- --check
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo test --all-targets --locked
cargo build --release --locked
```

Tests use temporary state and local provider/MCP fixtures; no paid credentials are needed. Unit tests stay inline beside the code they cover. Integration tests are grouped by behavior under `tests/contract/`.

See the [v2 API contract](docs/api-v2.md) and [legacy API contract](docs/api-contract.md) for API details.

The [OpenCode v2 migration plan](docs/opencode-v2-plan.md) compares our current implementation with upstream v2.2.5 and T3 Code, including endpoint coverage and the v1 compatibility decision.

## Hooks and event subscriptions

Extensions use hooks to validate or transform operations and event subscriptions to observe execution. Register commands under `ocmini.hooks`, or consume the directory-scoped SSE stream through `GET /event`. Start with [hooks guide](docs/hooks.md) and the documentation-only [hook recipes](docs/hooks-examples.md); [hook reference](docs/hooks-reference.md) contains all 26 hooks, 49 events, and advanced settings. Inspect the active configuration with `ocmini debug hooks --json`. OpenCode JavaScript plugins use a separate interface.

## Sandboxed environments

The sandbox also ships a standalone `ocmini-sandbox` binary that runs one
command without ocmini's tools or session database. `cargo install --path .`
installs it next to `ocmini` (it needs the default `sandbox` feature):

```sh
cargo install --path . --locked                      # ocmini and ocmini-sandbox
cargo install --path . --locked --bin ocmini-sandbox # only the sandbox

ocmini-sandbox -- ls -la
ocmini-sandbox --preset locked-down -C ~/project -- sh
ocmini-sandbox --preset workspace-write --sandbox-network isolated -- make test
```

The standalone command defaults to `read-only`, or `OCMINI_SANDBOX_PRESET` when
set. `--preset` and `--sandbox-preset` select the same preset files described
below; all `--sandbox-*` policy overrides work here too. `--presets-dir DIR`
selects another preset directory, installing defaults only when it does not
exist. It does not read ocmini's application configuration. `-C`/`--directory`
sets the working directory and `${workspace}`. Place the program and its
arguments after `--`; they execute directly. Use `sh -c '…'` for shell syntax.

Standard input, output and error pass through, including terminal input and
pipes. Commands keep their exit code; signal exits return `128 + signal`.
Missing commands return 127, commands that cannot execute return 126, and sandbox
startup failures return 78. Background descendants are stopped when the command
finishes. Temporary sandbox machinery is removed afterward; no ocmini session
is created. Linux enforcement and networking requirements below apply equally
to this binary. Selecting `off` explicitly executes with normal host rights.

Linux builds include sandbox support; the default preset is `off`. Select a preset
in the existing OpenCode configuration:

```json
{"sandbox":"workspace-write"}
```

The object form `{"sandbox":{"preset":"workspace-write"}}` is also supported.
The same setting applies to T3 Code's `ocmini serve` process, `ocmini run`, and the
interactive TUI. A process-wide override is available as
`--sandbox-preset NAME` or `OCMINI_SANDBOX_PRESET=NAME`.

On first application startup, preset files are installed in
`$XDG_CONFIG_HOME/opencode/sandbox/presets/` (normally
`~/.config/opencode/sandbox/presets/`). The files define new preset selections:
edit them, add presets, or remove them. Shipped presets are `off`, `read-only`,
`workspace-write`, `locked-down`, `read-all`, `read-all-net`, and `locked-read`.
The former `strict` preset is no longer shipped. Their execute lists use
`${env:PATH}` directly, alongside explicit system runtime requirements. Preset
generation performs no filesystem or toolchain discovery. Scratch and session
tool-output storage are always granted separately. Existing preset files are preserved; replace any
previously generated fixed project path with `${workspace}` to make it dynamic.

| Preset | Host reads | Workspace writes | External networking | Full process isolation |
| --- | --- | --- | --- | --- |
| `off` (default) | Normal OS rights | Normal OS rights | Host network | No jail |
| `read-only` | Granted paths | No | Isolated | No |
| `workspace-write` | Granted paths | Yes | Inbound and outbound allowed | No |
| `locked-down` | Granted paths | Yes | Isolated | Yes |
| `read-all` | All paths | Yes | Isolated | No |
| `read-all-net` | All paths | Yes | Inbound and outbound allowed | No |
| `locked-read` | Granted paths | No | Isolated | Yes |

Every active preset uses user/mount namespaces and filesystem fences. Full
process isolation additionally creates a PID namespace with fresh `/proc` and
signal scoping. The read-all variants use `fs.readable: ["/"]`; their writes
remain restricted to the workspace and temporary machinery. The other active
defaults read the workspace and listed system runtime roots. Execute roots use
the host PATH directories plus `/lib` for the dynamic loader. `read-only` and
`locked-read` omit broad `/proc`, `/sys`, and `/dev` read grants; the writable
restricted-read defaults include those roots. All active defaults provide a writable private
`/tmp`, scratch and session output even in `read-only`. Their environment is cleared except for `HOME`, `USER`, terminal
settings, locale and timezone; `PATH` is rebuilt from executable roots. All ship
`unix_sockets: "abstract"` and `io_uring: true`.

Filesystem rules support `${workspace}`, `${home}`, `${user}`, `${uid}`, `${gid}`,
`${config_home}`, `${data_home}`, `${cache_home}`, and `${env:NAME}`. For example:

```json
"fs": {
  "readable": ["${env:PATH}", "${workspace}", "/usr", "/etc", "/lib"],
  "writable": ["${workspace}"],
  "exec": ["${env:PATH}", "/lib"]
}
```

`${workspace}` uses the session directory, including T3's selected workspace,
CLI directory selection, and resumed sessions. User, home and XDG variables
describe the host process; `${uid}` and `${gid}` are its effective IDs before
entering a user namespace. Ordinary path variables substitute a single value.
A whole filesystem-list entry `${env:NAME}` expands the environment variable as
a platform-separated path list (`:` on Linux, `;` on Windows). This works for any
environment variable, including `${env:PATH}` and your own directory lists.
Entries must be absolute, nonempty paths without `..`; duplicate grants are
removed while preserving order. Missing entries in an environment path list
are skipped for read/write/exec grants, matching normal command lookup.
Missing `fs.hide` targets are refused. Explicit literal paths retain
their enforce-or-refuse behavior. A variable containing several paths must
occupy the whole list entry.
Expansion reads the host environment before the sandbox filters environment
variables or rebuilds `PATH`. For example, add all host PATH directories:

```sh
ocmini run --sandbox-preset workspace-write \
  --sandbox-read '${env:PATH}' --sandbox-exec '${env:PATH}' "Your prompt"
```

The same syntax works in preset `fs.readable`, `fs.writable`, `fs.exec`, and
`fs.hide` arrays. List aliases and implicit runtime path discovery are
unsupported. Tools that need data outside their PATH directories require
explicit read grants, which can use additional environment variables. Unknown,
empty or unset variables and paths that resolve to relative paths or contain `..` are
refused. Expansion runs once after preset inheritance, with no shell evaluation
or recursive expansion.

CLI overrides apply to the current invocation and work before or after `run`,
`tui`, or `serve`. For example, add a data directory and change networking:

```sh
ocmini run --sandbox-preset workspace-write \
  --sandbox-read '${home}/datasets' \
  --sandbox-write '${home}/datasets' \
  --sandbox-network outbound-only "Work with the dataset"
```

`--sandbox-read`, `--sandbox-write`, `--sandbox-exec`, and `--sandbox-hide` append
paths and can be repeated. Executable roots must also be readable; hidden paths
require `process_isolation: full`. Quote path variables to keep the shell from
expanding them. Filesystem grants must resolve to absolute paths and exist when
the jail starts. Executable roots must also be representable in the platform's
`PATH`; invalid entries refuse startup instead of clearing command lookup.

`--sandbox-network MODE` replaces the entire network block, with modes `host`,
`outbound-only`, `inbound-only`, `isolated`, or `firewall`. Its `firewall` policy
starts with inbound and outbound denied; add allowed rules explicitly:

```sh
ocmini serve --sandbox-preset workspace-write \
  --sandbox-network firewall \
  --sandbox-add 'network.outbound.allow={"protocol":"tcp","destination":"*.github.com","port":443}'
```

`--sandbox-config FILE` overlays a JSON policy object; repeat it to load several
files in order. Objects merge recursively; arrays and scalar values replace the
previous value. Relative overlay filenames use the launch directory.
`--sandbox-set KEY=VALUE` replaces any policy field, such as
`--sandbox-set 'fs.writable=["${workspace}","/scratch"]'` or
`--sandbox-set process_isolation=full`. `--sandbox-add KEY=VALUE` appends to a
list, accepting one value or a JSON array of values. Values can be JSON or bare
strings; keys use dotted field names. Both options are repeatable.

Precedence is the inherited preset, overlay files, the network flag, replacement
settings in order, then additive settings. Overrides require an active preset
and use the same validation as preset files.

Each thread saves its preset name and effective settings in its existing session
record, including CLI additions. Resume and recovery use these saved settings
before workspace defaults, even if the preset file has changed or been removed. Extra CLI
arguments modify the saved settings and become the new saved settings. Selecting
a different `--sandbox-preset` starts from that preset and replaces previous
additions; selecting the same preset retains them. Saved `off` also stays off.
New and older sessions without saved settings use the configured default on
their first activation. Subagents inherit the root's settings; forks retain the
source tree's last settings. Path variables resolve when the runner starts.
If another process changes a thread's saved sandbox while a server retains its
old jail, that server refuses further execution for the thread with a restart
message. Restart the server to load the saved settings; an existing jail never
silently continues with stale grants.

Sandboxed sessions require Linux 6.12 or later, Landlock ABI 6, and unprivileged
user/mount namespaces. Network mediation additionally requires `/dev/net/tun`.
Enforcement failures refuse startup with exit 78. Approvals cannot widen a jail.
A running jail's policy stays frozen and is inherited by its subagents; a failed runner is never
restarted and commands are never replayed. Caller-supplied session IDs used for
sandbox storage must be a single path component. Reused scratch files are
checked before writing; symlinks and hard links refuse startup. Tool output
rewritten by hooks is saved within the session's readable spill directory.
Resumed and recovered work initializes the sandbox before running tools. A child
resumed before its parent freezes the root policy for the whole restored tree.
V2 directory moves keep that policy: tools use the new directory, and a move
outside the runner's readable filesystem is refused. Moving never adds grants.
With private `/tmp`, restored host folders retain their configured write access;
read-only folders stay read-only even below the writable temporary filesystem.

`workspace-write` and `read-all-net` provide mediated networking and publish development servers
on a per-runner `127.80.x.x` address. Preview URLs are printed as listeners appear.
Inbound firewall rules remain authoritative. Localhost-only IPv4 servers work;
servers bound exclusively to IPv6 `::1` are not published.

`unix_sockets` accepts `off`, `abstract`, or `deny`. Independently, `io_uring: false`
disables io_uring. With io_uring enabled, `IORING_OP_SOCKET` can bypass the seccomp
socket-family gate, including UNIX and VSOCK restrictions. TCP buffers use the
unmodified upstream smoltcp API with demand-paged backing and fixed configured
window capacity; smoltcp is not vendored. See [the design](docs/sandbox-plan.md)
for the policy schema and accepted compatibility limitations.

Build with `cargo build --release --no-default-features` to omit the sandbox crate.
Requesting an active sandbox in that binary is refused, including requests for
other workspaces served by the same process. See the
[readiness review](docs/sandbox-validation.md) for findings and verification.

## Workspace

The `ocmini` binary combines these library crates without changing the runtime's
worker model:

- `ocmini-core`: LLM lifecycle, sessions, storage, providers, hooks, permissions, and application adapters.
- `ocmini-tools`: all built-in tools, their schemas, transactions, output handling, and tool hooks.
- `ocmini-sandbox`: Linux enforcement, runner protocol, network policy, DNS/SNI mediation, and previews.
- `ocmini-tui`: interactive terminal and one-shot terminal frontends.
- `ocmini-server`: HTTP/SSE and OpenCode API routes.
- `ocmini-web`: the embedded browser interface and its file routes, merged into the server behind the default `web` feature.

Run workspace checks with `cargo test --workspace --all-targets` and
`cargo clippy --workspace --all-targets --all-features -- -D warnings`.

History storage uses a compile-time `HistoryBackend` contract in `ocmini-core`.
`Store<B>` dispatches directly to its backend type; `SelectedBackend` selects
SQLite for the application. There is no runtime backend selector. Core owns
fork remapping, recovery transformations, compaction policy and hook/turn
orchestration. The backend supplies queries and atomic commits across history,
admission receipts, session state and hook delivery. SQLite retains its existing
database, migrations, WAL connections and paging behavior. Only SQLite ships
today; another implementation can use the same core policies and select its type
at build time.
