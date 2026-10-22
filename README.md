# ocmini

A minimal OpenCode replica in Rust. One binary for terminal chat, coding tools, saved sessions and an OpenCode 1.x HTTP/SSE server.

## Quick start

Building requires Rust 1.85+ and a C compiler for bundled SQLite. Search tools require ripgrep (`rg`) on `PATH`.

```sh
cargo install --path . --locked
ocmini                                    # terminal chat
ocmini --model local/my-model --directory ~/project
ocmini serve --hostname 127.0.0.1 --port 4096
```

Terminal controls: Enter sends, bracketed paste inserts multiline text, Ctrl+V (or Ctrl+Shift+V when forwarded by the terminal) attaches a clipboard image, and pasting an image file path attaches it. Alt+Backspace removes the latest attachment; Backspace also removes it when the text is empty. Ctrl+U clears text and attachments. Ctrl+C aborts the current turn, Ctrl+D exits, Ctrl+R opens the session selector. The draft previews multiline text with `↵` and scrolls horizontally; the full text and line breaks are sent unchanged. Backspace removes whole Unicode characters and stays within the draft.

Questions appear one at a time with their numbered options. The answer line is the same full editor as the prompt - caret movement, selection, word motions, kill, yank, undo and multiline all work on the answer - and the prompt you were typing stays untouched underneath, returning when the question settles. A line of option numbers answers with those options; any other text is sent as a custom answer exactly as typed. Enter walks to the next question and wraps back to the first, and each answer is kept in the transcript. Tab and Shift+Tab walk the questions forward and back - each question keeps its own draft, so switching never loses a half-typed answer, and answered questions reopen prefilled so they can be changed. Backspace on an empty answer line also steps back, and Esc dismisses the request. When the last answer lands, a confirmation screen lists every question with its answer - Enter sends, Tab steps back to edit, Esc dismisses. Your prompt is never sent while a question is open.

Transcript scrolling is a config option, `ui.scroll_mode`:

- `native` (default): completed transcript rows commit to the terminal's own scrollback and only the prompt is redrawn. Wheel and terminal shortcuts scroll as usual; PageUp/PageDown page the draft.
- `fullscreen`: ocmini repaints the whole screen on the alternate screen and scrolls the transcript itself. PageUp/PageDown page, Ctrl+Home/Ctrl+End jump to the top and bottom, and the mouse wheel scrolls (Shift+wheel keeps the terminal's native selection scroll). The prompt header lists these keys. On exit the transcript is restored to the terminal's scrollback.

Set it in `opencode.json` before starting; a running TUI keeps the mode it started with:

```json
{ "ui": { "scroll_mode": "fullscreen" } }
```

Clipboard image paste needs `wl-paste` (Wayland), `xclip` (X11), `osascript` (macOS) or PowerShell (Windows). Many terminals handle Ctrl+Shift+V themselves: pasted text and image paths work, but a screenshot cannot reach the app if the terminal sends no paste event or image data. Use Ctrl+V for a direct clipboard read, or configure the terminal to forward Ctrl+Shift+V. PNG, JPEG, GIF and WebP images are supported. Quoted paths, shell-escaped spaces and percent-encoded `file://` URLs work too.

Over SSH, ordinary terminal paste carries text, not your local image clipboard. Copy the image to the remote host (for example with `scp`) and paste its **remote** path. X11 clipboard forwarding can also work when `DISPLAY` is forwarded and `xclip` is installed on the remote host. Clipboard images are stored with the message, so they remain available when resuming a session.

To use it with T3 Code, set the OpenCode provider's binary path to the installed `ocmini` executable.

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

`run` drives the same engine as the TUI: sessions, agents, models, tools, permissions and questions all behave the same. Text mode prints the agent/model header, tool lines and the streamed answer. `--thinking` also prints chain of thought. `--format json` emits one JSON object per line (`step_start`, `text`, `step_finish`, `tool_use`, and `reasoning` with `--thinking`) for scripts. Tool permissions follow one policy: `--auto` allows them, a TTY prompts for `[y]es/[a]lways/[n]o`, and a non-TTY run refuses them so the command never hangs. `--interactive` (`-i`) keeps prompting after the first answer (exit with `/exit` or Ctrl+D); `ocmini` without `run` is the full TUI. Flags: `--agent`, `--variant`, `--title`, `--dir`, `--command`, `--model`.

## What stays

- **Coding loop:** streaming replies and reasoning, file attachments, cancellation, steering, permissions and questions.
- **Tools:** `bash`, `read`, `write`, `edit`, `apply_patch`, `glob`, `grep`, `webfetch`, `todowrite`, `question`, `task` and `skill`.
- **Sessions:** SQLite history, resume, forks, child tasks, context compaction and recovery after interruption.
- **Providers:** OpenAI Chat Completions, OpenAI Responses and Anthropic Messages, including compatible endpoints.
- **Project setup:** OpenCode JSON/JSONC configuration, agents, commands, skills and `AGENTS.md` instructions.
- **MCP:** stdio and Streamable HTTP servers.
- **API:** OpenCode 1.x HTTP/SSE, model inventory and usage reporting.

Tool text over 50 KiB is saved under `$XDG_DATA_HOME/ocmini/tool-output` (default `~/.local/share/ocmini/tool-output`). The conversation keeps a preview of the beginning and end plus the saved path, so omitted content can be read or searched later. Large shell output is captured to disk with bounded memory. Existing file-read/search paging and media limits still apply. Full output files are retained by default so resumed sessions can still read them. Set `OCMINI_TOOL_OUTPUT_RETENTION_DAYS` to a positive integer to remove expired output files at startup; unset or `0` retains them. These files use disk space separately from session history.

A normal `bash` call ends its whole process group when the command exits, so `cmd &` and `nohup cmd &` do not survive it. For work that must outlive the call, pass `background: true`: the command starts in its own session, the tool returns its pid and log path at once, the job keeps running across tool calls, and `kill <pid>` stops it. A background job belongs to the session that started it: aborting the turn, deleting the session, or stopping ocmini ends it. Its log is not size-bounded and expires with `OCMINI_TOOL_OUTPUT_RETENTION_DAYS` like other output files. A command that reaches its `timeout` completes with the output it printed before the limit plus a `shell_metadata` notice, so a cancelled build still shows its work; aborting a turn discards the result.

Compaction can be requested manually, runs automatically with room reserved for model output, and recovers from provider context-overflow errors. It can repeat throughout a task, including within one model step while each checkpoint reduces the overflowing request. Recovery stops when compaction fails or no longer reduces that request. Subsequent requests use the latest valid summary, recent messages kept verbatim, and newer messages; the original history remains saved. The default recent-message budget is roughly a quarter of usable context, between 2,000 and 15,000 tokens. Complete tool call/result groups are retained when they fit, with the latest user message taking priority. Budgets use approximate token estimates. Unknown context windows rely on manual compaction or provider overflow recovery.

`grep` and `glob` stream ripgrep results, return up to 100 matches, and use ripgrep's ignore and glob rules. Explicit globs can override ignore rules. Search has no additional candidate-file ceiling. Text reads accept files of any size and default to 2,000 lines; `offset` and `limit` page through them without a separate 20,000-line ceiling. Each line is capped at 2,000 characters and each file page at 50 KiB of text. Reads validate UTF-8 and binary content while retaining a bounded line preview. Media attachment limits still apply.

## What gets cut

Desktop/web apps, hosted accounts and billing, plugin marketplaces and installers, auto-updates, telemetry, LSP management and the OpenCode 2.x service layer.

Provider support is limited to the three HTTP protocols above. Native Gemini, Vertex, Bedrock, Azure-specific authentication, arbitrary npm provider SDKs and WebSocket transports are excluded. Compatible HTTP endpoints can still be configured. MCP browser OAuth and legacy HTTP+SSE transport are also excluded.

## Configuration

Uses OpenCode-shaped `opencode.json` or `opencode.jsonc`. Global configuration lives in `$XDG_CONFIG_HOME/opencode`; project configuration lives in the project or `.opencode` directory. Environment overrides include `OPENCODE_CONFIG`, `OPENCODE_CONFIG_DIR` and `OPENCODE_CONFIG_CONTENT`.

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

Set `OPENCODE_SERVER_PASSWORD` to require Basic authentication with username `opencode`. The server defaults to `127.0.0.1:4096`; `--port 0` chooses a free port. `ocmini --help` lists commands. The reported version, `1.19.4-ocmini.0.3.4`, identifies API compatibility.

Async I/O runs on the main thread by default; file operations and history loading use a separate blocking pool as needed. Set `TOKIO_WORKER_THREADS` to a positive integer to opt into a multithreaded async runtime for heavier concurrent workloads. SQLite checkpoints allow oversized journal files to shrink to 1 MiB on reuse; active transactions can temporarily exceed this size.

## Development

```sh
cargo fmt --all -- --check
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo test --all-targets --locked
cargo build --release --locked
```

Tests use temporary state and local provider/MCP fixtures; no paid credentials are needed. Unit tests stay inline beside the code they cover. Integration tests are grouped by behavior under `tests/contract/`.

See [SPEC-CONTRACT.md](SPEC-CONTRACT.md) for API details and [VALIDATION.md](VALIDATION.md) for checks and coverage.
