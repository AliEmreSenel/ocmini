# Hook recipes

These examples are documentation only. Copy a recipe's Python code to the path you choose, then add its registration to global `~/.config/opencode/opencode.json` (or `$XDG_CONFIG_HOME/opencode/opencode.json`). Replace `/absolute/path/...` with your paths. Nothing below is installed or registered automatically.

Each JSON block is a standalone configuration. To combine recipes, put their entries in the same `handlers` or `observers` array. Commands run in the session's directory, read JSON from stdin, write protocol responses to stdout, and send logs to stderr. These examples use the default inline payload protocol.

## Run your own script when the AI finishes

Use `turn.end.after` when a script should run before the completed turn is returned to its caller. This example runs only for successfully completed primary turns in root sessions, so child tasks and cancelled/failed turns do not trigger it.

```json
{
  "ocmini": {
    "hooks": {
      "handlers": [{
        "id": "run-after-ai",
        "on": "turn.end.after",
        "command": ["python3", "/absolute/path/after_ai.py"],
        "timeoutMs": 30000,
        "match": {"session": "root", "outcome": "completed"}
      }]
    }
  }
}
```

For `after_ai.py`, wrap an existing executable script:

```python
import json
import subprocess
import sys

request = json.load(sys.stdin)
subprocess.run(
    [
        "/absolute/path/on_ai_done.sh",
        request["context"]["sessionID"],
        request["operationID"],
    ],
    check=True,
    timeout=20,
    stdout=sys.stderr,
)
```

Your script receives the session ID and a stable operation ID as arguments. It can use those to locate the session or deduplicate its own effects. Its stdout is redirected to stderr because this read-only hook must not return a decision or mutation. A failure is reported as a finalizer diagnostic and does not change the already-settled turn outcome. Finalizers are not automatically replayed after a crash.

## Show a desktop notification asynchronously

Use an observer when notification work should run independently of the caller's completion. This Linux example requires `notify-send` on `PATH`.

```json
{
  "ocmini": {
    "hooks": {
      "observers": [{
        "id": "desktop-completion",
        "on": "ocmini.turn.ended",
        "command": ["python3", "/absolute/path/notify_ai.py"],
        "match": {
          "session": "root",
          "turnKind": "primary",
          "outcome": "completed"
        }
      }]
    }
  }
}
```

For `notify_ai.py`:

```python
import json
from pathlib import Path
import subprocess
import sys

request = json.load(sys.stdin)
project = Path(request["context"]["directory"]).name
for event in request["events"]:
    session = event["properties"]["sessionID"]
    subprocess.run(
        ["notify-send", "ocmini finished", f"{project}: {session}"],
        check=True,
        timeout=3,
        stdout=sys.stderr,
    )
```

The observer receives an `events` array, even for a single delivery. Empty stdout acknowledges successful processing. This registration uses best-effort delivery; for notifications that need retry/restart support, exact `ocmini.turn.ended` subscriptions can add `delivery: "durable"`. Durable consumers must deduplicate by event ID because redelivery can repeat external effects. See the [delivery contract](hooks-reference.md#durable-terminal-notifications).

## Keep working until the tests pass

A completion gate can ask the AI to fix a failing check under the same prompt. This example runs `cargo test --quiet`; replace that command with your project's test or lint command.

```json
{
  "ocmini": {
    "hooks": {
      "handlers": [{
        "id": "tests-before-done",
        "on": "turn.end.before",
        "command": ["python3", "/absolute/path/check_tests.py"],
        "timeoutMs": 65000,
        "match": {"session": "root"}
      }],
      "maxEndContinuations": 3
    }
  }
}
```

For `check_tests.py`:

```python
import json
import subprocess
import sys

json.load(sys.stdin)
try:
    check = subprocess.run(
        ["cargo", "test", "--quiet"],
        capture_output=True,
        text=True,
        timeout=60,
    )
    if check.returncode == 0:
        response = {"decision": "allow"}
    else:
        output = (check.stdout + check.stderr)[-6000:]
        response = {
            "decision": "continue",
            "feedback": "Tests failed. Fix them and rerun the checks:\n" + output,
        }
except subprocess.TimeoutExpired:
    response = {
        "decision": "continue",
        "feedback": "The test check exceeded 60 seconds. Investigate before finishing.",
    }
print(json.dumps(response))
```

The wrapper exits successfully and returns continuation JSON when tests fail; a failing wrapper exit alone would be a hook execution error. The default three-continuation budget prevents a failed check from creating an endless completion loop. Steering, cancellation, and configured agent step limits still apply. Already-streamed text remains visible during validation.

## Tell the AI which branch it is working on

Use `turn.start.before` to add fresh turn-local context without rewriting the saved user prompt. This recipe preserves context produced by earlier handlers and adds the current Git branch.

```json
{
  "ocmini": {
    "hooks": {
      "handlers": [{
        "id": "branch-context",
        "on": "turn.start.before",
        "command": ["python3", "/absolute/path/branch_context.py"],
        "failurePolicy": "continue"
      }]
    }
  }
}
```

For `branch_context.py`:

```python
import json
import subprocess
import sys

request = json.load(sys.stdin)
try:
    branch = subprocess.check_output(
        ["git", "rev-parse", "--abbrev-ref", "HEAD"],
        text=True,
        stderr=subprocess.DEVNULL,
        timeout=2,
    ).strip()
except (subprocess.CalledProcessError, subprocess.TimeoutExpired, OSError):
    branch = "unavailable"
existing = request["input"]["context"]
context = "\n".join(filter(None, [existing, f"Current Git branch: {branch}."]))
print(json.dumps({"set": {"context": context}}))
```

This context is prepared once when a turn starts. The optional failure policy retains the prior valid input if the wrapper itself fails. A detached checkout reports Git's `HEAD` value; a non-repository directory reports `unavailable`.

## Give shell commands consistent environment settings

Use `shell.env` to adjust an individual shell invocation without changing ocmini's global process environment. This example disables color escape sequences in shell output and preserves changes from earlier handlers.

```json
{
  "ocmini": {
    "hooks": {
      "handlers": [{
        "id": "plain-shell-output",
        "on": "shell.env",
        "command": ["python3", "/absolute/path/shell_env.py"]
      }]
    }
  }
}
```

For `shell_env.py`:

```python
import json
import sys

request = json.load(sys.stdin)
env = dict(request["input"]["env"])
env.update({"NO_COLOR": "1", "CARGO_TERM_COLOR": "never"})
print(json.dumps({"set": {"env": env}}))
```

The returned map is an overlay: strings set values and `null` removes inherited values. It is not a dump of the runtime's environment. This boundary also applies to command shell interpolation and background shell launch. Tools that do not honor these color variables may still emit color.

For registration, matching, and inspection, start with [hooks guide](hooks.md). All allowed fields and advanced options are in [hook reference](hooks-reference.md).
