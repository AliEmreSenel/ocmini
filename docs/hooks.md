# Hooks and events

ocmini extensions have two interfaces:

- **Hooks** run during an operation. Their boundary defines what they may validate, change, or reject. `turn.end.after` is an awaited, read-only finalizer.
- **Event subscriptions** receive facts about operations. External clients use SSE; local commands register as observers. Observers cannot change the operation.

All 26 hooks and 49 events are available. Start with the task below that matches your integration; detailed contracts and tuning live in [hook reference](hooks-reference.md). OpenCode JavaScript plugins use a separate interface.

For copyable recipes, see [hook examples](hooks-examples.md): run your own script on completion, show a desktop notification, require passing tests, add branch context, or adjust shell environments. These examples are documentation only.

## Run a command before or after a turn ends

Add this to global `~/.config/opencode/opencode.json` (or `$XDG_CONFIG_HOME/opencode/opencode.json`):

```json
{
  "ocmini": {
    "hooks": {
      "version": 1,
      "handlers": [{
        "id": "turn-check",
        "on": ["turn.end.before", "turn.end.after"],
        "command": ["python3", "/absolute/path/turn_hook.py"]
      }]
    }
  }
}
```

A registration needs `id`, `on`, and `command`. One command can serve several exact hook names by inspecting the incoming `hook` field. Commands launch directly from argv arrays in the session directory; use absolute script paths for global registrations.

For example, `turn_hook.py` can start with:

```python
import json
import sys

request = json.load(sys.stdin)
if request["hook"] == "turn.end.before":
    # Replace this with a real completion check.
    print(json.dumps({"decision": "allow"}))
elif request["hook"] == "turn.end.after":
    print("Turn ended:", request["input"]["reason"], file=sys.stderr)
```

Scripts read one JSON request from stdin, write a response object to stdout, and send diagnostics to stderr. Empty stdout on a successful exit means no change. Ordinary hooks default to 30 seconds; finalizers and observers default to 5 seconds. Set a registration's `timeoutMs` when its command needs longer.

At `turn.end.before`, return `decision: "allow"`, `decision: "block"` with a reason, or request another model step:

```json
{"decision":"continue","feedback":"The test still fails. Fix it and rerun it."}
```

Continuation stays under the same prompt. The default limit is three hook-requested continuations; trusted `maxEndContinuations` can change it or use `null` to remove that limit. Agent step limits still apply. A nonzero command exit is an execution failure, not a continuation request.

`turn.end.after` receives the settled outcome and cannot change it. It finishes before synchronous callers receive completion. Use it when cleanup is part of finishing the turn; use an event subscription for an asynchronous integration. Finalizer failures are diagnostic.

## Choose hooks by the operation you need

The catalog has four families. These are documentation groups; use the existing names directly in `on`.

- **Conversation:** `prompt.accept.before`, `command.execute.before`, `command.expand.after`, `turn.start.before`, `turn.end.before`, `turn.end.after`.
- **Tools:** `tool.catalog.transform`, `tool.execute.before`, `tool.execute.after`, `shell.env`, `permission.decide`, `question.ask.before`.
- **Models:** `model.context.transform`, `model.params.transform`, `model.headers.transform`, `model.retry.before`, `message.text.complete`, `compaction.before`, `compaction.after`.
- **Resources:** `session.create.before`, `session.update.before`, `session.fork.before`, `session.delete.before`, `task.spawn.before`, `skill.load.after`, `file.change.before`.

Each hook permits specific fields. Mutation responses use `set`; accepted changes are validated before the next handler receives them. Tool arguments and file paths are authorized after preparation. Hooks cannot broaden permissions or replace runtime-owned identity, measured usage, or execution results. See the [hook catalog](hooks-reference.md#hook-catalog) for each boundary's contract.

## Subscribe to facts about execution

External applications can use the existing directory-scoped SSE stream without registering a command:

```sh
curl -N --get 'http://127.0.0.1:4096/event' \
  --data-urlencode 'directory=/workspace/my-project' \
  --data-urlencode 'types=ocmini.turn.ended,file.edited'
```

Omit `types` for all events; supplied names are exact. Configure HTTP Basic authentication when the server uses `OPENCODE_SERVER_PASSWORD`.

To run a local command asynchronously, add an observer:

```json
{
  "ocmini": {
    "hooks": {
      "observers": [{
        "id": "completion-notification",
        "on": ["ocmini.turn.ended", "ocmini.task.ended"],
        "command": ["python3", "/absolute/path/on_event.py"]
      }]
    }
  }
}
```

Observers read an `events` array even for one event. Their output can acknowledge processing but cannot mutate events. Observer `on` patterns support `"*"`; hook names are exact. A turn-ended event describes one prompt; session idle means the queue drained and does not imply success.

Ordinary observers and SSE are best-effort. For restart-safe terminal notifications, an observer can opt into `delivery: "durable"` for exact `ocmini.turn.ended` and/or `command.executed` names. Consumers deduplicate by event ID. See [delivery and recovery](hooks-reference.md#durable-terminal-notifications) for acknowledgement, retry, quota, and retention behavior.

## Add options when your integration needs them

The defaults support the basic examples. Additional options address specific needs:

- `match`: restrict a registration to a directory, root/child session, agent, model, turn/request kind, tool, or outcome.
- `timeoutMs`: allow a longer execution. Queue waiting is measured separately; overall chain deadlines are optional.
- `failurePolicy: "continue"`: make an optional handler retain the previous valid input on execution/protocol failure. Explicit denials and cancellation still apply.
- `payloadMode: "file"`: exchange large exact input/response snapshots through temporary files.
- `delivery: "durable"`: retry supported terminal notifications after restart.

Explicit launch configuration, including `OPENCODE_CONFIG_CONTENT`, also works. Project registrations require global/launch `allowProjectHooks` policy; a project cannot authorize itself or replace trusted handlers. Commands run with local OS access.

An admitted turn saves its registrations. Duplicate prompt IDs do not repeat execution or finalizers. Queued work can resume after restart; interrupted started work does not replay effects. Cancellation takes priority over a completion check. Already streamed text remains visible during that check.

## Inspect and troubleshoot

```sh
ocmini debug hooks --json
```

Inspection validates configuration and shows the catalog, allowed fields, registrations, trust sources, effective budgets, and bounded delivery metadata. It does not execute commands or recover turns.

Use the [hook reference](hooks-reference.md) for protocol details, all event names, limits, and advanced delivery policy.
