# Hooks and events reference

Start with [hooks guide](hooks.md) for common tasks. This document contains the complete contracts, advanced options, and delivery guarantees.

All names and settings below belong to the existing `ocmini.hooks` interface. The four families in the guide are navigation categories, not additional configuration keys or execution modes. `ocmini debug hooks --json` is the executable catalog: it shows schemas, writable fields, registrations, provenance, effective budgets, and read-only delivery metadata without executing scripts.

## Hook catalog

Hooks run in registration order. A boundary defines the fields it accepts, the changes it permits, and whether it can reject an operation. Every accepted transformation is validated before the next handler uses it. Runtime-owned identity, permissions, measured usage, and execution outcomes remain authoritative.

| Hook | Contract |
| --- | --- |
| `turn.start.before` | Once when queued execution starts, before command shell interpolation or a provider call; add turn-local context or reject execution. Cannot rewrite identity or stored user input. |
| `turn.end.before` | On natural completion only; accept, reject completion as an error, or return feedback requesting another step under the same prompt. |
| `turn.end.after` | Read-only finalizer after a durable outcome, before waiters/idle; receives success, steering, cancellation, limits, or failure. Cannot reopen the turn. |
| `tool.execute.before` | Before permission evaluation and execution; replace validated arguments or reject. Tool name and call identity remain fixed. Includes built-ins, MCP calls, and command shell interpolation. |
| `tool.execute.after` | Before publishing the terminal result; transform successful output/title and permitted metadata, then reapply output bounds and regenerate spill metadata. Receives failures read-only; cannot turn an executed failure into success or undo effects. |
| `tool.catalog.transform` | Filter allowed tools or edit descriptions; cannot expose a forbidden tool, add an implementation, or change its executable input schema. |
| `shell.env` | Add/remove allowed environment entries for a shell invocation, including command interpolation and background launch. Never changes the runtime's global environment. |
| `permission.decide` | Inspect each normalized permission decision; keep or tighten allow → ask → deny. Cannot override a deny or answer a human approval. |
| `question.ask.before` | Edit question wording/options before the pending request is registered, or reject it. Revalidate shape; cannot fabricate the user's answer. |
| `model.context.transform` | Change public system text and allowed request-local message content before a logical model step. Preserve IDs, tool-call/result linkage, and opaque provider replay data. |
| `model.params.transform` | Set supported generation parameters, variant, and provider options within model limits; immutable provider/model selection. |
| `model.headers.transform` | Add permitted non-authentication request headers before transport. No credential exposure or replacement of runtime auth. |
| `model.retry.before` | Veto a retry or adjust its delay within runtime bounds. Cannot exceed the existing attempt budget or retry cancellation and non-retryable failures. |
| `message.text.complete` | Transform one completed text part before its final full update. Text already streamed remains visible until that replacement. |
| `compaction.before` | Add context or replace the summary instructions before summarization. Cannot disable overflow checks or rewrite the retained-history boundary. |
| `compaction.after` | Transform a successful summary before its checkpoint commits. Revalidate nonempty content and shrinking-window rules; cannot report a failed compaction as success. |
| `prompt.accept.before` | Transform validated public prompt parts or reject before durable admission; revalidate after every handler. IDs, directory, and permission authority stay immutable. Includes `noReply` inputs. |
| `command.execute.before` | Transform command arguments or reject before pure template preparation and admission. Shell interpolation still waits until after the receipt is durable. |
| `command.expand.after` | Transform the prepared prompt after pure argument substitution and before admission. It does not rerun after shell interpolation. |
| `session.create.before` | Validate/change title and non-authority metadata, or reject creation; directory and parent ownership are fixed. |
| `session.update.before` | Validate/change explicitly requested title/non-authority metadata, or reject. Permission updates are visible read-only and still use normal API validation. Automatic accounting/title bookkeeping bypasses this gate. |
| `session.fork.before` | Inspect fork source/boundary and change the new title, or reject. Cannot include excluded history or alter source records. |
| `session.delete.before` | Reject deletion before it begins; active-work checks still apply. Cannot cancel work or partially delete history itself. |
| `task.spawn.before` | Validate/change child prompt, allowed agent/model, or title before creation. Recheck model access, ancestry, nesting, and permissions afterward. |
| `skill.load.after` | Transform loaded skill text before it enters model context. Skill name, source path, and provenance remain fixed. |
| `file.change.before` | Inspect or transform an entire proposed write/edit/patch transaction before commit. Recheck every effective path/content, permissions, and conflict conditions. |

## Event catalog

There are 49 event names. Existing OpenCode-compatible envelopes remain available. New lifecycle names use the `ocmini.` prefix. Events describe facts; observers cannot mutate or veto them. Connection events are transport-local, and token deltas require explicit batching for command observers.

| Event | Meaning |
| --- | --- |
| `server.connected` | An SSE subscriber connected; transport-local, not a runtime startup event |
| `server.heartbeat` | SSE keepalive; no command process per heartbeat |
| `session.created` | Persisted session info, including actual child sessions |
| `session.updated` | Persisted metadata/title/permission/usage update |
| `session.deleted` | Deleted session info; retain directory in the event for scope filtering |
| `session.status` | Busy, retry, or idle status object |
| `session.idle` | Compatibility idle event; use status for new integrations |
| `session.error` | User-visible execution failure with existing error shape |
| `session.compacted` | A summary and compaction checkpoint committed |
| `session.title` | Background title generation start/done |
| `message.updated` | Persisted user/assistant message metadata |
| `message.part.updated` | Full text/reasoning/tool/step part replacement |
| `message.part.delta` | Streaming text/reasoning append; SSE-only by default |
| `todo.updated` | Persisted todo state |
| `permission.asked` | Registered pending permission request |
| `permission.replied` | Permission request settled, including rejection/cancellation settlement |
| `question.asked` | Registered pending question |
| `question.replied` | User's validated answer |
| `question.rejected` | Question dismissed/cancelled |
| `ocmini.runtime.started` | Execution runtime ready; instance ID and runtime mode; diagnostic/probe commands do not execute hooks |
| `ocmini.runtime.stopping` | Graceful shutdown began; not guaranteed on a process kill |
| `ocmini.config.loaded` | Scoped execution config snapshot selected; fingerprint and source metadata, never resolved credentials |
| `ocmini.hook.started` | Invocation ID, handler ID, boundary, origin, and timing |
| `ocmini.hook.completed` | Successful invocation, duration, and decision summary; no raw stdout |
| `ocmini.hook.failed` | Exit/timeout/protocol/cancellation failure, duration, sanitized diagnostic |
| `ocmini.hook.queue.overflow` | An observer queue rejected work; counts and subscription ID; never re-enqueues itself |
| `ocmini.prompt.accepted` | Durable receipt; prompt ID and execution/context-only mode |
| `ocmini.turn.started` | A queued turn acquired execution; turn ID, kind, parent-session relationship |
| `ocmini.turn.end.requested` | A natural completion candidate reached the gate; end-attempt number and assistant ID |
| `ocmini.turn.continued` | Hook feedback committed for another step; origin, attempt, feedback reference |
| `ocmini.turn.ended` | Durable terminal outcome and bounded finalizers finished; reason, result reference, usage summary, finalizer diagnostics |
| `ocmini.tool.started` | Final validated/authorized call began; call ID, name, input reference |
| `ocmini.tool.completed` | Final successful result persisted; call ID and result reference |
| `ocmini.tool.failed` | Logical call failed/rejected/cancelled; stage and whether effects already ran |
| `ocmini.permission.decided` | Effective allow/ask/deny decision and reason after hook restrictions |
| `ocmini.model.started` | A provider attempt began; request ID, step, attempt, model, and request kind |
| `ocmini.model.completed` | A provider attempt completed; finish reason and measured usage |
| `ocmini.model.failed` | Provider attempt failed; normalized error and retry eligibility |
| `ocmini.model.retrying` | Retry was scheduled; attempt and delay after hook processing |
| `ocmini.compaction.started` | Manual/proactive/overflow summary attempt began |
| `ocmini.compaction.failed` | Summary generation/validation/checkpoint failed; old history remains authoritative |
| `ocmini.prompt.rejected` | Hook rejected otherwise valid admission; attempt ID and reason; no fabricated user receipt |
| `command.executed` | A command's prompt operation returned successfully; preserve upstream `name`, `sessionID`, `arguments`, and result `messageID`. Emit after execution settles rather than merely after expansion/admission |
| `ocmini.session.forked` | New fork committed; source ID, new ID, excluded boundary |
| `ocmini.task.started` | Child execution launched; parent/child session IDs, tool call, agent/model |
| `ocmini.task.ended` | Child task settled; IDs and terminal status/result reference |
| `ocmini.skill.loaded` | Skill text loaded/transformed; name, path, source fingerprint |
| `file.edited` | A tool-owned file change committed; preserve upstream `file` as the absolute path, with additive scoped operation/diff/result metadata |
| `ocmini.mcp.status` | Scoped MCP connecting/connected/disconnected/error transition; server name, no credentials |

## Registration and command protocol

Add `ocmini.hooks` to your global `~/.config/opencode/opencode.json` (or `$XDG_CONFIG_HOME/opencode/opencode.json`). Explicit launch configuration, including `OPENCODE_CONFIG_CONTENT`, also works.

```json
{
  "ocmini": {
    "hooks": {
      "version": 1,
      "handlers": [
        {
          "id": "verify-completion",
          "on": "turn.end.before",
          "command": ["python3", "/absolute/path/verify_completion.py"],
          "timeoutMs": 30000,
          "match": { "session": "root", "agent": ["build"] }
        }
      ],
      "maxEndContinuations": 3
    }
  }
}
```

Commands are argv arrays, launched directly with the session directory as the working directory. Use an explicit `bash -c` command if you need shell syntax. Input text is never interpolated into a command. Relative executable paths resolve against their configuration source; relative script arguments resolve against the session directory. Use absolute script paths for global registrations.

One command can register for several hooks with `"on": ["turn.start.before", "turn.end.before", "turn.end.after"]`; dispatch on the incoming `hook` field. Hook names are exact. To register all current hooks, use the names from `.catalog.hooks[].name` in the debug output. Observer `on` patterns separately support `"*"`. A shared registration retains the correct default timeout for each boundary: ordinary hooks 30s, finalizers 5s.

A command reads one versioned JSON object from stdin:

```json
{
  "version": 1,
  "hook": "turn.end.before",
  "invocationID": "hki_...",
  "operationID": "opaque-stable-operation-key",
  "context": {
    "directory": "/workspace/project",
    "sessionID": "ses_...",
    "turnID": "msg_user",
    "messageID": "msg_assistant",
    "turnKind": "primary",
    "requestKind": "primary",
    "agent": "build",
    "model": { "providerID": "provider", "modelID": "model" },
    "endAttempt": 1
  },
  "input": {
    "reason": "completed",
    "response": { "text": "Implemented the change.", "messageID": "msg_assistant" }
  }
}
```

Context fields vary by boundary. IDs, correlation, and runtime-owned authority are immutable. Handlers execute in registration order, with each accepted change visible to the next handler. Exit zero with empty stdout or `{}` means no change. Otherwise return one JSON object on stdout; send logs to stderr.

```json
{ "decision": "allow" }
```

```json
{ "decision": "block", "reason": "The proposed operation is disallowed." }
```

Only `turn.end.before` can request another model step:

```json
{ "decision": "continue", "feedback": "The requested test still fails. Fix it and rerun it." }
```

This continues the same owning prompt, retaining parent IDs and usage attribution. It does not enqueue a new user message. The continuation limit defaults to three; any nonnegative `maxEndContinuations` is accepted, and `null` removes that hook-specific limit. Configured agent step limits still apply. Steering and cancellation take priority and cancel a waiting completion gate. Manual compaction, failures, and cancelled/queued prompts do not run the natural-completion gate.

For example, `verify_completion.py` can run a real check and return feedback:

```python
import json
import subprocess
import sys

request = json.load(sys.stdin)
try:
    check = subprocess.run(
        ["cargo", "test", "--quiet"], capture_output=True, text=True, timeout=25
    )
    result = {"decision": "allow"} if check.returncode == 0 else {
        "decision": "continue",
        "feedback": "Tests failed:\n" + (check.stdout + check.stderr)[-8000:],
    }
except subprocess.TimeoutExpired:
    result = {"decision": "continue", "feedback": "The test check timed out; investigate it."}
print(json.dumps(result))
```

A failing command exit is a hook execution error; it does not implicitly request continuation. The script must return continuation JSON itself.

`failurePolicy` defaults to `"block"` for awaited handlers. Set it to `"continue"` for an optional helper whose startup failure, timeout, invalid response, or overflow should produce a diagnostic and retain the previous valid input. A deliberate `decision: "block"` is never ignored; malformed responses declaring a denial also stop. Cancellation always cancels. Continuing preserves normal validation and permissions; it does not authorize an operation. Finalizer and observer failures remain diagnostic regardless of this setting.

Mutation hooks use `set` for their documented writable fields. For `tool.execute.before`, replace the complete arguments object:

```json
{ "set": { "args": { "command": "cargo test", "timeout": 120000 } } }
```

The final arguments and any file/task-specific rewrites are validated and authorized again before execution. After-tool transformations cannot undo side effects or change runtime-owned exit codes, child IDs, attachments, or diffs. Transformed outputs are limited again and receive fresh spill-file metadata.

`turn.end.after` is a read-only, awaited finalizer. Its input includes `reason`, `started`, `responseID`, and `error`. It runs after the durable outcome and before synchronous waiters and idle. Its failure is diagnostic and does not change the completed outcome. Already streamed text remains visible during end-before validation; hooks do not provide buffered output or retroactive redaction.

## Large payloads and long checks

Existing scripts keep the inline v1 protocol. Set `payloadMode: "file"` to receive an exact snapshot through a private temporary JSON file:

```json
{
  "id": "large-completion-check",
  "on": "turn.end.before",
  "command": ["python3", "/absolute/path/check.py"],
  "payloadMode": "file",
  "timeoutMs": 600000,
  "queueTimeoutMs": 30000,
  "failurePolicy": "block"
}
```

The small stdin wrapper has `version: 2`, `invocationID`, and `payload: {path, bytes, contentType, lifetime}`. The referenced file contains the complete original v1 envelope, including `hook`, `operationID`, `context`, and `input`. Its lifetime is the invocation; it is a snapshot, not a live workspace file. On Unix, the directory is private and input file read-only. `OCMINI_HOOK_PAYLOAD_PATH`, `OCMINI_HOOK_RESPONSE_PATH`, and `OCMINI_HOOK_PAYLOAD_DIR` identify the provisioned paths.

```python
import json
import os
import sys

request = json.load(sys.stdin)
if request["version"] == 2:
    with open(request["payload"]["path"], encoding="utf-8") as payload:
        request = json.load(payload)

# A small response can still go directly to stdout.
response = {"version": 1, "decision": "allow"}

# For a large response, write the normal v1 response to the provisioned file.
path = os.environ["OCMINI_HOOK_RESPONSE_PATH"]
with open(path, "x", encoding="utf-8") as output:
    json.dump(response, output)
json.dump({"version": 2, "responseFile": path}, sys.stdout)
```

Only that response path is accepted, with no extra fields in the v2 control wrapper. The runtime reads a complete regular-file JSON response through a bounded reader; missing, oversized, invalid, or symlinked responses fail. No decision is inferred from a truncated prefix. Large replacement values still undergo ordinary field, file-conflict, and permission validation. File payloads default to 64 MiB per input/response through trusted `maxPayloadBytes`; this is separate from the small inline/control budgets. Temporary files are cleaned on completion, failure, and cancellation; Unix invocations also reclaim abandoned, unlocked payload directories with a bounded scan.

Execution time starts after capacity becomes available. `timeoutMs` accepts explicit long checks, including ten-minute checks; there is no two-minute ceiling. `queueTimeoutMs` separately bounds waiting for capacity. `chainTimeoutMs` and `finalizerChainTimeoutMs` default to `null`; set a positive value to impose an end-to-end chain deadline, including queue waiting. An explicit chain deadline can shorten a command's execution window, as shown by `effectiveBudgets` in debug output. Admission remains a separate 2s total default, configurable up to 5s to meet the ten-second command-receipt contract. Put expensive admission checks in `turn.start.before`, after the receipt is durable.

## Project registration and matching

Project `opencode.json` or `.opencode/opencode.json` registrations require trusted user/launch policy. In global configuration, allow selected canonical directories:

```json
{
  "ocmini": {
    "hooks": {
      "version": 1,
      "allowProjectHooks": ["/workspace/my-project"]
    }
  }
}
```

A project cannot grant itself this permission or erase/replace trusted global handlers. Global `allowProjectHooks: true` enables all project registrations. No executable directory is automatically scanned, and no package installation occurs. Commands run with your local OS access; the hook runner is not a sandbox.

Registration `match` supports `directory`, `session` (`root`/`child`), `agent`, `model` (`provider/model`), `turnKind`, `requestKind`, `tool`, and `outcome`. Values can be a string or an array of simple glob patterns. Generic model/text hooks default to primary requests; dedicated compaction hooks default to compaction. Turn hooks default to primary turns. Start/after can explicitly opt into manual compaction; end-before always excludes it. Resource hooks have no implicit request-kind filter. Explicitly match `requestKind: "title"` to customize detached title calls.

Registrations merge by ID in source order; trusted configuration can disable its registered handler with `enabled: false`. An admitted turn saves its effective registrations. Script-file contents are not copied or frozen. Later admissions pick up configuration changes after the existing configuration cache expires.

## Subscribe to events

An external application can use the existing directory-scoped SSE stream. No hook registration is needed:

```sh
curl -N --get 'http://127.0.0.1:4096/event' \
  --data-urlencode 'directory=/workspace/my-project' \
  --data-urlencode 'types=ocmini.turn.ended,file.edited'
```

Add HTTP Basic authentication (`-u 'opencode:your-server-password'`) when `OPENCODE_SERVER_PASSWORD` is configured. Omit `types` to receive all events. `types` is an exact comma-separated name filter; directory scoping still applies. Transport lag errors remain visible even if normal events are filtered. SSE envelopes retain the existing shape:

```json
{
  "id": "evt_...",
  "type": "ocmini.turn.ended",
  "properties": {
    "version": 1,
    "directory": "/workspace/my-project",
    "sessionID": "ses_...",
    "turnID": "msg_user",
    "reason": "completed",
    "started": true,
    "responseID": "msg_assistant"
  }
}
```

A turn ends independently of session idleness. `ocmini.turn.ended` reports one admitted prompt. `session.status` becoming `idle` means the queue drained. Follow status/error/outcome fields rather than treating idle as proof of success.

To run a local command when an event occurs, register an observer:

```json
{
  "ocmini": {
    "hooks": {
      "version": 1,
      "observers": [
        {
          "id": "completion-notification",
          "on": ["ocmini.turn.ended", "ocmini.task.ended"],
          "command": ["python3", "/absolute/path/on_event.py"],
          "timeoutMs": 5000,
          "match": { "session": "root" }
        }
      ]
    }
  }
}
```

Observers read a versioned envelope containing `subscriptionID` and an `events` array, even for a single event:

```python
import json
import sys

request = json.load(sys.stdin)
for event in request["events"]:
    print(event["type"], event["properties"].get("reason"), file=sys.stderr)
```

Observer output cannot mutate events or control the operation. Observers have separate bounded queues and process capacity. Full-part subscriptions default to terminal updates. Token deltas require explicit batching with both `batchSize` greater than one and positive `batchMs`; heartbeats do not spawn commands. Hook diagnostics require explicit event-name subscriptions and cannot observe their own invocations recursively.

Best-effort observers support `overflow: "dropNewest"` (default), `"dropOldest"`, or `"coalesce"`. Coalescing combines only replaceable session-status and nonterminal part updates; terminal facts and deltas are not coalesced. The next envelope reports omitted/coalesced deliveries in `gaps`/`dropped`; overflow diagnostics use `ocmini.hook.queue.overflow`. After a gap, retrieve current session/message state through the normal HTTP API. Set `payloadMode: "file"` to retain complete large events within the configured queue and payload budgets; inline observers keep the bounded marker/resource-ID representation.

## Durable terminal notifications

For a completion integration that must survive restart, opt into durable delivery for exact `ocmini.turn.ended` and/or `command.executed` names:

```json
{
  "ocmini": {
    "hooks": {
      "observers": [{
        "id": "completion-audit",
        "on": ["ocmini.turn.ended", "command.executed"],
        "delivery": "durable",
        "command": ["python3", "/absolute/path/audit.py"],
        "retryAttempts": 8,
        "retryDelayMs": 1000,
        "retentionMs": 86400000
      }]
    }
  }
}
```

Accepted outbox entries commit in the same SQLite transaction that finalizes the turn. Stable event IDs survive retries; workers use unique leases and bounded exponential backoff. `retryAttempts` is the maximum total attempts including the first dispatch. Successful acknowledgements remove entries; exhausted entries remain inspectable until expiry. Empty stdout or `{"ack": true}` acknowledges a successful command. As with other observers, nonzero exit or invalid acknowledgement is a failure. Consumers must deduplicate by event ID: external effects can repeat if a process crashes after the effect and before acknowledgement.

The outbox uses the subscription's count/byte queue budget, including exhausted entries, and a database-wide default budget of 4096 entries/64 MiB (`observerOutboxCount`/`observerOutboxBytes`). Full outboxes visibly reject new notifications without changing an already-settled turn. Retention expiry reclaims abandoned subscription namespaces too. Changing a registration changes its delivery key; old jobs do not run under a new command or disabled registration. Notifications need a running ocmini process with the matching directory configuration loaded. There is no automatically installed background service.

Only these two terminal events support durable delivery. SSE and ordinary observers stay best-effort. Recovery records interrupted turns using their saved subscription snapshots even when starting in another directory, preserving the settled response and never replaying tool effects. Database failures preserve pending recovery snapshots; outbox quota rejections remain diagnostic. `ocmini debug hooks --json` shows delivery state, attempts, lease and expiry metadata without exposing event bodies or modifying the database.

## Efficiency, limits, and recovery

- Disabled hooks spawn no processes or observer workers. Handler lookup is indexed by name; payloads are constructed only for registered boundaries. Directory registrations are cached with bounded retention. Hooks run per operation, not per token.
- Events share immutable `Arc` payloads. Observer queues count both entries and encoded bytes; unmatched events avoid encoding. Default queue limits are 128 events and 4 MiB per subscription, with a 128 KiB per-event budget.
- Normal hook command timeouts default to 30 seconds; observers/finalizers default to 5 seconds. Queue wait defaults to 30 seconds independently. Chain deadlines are opt-in; admission retains its short end-to-end deadline. Positive execution/chain settings have platform-representability checks rather than the former 120s ceiling.
- Defaults remain 1 MiB inline stdin, 256 KiB structured stdout, and a 64 KiB retained stderr tail. Required input and complete decisions are never silently truncated. Stderr drains continuously; excess logging does not kill the hook. Failed invocations can retain the tail in private diagnostic files (up to 32 files/4 MiB total); the caller and diagnostic event receive the path, not raw logs. Programs are terminated and reaped on timeout, cancellation, or graceful shutdown.
- `maxProcesses` defaults to eight per scope. Trusted `maxGlobalProcesses`/`maxGlobalObservers` default to 16/four and can be changed; the latest loaded trusted runtime policy resizes the shared pools while existing holders finish. Snapshots cannot resize them. Each pool has a bounded pending queue (`maxPendingHooks`, default 128); saturation is reported before serialization. Capacity remains reserved through response parsing. Idle observers wait on notifications rather than polling for shutdown.
- Outcomes and exact result references are durable. Duplicate prompt IDs do not repeat execution or finalizers; synchronous duplicates in another process wait for finalization to finish. While an abort settles queued turns, new admissions return a busy response and can be retried after idle. Queued, unstarted prompts can resume after restart; interrupted started turns do not replay potentially completed effects.
- External effects are not exactly once. Finalizers have at most one automatic dispatch attempt; a crash can leave a finalizer interrupted/skipped. Use `operationID` for finalizer idempotency and event IDs for durable-observer idempotency. Reliable delivery is opt-in and bounded by acceptance, attempt, and retention budgets.

## Settings by concern

These groups describe the existing flat configuration. They do not add nested objects, aliases, or new defaults. Start with `id`, `on`, and `command`; add other options when a specific workload requires them.

- Registration: `version`, `handlers`, `observers`, and trusted `allowProjectHooks`. Each registration has `id`, `on`, `command`, optional `enabled`, and optional `match`.
- Execution behavior: per-registration `timeoutMs`, `queueTimeoutMs`, and `failurePolicy`; trusted `maxEndContinuations`, `chainTimeoutMs`, `finalizerChainTimeoutMs`, `admissionChainTimeoutMs`, and default `queueTimeoutMs`.
- Payload transport: per-registration `payloadMode`, `maxInputBytes`, `maxOutputBytes`, and `maxStderrBytes`; trusted defaults with the same byte names and `maxPayloadBytes` for file transport.
- Runtime capacity: trusted `maxProcesses`, `maxGlobalProcesses`, `maxGlobalObservers`, and `maxPendingHooks`.
- Observer delivery: per-registration `delivery`, `overflow`, `queueCount`, `queueBytes`, `batchSize`, `batchMs`, and `terminalOnly`; trusted defaults `observerQueueCount`, `observerQueueBytes`, and `observerEventBytes`.
- Durable delivery: per-registration `retryAttempts`, `retryDelayMs`, and `retentionMs`; trusted database-wide `observerOutboxCount` and `observerOutboxBytes`.

Byte budgets are positive configurable sizes with platform checks; raising them raises potential resource use. These bound supervisor transport/storage work and do not limit a hook child's own CPU, memory, or filesystem access. Authentication stores, runtime auth headers, passwords, opaque reasoning, and full environment dumps are excluded from payloads. Source/prompt/tool text supplied to a selected local hook can still contain application data.
