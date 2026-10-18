# ocmini compatibility contract

ocmini implements the OpenCode 1.x HTTP/SSE API. Its compatibility version is `1.19.3-ocmini.0.2.1`; the package version is separate. `GET /api/info` returns 404 because the 2.x protocol is not supported. `GET /global/health` returns `healthy: true` and the compatibility version.

This document specifies observable behavior. Integration tests are grouped under `tests/contract/`, with unit tests beside their subsystems.

## Process and CLI contract

T3 launches:

```text
<binary> serve --hostname=127.0.0.1 --port=<chosen-port>
```

After a successful bind, stdout contains one flushed line:

```text
opencode server listening on http://<bound-address>:<actual-port>
```

Port zero chooses an available port and the banner reports the actual port. IPv6 literals are bracketed. Diagnostics go to stderr. Both `--option=value` and `--option value` are supported. Invalid options or port values exit unsuccessfully instead of silently using defaults. No arguments start terminal chat; `--help` displays command help. Terminal chat accepts `--model provider/model`, `--directory DIR`, `--continue` (latest top-level session in that project), `--session ID` (saved directory and history) and `--resume` (interactive selector: live search over title/ID/directory, cycling sort by updated/created/title, keyboard navigation; requires a terminal and fails cleanly without one). These flags also work with `tui`. `Ctrl+R` opens the same selector inside terminal chat to switch sessions. Resuming restores the last user model, agent and variant; an explicit model overrides the saved selection. `session list` prints saved IDs, titles and directories, with an optional `--directory DIR` filter.

T3's other probes are:

| Command | Required output |
| --- | --- |
| `--version` | A parseable 1.x compatibility version |
| `models --verbose` | A bare `provider/model` line followed by one JSON model object for each model |
| `agent list` | A `name (mode)` line followed by a JSON permission-rule array for each agent |
| `debug skill` | One JSON array of discovered skills, including their absolute `location` |

The model and agent CLI formats are **not** JSON arrays. Model IDs containing additional slashes are preserved after the first provider separator. Model data includes configured variants and limits; it is not reduced to names alone.

T3 supplies `OPENCODE_CONFIG_CONTENT` and, when applicable, `OPENCODE_SERVER_PASSWORD`. An empty `{}` config overlay must retain the user's file configuration. T3 does not need ocmini to rewrite OpenCode's config or credential files.

## HTTP transport and directory scope

If `OPENCODE_SERVER_PASSWORD` is nonempty, every route, including probes and unknown routes, requires HTTP Basic authentication for user `opencode`. The password is UTF-8; wrong or missing credentials return 401. Credentials are not written to access logs.

Requests are scoped by `directory` query parameter or `x-opencode-directory` header. The SDK may percent-encode the entire header. Decode it once, preserve Unicode, normalize path components and use the same scope for session access, lists, catalogs, pending requests, events and MCP registrations. The query parameter takes precedence. Requests without a scope use the process working directory for creation/catalogs; direct ID reads remain available without an explicit scope. A sibling path is not a prefix match for another project.

Failed lookups return 404. Invalid bodies or invalid identifiers return a client error. Storage/provider failures must not be converted into successful empty responses. JSON errors carry `name` and `data.message` so T3 can display a useful error.

The JSON request-body limit defaults to 128 MiB and can be set with `OCMINI_MAX_REQUEST_BYTES`. This bounds an individual HTTP body, not the combined size of local attachment files. T3 sends file URLs, so multiple native attachments do not become large inline JSON bodies. Native local attachments are limited to 20 MiB per file.

## Session and prompt operations

| HTTP operation | Behavior used by T3 |
| --- | --- |
| `POST /session` | Create a durable session with optional `title`, `parentID` and ordered `permission` rules; return the session |
| `GET /session/{id}` | Return session metadata for resume and snapshots |
| `PATCH /session/{id}` | Update title and permissions; permission changes apply to subsequent tool decisions |
| `DELETE /session/{id}` | Delete session data and return JSON `true`; an active session must be stopped first |
| `GET /session/{id}/children` | Return actual task child sessions |
| `GET /session/status` | Return object statuses for active sessions, such as `{sessionID: {type: "busy"}}` or a retry object with attempt/message/next; omit idle sessions |
| `GET /session/{id}/message` | Return chronological `{info, parts}` records; stable order is required even for matching timestamps |
| `POST /session/{id}/prompt_async` | Validate and durably admit the prompt before returning 204; model work continues through SSE |
| `POST /session/{id}/message` | Run a prompt and return its completed `{info, parts}` result; T3 reads text parts and `info.error` |
| `POST /session/{id}/command` | Expand and execute a configured command; return the completed result, with prompt admission visible on SSE |
| `POST /session/{id}/abort` | Cancel active/queued work and unblock pending requests; return JSON `true` |
| `POST /session/{id}/summarize` | Run real model summarization, persist the summary and compaction boundary, emit `session.compacted`; return JSON `true` after completion |
| `POST /session/{id}/fork` | Copy retained history into an independent session; optional `messageID` is the first **excluded** message |

The implementation additionally provides `GET /session`, `GET /session/{id}/message/{messageID}`, pending-request lists, and MCP connect/disconnect operations for diagnostics and management.

A prompt body uses a nested model reference:

```json
{
  "messageID": "msg_client_generated_id",
  "model": { "providerID": "provider", "modelID": "model" },
  "agent": "build",
  "variant": "high",
  "system": "T3 runtime and orchestration instructions",
  "parts": [
    { "type": "text", "text": "User request" },
    { "type": "file", "mime": "text/plain", "filename": "notes.txt", "url": "file:///workspace/notes.txt" }
  ]
}
```

The full request must reach the engine. Preserve the client `messageID`, system instructions, agent, variant, individual part data, optional `noReply`, and optional tool overrides. Omitted model selections resolve from session/agent/config defaults or fail clearly if none can be resolved.

T3 turns and command admission are correlated using the client-supplied user message ID. Regenerating that ID breaks admission, usage ownership and rewind boundaries. A command's user `message.updated` is its receipt; T3 allows ten seconds for it even though generation may continue longer.

Repeating the same user `messageID` within a session is idempotent: attach to the admitted execution or return its recorded result without a second provider call. An ID owned by another session or by an assistant message is rejected. `noReply` persists user context and returns the user record without starting model work.

### Steering and interruption

T3 sends another `prompt_async` while the session is busy to steer an active turn. Admit and persist that message without starting a second concurrent loop for the same session. Drain queued messages before emitting terminal idle. Do not silently drop a prompt, overwrite a cancellation handle, or acknowledge work that was not admitted.

Cancellation must interrupt a stalled provider stream, a subprocess and a permission/question wait. It cannot depend on the provider sending another token. T3 treats `MessageAbortedError` as interruption rather than failure. A root abort and child cleanup are separate operations in the T3 adapter; child sessions remain discoverable through `/children`.

Agent turns have no implicit step budget: continue model/tool rounds until completion, steering, cancellation or an execution error. An explicitly configured `agent.<name>.steps` (legacy `maxSteps`) limits the number of rounds; exhausting it preserves completed tool results for the next prompt.

Automatic compaction may repeat within a turn and within an overflowing model step while the checkpoint reduces the request. A failed compaction or non-shrinking overflowing request terminates recovery; there is no fixed compaction count. `compaction.auto` defaults to true and controls proactive and overflow recovery; manual summaries remain available when false. Headroom accounts for the model's input/output limits. `compaction.reserved`, `preserve_recent_tokens`, `tail_turns` and opt-in `prune` control the reserve, verbatim tail and old-tool context pruning. Pruning marks parts as compacted without deleting their stored results. Zero token/turn budgets disable the verbatim tail. Transient provider failures permit five retries per model step.

Shell and MCP operations honor larger explicit positive timeouts. Web fetches use a 30-second default and clamp requested timeouts to 120 seconds, with a 5 MiB response limit. All remain cancellable. `grep` and `glob` require `rg` on PATH and stream at most 100 results with ripgrep's native glob/ignore rules, without a candidate-file ceiling. Text reads have no whole-file size ceiling: pages default to 2,000 lines, allow larger explicit limits, and retain at most 2,000 characters per line and 50 KiB of text. Existing media limits remain.

### Commands

T3 sends `model` as a **string** on command requests, for example `provider/model`, plus `command`, `arguments`, `messageID`, optional `agent` and `variant`, and file `parts`. It does not send top-level `providerID` and `modelID` on this route.

Commands come from configuration and Markdown command files. Argument expansion supports:

- `$ARGUMENTS` receives the literal complete argument string.
- `$1`, `$2`, etc. use quoted argument grouping. The largest numbered placeholder receives the remaining arguments joined with spaces.
- Arguments are appended after a blank line when a template has no placeholders.
- Shell markers of the form `!` followed by a backtick-delimited command are evaluated by the engine in the session directory, using the same tool permission and cancellation machinery as bash.

Argument substitution must not interpret replacement metacharacters in values such as `$&`, `$$` or `$'`.

### Fork and rewind

T3's 1.x adapter implements rewind using `/fork`, not `/revert`. It finds the first removed message and verifies that the fork contains exactly the number of records preceding that boundary. Including the boundary is an error. A full fork without a boundary copies all retained history.

Every copied message and part needs a new ID, the fork's `sessionID`, and remapped internal message references, including assistant `parentID`. The original session remains unchanged. A fork is an independent conversation; it is not a running task child and does not acquire `parentID` merely because it was forked. Filesystem checkpoint restoration belongs to T3's checkpoint service.

## Message and SSE contract

`GET /event` returns `text/event-stream` with complete JSON values in `data:` frames. The first event is `server.connected`; periodic `server.heartbeat` events keep idle connections alive. There is no `/api/event` wrapper in the 1.x protocol.

Events are scoped to the requested directory. A subscriber that falls behind must receive a visible error and reconnect, rather than silently omit data and later report a successful turn. Persisted message history remains available for resumption; SSE does not advertise replay semantics it cannot provide.

| Event type | Properties required by T3 |
| --- | --- |
| `message.updated` | `sessionID`, `info` |
| `message.part.updated` | `part`, including its `id`, `sessionID`, `messageID` and discriminated `type` |
| `message.part.delta` | `sessionID`, `messageID`, `partID`, `field: "text"`, `delta` |
| `session.status` | `sessionID`, `status: {type: "busy"}` or `{type: "idle"}`; retry status is also an object |
| `session.idle` | `sessionID` |
| `session.error` | `sessionID`, `error: {name, data: {message}}` |
| `session.created`, `session.updated`, `session.deleted` | `info` session metadata |
| `session.compacted` | `sessionID` |
| `todo.updated` | `sessionID`, `todos` |
| `permission.asked`, `question.asked` | The corresponding pending request object |
| `permission.replied` | `sessionID`, `requestID`, `reply` |
| `question.replied` | `sessionID`, `requestID`, `answers` |
| `question.rejected` | `sessionID`, `requestID` |

The event and status-map values are objects, never the strings `"busy"` or `"idle"` alone.

### Ordering and live text

Persist and emit the user message before its parts. Emit assistant message metadata before its parts. Emit an initial empty text/reasoning `message.part.updated` before any delta for that part: T3 discards deltas for unknown part IDs. Subsequent delta events append text; full updates replace it. Final persisted parts must match the assembled text.

Tool parts progress through pending/running/completed/error states and retain their `callID`, input, timestamps and relevant metadata. For completed bash tools, `metadata.exit` supplies T3's exit-code display. For file changes, `metadata.diff` or `metadata.patch` supplies the change preview. Error states carry the actual failure and produce a tool result visible to the next model request.

Each assistant message records `parentID` equal to the owning user prompt ID. Each model step emits a `step-finish` part with `reason`, `cost` and `tokens`. T3 attributes usage using the assistant's parent ID and step parts; assistant token totals alone are insufficient. Finish the message with `time.completed` and its provider finish reason before the session becomes idle.

Malformed or prematurely terminated provider streams produce a recorded assistant error and `session.error`, followed by idle. Partial text must not turn into a successful completion solely because the transport closed.

Token shape:

```json
{
  "input": 0,
  "output": 0,
  "reasoning": 0,
  "cache": { "read": 0, "write": 0 }
}
```

Counts must come from provider responses with explicit handling of cached and reasoning tokens. Unknown usage must not be represented as invented measured usage.

## Permissions, questions and child sessions

Permission rules are ordered `{permission, pattern, action}` records. The last matching rule wins, including a later allow overriding a broader deny. T3 supplies full-access, supervised and auto-accept-edits rules on session creation and may update child-session rules later. Read-only agent restrictions remain enforced by the runtime. File edits use the `edit` permission family, and operations outside the workspace also require `external_directory` permission.

Permission request shape:

```json
{
  "id": "per_request",
  "sessionID": "ses_session",
  "permission": "bash",
  "patterns": ["the shell command"],
  "always": ["an approved pattern"],
  "metadata": {},
  "tool": { "messageID": "msg_assistant", "callID": "call_tool" }
}
```

`tool` is an object, not a tool-name string. `POST /permission/{requestID}/reply` accepts `{reply: "once" | "always" | "reject"}` and returns JSON `true`. The legacy session-scoped route accepts the response spelling and checks that the request belongs to that session. Invalid or already-resolved requests do not succeed.

Question requests use a flat `questions` array of `{question, header, options, multiple?, custom?}` objects. `POST /question/{requestID}/reply` accepts `{answers: string[][]}` with one answer array per question; `/reject` cancels it. Replies and cancellation remove the pending entry and emit the matching settlement event. Aborting a session also settles its outstanding requests.

Real task tools create child sessions and publish `session.created` with `parentID` before dependent child events. Their running and completed tool metadata contains `sessionId` and, when known, `model: {providerID, modelID}`. T3 uses these fields to create child threads, route child approval requests, update child permissions and discover background work. Writing a fabricated task-result string without running a child is not compatible behavior.

`subagent_depth` configures the nesting ceiling, defaulting to 8. Zero prevents child-task creation; larger values permit deeper nesting. Invalid or cyclic ancestor records produce tool errors.

## Models, agents, commands, skills and MCP

`GET /provider` returns `{all, connected, default}`. Every advertised model has a nonempty `id` and `name`; configured variants, limits and capability metadata are preserved. `connected` reflects usable configuration/credentials, not every known provider name. T3 builds its model picker only from connected providers. Unsupported provider SDKs fail explicitly rather than being sent through an unrelated API format.

`GET /agent` returns agent objects with `name`, `mode`, `permission`, `options`, and relevant description/prompt/hidden metadata. Built-in and configured agents use the same catalog as CLI probes. Disabled agents are omitted. T3 exposes primary/all agents and hides subagent-only or hidden entries.

`GET /command` returns configured names, descriptions, `source`, templates and placeholder `hints`. `GET /skill` returns discovered skills with nonempty `name` and absolute `location`; T3 drops skills lacking either field. Read configured and project/global OpenCode skill directories, and preserve the actual skill content for the engine.

T3 registers its tool bridge through:

```json
{
  "name": "t3-code",
  "config": {
    "type": "remote",
    "url": "http://t3-server/mcp-endpoint",
    "headers": { "Authorization": "the supplied credential" },
    "oauth": false
  }
}
```

MCP registration must complete its initialization and tool discovery, propagate supplied headers, and expose the discovered tool definitions to subsequent model requests. `GET /mcp` and the add result report real connection status. A dead endpoint never becomes `connected` merely because it was registered, and an explicit reconnect returns an error if the handshake fails. Remote JSON/streamable-HTTP and local stdio peers require request/response ID correlation, timeouts, cancellation, and error handling; a peer's `isError` is a tool error, not successful output. Directory-scoped registrations must not overwrite another directory's tool bridge.

MCP initialization is lazy per normalized request/session directory. Starting the runtime, browsing metadata, selecting a session, or admitting a `noReply` message must not launch configured MCP servers. The first MCP operation or model turn initializes that directory's hub once, including under concurrent requests. Local servers default to that directory as their working directory; an explicit configured `cwd` still takes precedence. Opening multiple directories, including subfolders or Git worktrees of one repository, keeps separate hubs rather than sharing them by repository identity.

## Storage and deliberate exclusions

Session state and transcripts live in the ocmini SQLite database under the XDG data directory. Writes that admit work or fork a transcript are transactional. Message/part ordering is stable. Deleting a session removes dependent data. Restart recovery distinguishes admitted work from interrupted execution; it must not silently rerun already-started shell mutations. There is no automatic 30-day transcript deletion: T3 resumes provider sessions by their native IDs and needs their history.

For T3's usage widget, a separate `opencode-ocmini.db` sidecar under the OpenCode XDG data directory contains a compatible `message` table with `id`, `session_id`, `data`, and `time_created`. Do not overwrite the real `opencode.db` or duplicate billed usage when copying history during a fork.

The headless runtime intentionally does not include OpenCode's desktop/web/TUI applications, hosted account/billing services, plugin marketplace, automatic plugin/package installation, updater, LSP fleet or OpenCode 2.x background-service protocol. Authentication mechanisms and provider formats that require those omitted integrations must fail honestly; the compatibility version is not a promise of support for every OpenCode distribution feature.

Full tool outputs saved separately from transcript previews are retained by default. `OCMINI_TOOL_OUTPUT_RETENTION_DAYS` enables optional startup expiry; `0` disables it. Transcript pruning remains separately controlled by `OCMINI_RETENTION_DAYS`.
