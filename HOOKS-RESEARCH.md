# Hook and event limits: research and proposed revisions

Research snapshot: **2026-10-24**. This document compares 15 systems using official documentation and published source. Three Gemini research tasks supplied leads; the conclusions below use directly checked primary sources. The recommendations have now been implemented; the original audit below records the behavior that motivated them. Start with [HOOKS.md](HOOKS.md); exact current contracts are in [HOOKS-REFERENCE.md](HOOKS-REFERENCE.md).

## Follow-up research and implementation

Additional checks focused on correctness at the boundaries. Kubernetes distinguishes an explicit rejection from a failed webhook; our optional-failure policy preserves that distinction. [Admission contract](https://kubernetes.io/docs/reference/access-authn-authz/extensible-admission-controllers/) AWS's transactional-outbox guidance requires event recording and the domain change in one transaction, with idempotent consumers; this informed the deliberately narrow durable terminal-event support. [Outbox pattern](https://docs.aws.amazon.com/prescriptive-guidance/latest/cloud-design-patterns/transactional-outbox.html) Tokio's semaphore contract informed cancellation-safe capacity ownership through encoding, execution, and response parsing. [Semaphore reference](https://docs.rs/tokio/latest/tokio/sync/struct.Semaphore.html) Tempfile cleanup is tied to object lifetime and does not survive a process crash automatically; file payloads therefore add owner leases and bounded abandoned-directory reclamation on Unix. [TempDir lifetime](https://docs.rs/tempfile/latest/tempfile/struct.TempDir.html)

Implemented: bounded draining stderr tails and private retained diagnostics; explicit failure policies; optional chain deadlines, independent queue/execution timing and longer execution overrides; configurable global/pending capacity; opt-in file payloads and complete response-file validation; continuation opt-out; observer overflow/coalescing policies; and an opt-in bounded SQLite outbox for exact turn-ended/command-executed facts. Domain commit/recovery tests cover finalizer crashes and notification retries without repeating model or tool work. Most numeric defaults were retained; no provider quota was copied as a local requirement.

## Recommendation

Keep bounds on resident memory, process concurrency, and queued work. Change how the system handles large payloads, verbose diagnostics, long checks, and optional-hook failures. Several current numeric ceilings were conservative implementation choices; the research does not establish them as industry requirements or workload-tested defaults.

The strongest changes are:

1. Keep a bounded stderr tail while draining excess output; do not kill a valid hook merely for logging too much.
2. Support larger input and mutation data through explicit local payload references, with bounded memory and disk use.
3. Remove the blanket two-minute ceiling for explicitly configured execution hooks. Treat queue waiting, execution, chain deadlines, and admission receipt latency separately.
4. Distinguish an intentional rejection from a crashed or unavailable hook, with an explicit failure policy.
5. Keep best-effort observers inexpensive; offer durable delivery only where an integration requires it.

## What the closest coding tools do

“Not specified” means the checked documentation does not publish a value. It does **not** mean unlimited. A limit on text delivered to a model is also different from a limit on the complete JSON control response.

| System | Verified behavior | Implication for ocmini |
| --- | --- | --- |
| **Claude Code** | Current defaults are 600s for command/HTTP/MCP hooks on most events, 30s on prompt submission/model switches, and 10s on message display. Selected output strings and plain stdout have a 10,000-character inline cap; larger content becomes a file reference with a preview. Input includes `transcript_path`. Most unexpected failures continue; `onFailure: "block"` enables blocking for supported hooks. [Official reference](https://code.claude.com/docs/en/hooks) | Separate inline content from control decisions; allow explicit long checks and failure policies. The text-field cap is not a documented total stdout JSON byte limit. |
| **Gemini CLI** | Configurable 60,000ms default; JSON stdin/stdout and stderr diagnostics; `transcript_path` input. Docs distinguish exit 2 rejection from other warning exits. [Official reference](https://geminicli.com/docs/hooks/reference/) The checked command runner uses `spawn` and accumulates stdout/stderr strings without a byte check in that method. [Pinned source](https://github.com/google-gemini/gemini-cli/blob/cd1b45c2db0a5ea7b7fc3b67c1ba5879601d6005/packages/core/src/hooks/hookRunner.ts) | There is no basis for importing Node `exec`'s default buffer cap into this `spawn` path. Unbounded accumulation is not a memory-efficiency pattern to copy. |
| **Cursor** | Configurable timeout, documented default described as “platform default.” `failClosed` defaults false, with stricter invalid-response behavior for permission hooks. Stop/subagent-stop follow-ups default to 5 per script; `loop_limit: null` removes the cap. Byte caps are not specified in the checked reference. [Official reference](https://cursor.com/docs/hooks) | Failure policy and continuation policy can be user choices. Do not infer a numeric timeout from examples. |
| **GitHub Copilot** | Command `timeoutSec` defaults to 30s. Command `preToolUse` errors deny, but timeouts fall through to normal permissions; HTTP hook errors also fall through. Other events have different failure behavior. Numeric stream caps are not specified in the checked reference. [Official reference](https://docs.github.com/en/copilot/reference/hooks-reference) | Failure behavior varies even within one product. Label the actual boundary and transport; do not combine legacy VS Code and current Copilot rules. |
| **OpenCode v1.19.4** | Plugin dispatch awaits mutable callbacks sequentially; event callbacks are invoked without awaiting their promises. This dispatch layer has no spawned-hook stdio byte budget or standalone callback timeout. [Pinned dispatcher](https://github.com/anomalyco/opencode/blob/v1.19.4/packages/opencode/src/plugin/index.ts) | In-process JavaScript plugins have a different resource boundary. Absence of an IPC quota is not evidence of isolated or bounded plugin execution. |

## What large webhook and event platforms do

The following notification systems deliver facts about operations. Their receiver acknowledgement deadlines are **not** limits on how long background business logic can run, and they do not provide a pre-operation veto.

| System | Verified size, time, and delivery behavior | Relevant pattern |
| --- | --- | --- |
| **GitHub webhooks** | Payload cap 25 MB; oversized payloads are not delivered. [Payload docs](https://docs.github.com/en/webhooks/webhook-events-and-payloads) Respond within 10s and queue work. [Best practices](https://docs.github.com/en/webhooks/using-webhooks/best-practices-for-using-webhooks) Failed deliveries are not automatically redelivered. [Failure handling](https://docs.github.com/en/webhooks/using-webhooks/handling-failed-webhook-deliveries) | A mature system can still lose delivery; do not assume every event bus is durable. |
| **Stripe webhooks** | Respond quickly, process with an asynchronous queue, and expect live-mode automatic retries for up to three days. [Webhook docs](https://docs.stripe.com/webhooks) Thin notifications carry identifiers; consumers retrieve full event/resource data as needed. [Event formats](https://docs.stripe.com/events/how-events-work) | Small notification plus retrievable data; retries require idempotent consumers. No general numeric outbound payload cap or acknowledgement deadline asserted here. |
| **Slack Events API** | Acknowledge within 3s; recommends a queue and processing separately. Failed delivery is retried three times. [Official reference](https://docs.slack.dev/apis/events-api/) | Fast receipt and asynchronous work are separate operations. |
| **Shopify webhooks** | HTTPS connection timeout 1s, total request timeout 5s. Recommends queues and reconciliation. Retries eight times over four hours; exhausted failures delete Admin-API-created subscriptions. [Verified-delivery docs](https://shopify.dev/docs/apps/build/webhooks/verify-deliveries) | Explicit retry/reconciliation behavior matters more than choosing a larger memory queue. No unverified large-payload URL scheme asserted here. |
| **AWS EventBridge** | `PutEvents` permits up to 10 entries whose combined size is less than 1 MB (1,048,576 bytes); one entry can use that request budget. Larger data should go to S3 with a URL in the event. [Publishing docs](https://docs.aws.amazon.com/eventbridge/latest/userguide/eb-putevents.html) Default delivery retry policy is 24h/up to 185 attempts; a DLQ can retain failures. [Retry docs](https://docs.aws.amazon.com/eventbridge/latest/userguide/eb-rule-retry-policy.html) | Explicit payload references and bounded retry retention. Older 256 KB claims are outdated for this checked interface. |
| **Amazon SQS** | Current maximum message size is 1 MiB. Extended clients support payloads up to 2 GB by storing them in S3 and sending references. [Current quotas](https://docs.aws.amazon.com/AWSSimpleQueueService/latest/SQSDeveloperGuide/quotas-messages.html) | A small transport envelope need not impose the same limit on logical data size. |
| **AWS Lambda** | Standard function timeout ceiling 900s; 6 MB synchronous request/response and 1 MB asynchronous payload. Managed Instances have separate longer invocation limits. [Quotas](https://docs.aws.amazon.com/lambda/latest/dg/gettingstarted-limits.html) | Execution time, payload size, concurrency, and hosting model are distinct budgets. These cloud quotas do not establish sensible local-hook defaults. |
| **Google Cloud Pub/Sub** | Message data limit 10 MB. [Quotas](https://docs.cloud.google.com/pubsub/quotas) Subscriber flow control limits outstanding messages and bytes; clients stop pulling until acknowledgements free capacity. [Subscription guidance](https://docs.cloud.google.com/pubsub/docs/subscribe-best-practices) | Bound both counts and bytes, then apply a documented backpressure policy. |
| **Azure Event Grid** | Event size 1 MB for checked topic interfaces. [Quotas](https://learn.microsoft.com/en-us/azure/event-grid/quotas-limits) The checked push-delivery interface waits 30s, then retries; defaults are 30 attempts/24h, with optional dead-letter storage. [Delivery docs](https://learn.microsoft.com/en-us/azure/event-grid/delivery-and-retry) | An acknowledgement timeout and durable retry policy solve different problems. |

**Kubernetes admission webhooks** are the closer comparison for synchronous gates: default timeout 10s, configurable from 1–30s; `failurePolicy` selects `Ignore` or `Fail`, defaulting to `Fail`. A correctly delivered explicit rejection is enforced regardless of that failure policy. [Admission reference](https://kubernetes.io/docs/reference/access-authn-authz/extensible-admission-controllers/)

Two additional architecture references support compact payloads. CloudEvents' 64 KByte figure is an interoperability acceptance/forwarding baseline, **not a universal maximum**; the specification recommends linking large data. [CloudEvents 1.0.2](https://github.com/cloudevents/spec/blob/v1.0.2/cloudevents/spec.md#size-limits) Microsoft's claim-check pattern stores payloads outside the broker and transmits a reference. [Azure architecture guidance](https://learn.microsoft.com/en-us/azure/architecture/patterns/claim-check) Applying these patterns to local files is our design recommendation.

## Audit of ocmini before these revisions

Checked against [configuration](src/hooks/config.rs), [subprocess runner](src/hooks/process.rs), [observers](src/hooks/events.rs), and [runtime](src/hooks/mod.rs).

| Current behavior | Proposed policy | Reason |
| --- | --- | --- |
| 1 MiB stdin, configurable to 16 MiB; oversized input fails before spawn | Keep an inline budget; add an explicit reference-capable payload mode with bounded streaming/spooling | Context and multi-file mutations can exceed small envelopes without requiring equally large resident buffers. |
| 256 KiB stdout, configurable to 4 MiB; overflow fails | Keep a bounded, complete JSON control response; support references for large replacement values and avoid returning unchanged context | A decision must be parsed completely. Never parse a truncated prefix or treat truncation as an allow. |
| 64 KiB stderr, configurable to 1 MiB; overflow kills the hook; captured stderr is discarded | Drain continuously; retain a bounded tail and byte counts; optionally retain a bounded diagnostic artifact | A verbose compiler or test runner should not fail an otherwise valid check. Diagnostic retention is different from a decision limit. |
| 30s command/60s chain defaults; command and chain overrides capped at 120s | Keep the convenient command default; allow explicitly longer execution. Make any overall chain deadline explicit and show the effective deadline | Raising a command timeout should not silently leave a shorter chain timeout in force. |
| Capacity waiting consumes command/chain time | Track queue-wait and execution time separately, while preserving any explicit end-to-end deadline | Saturation should be reported as saturation, rather than suggesting a script used all its execution budget. |
| 2s total admission default, hard max 5s | Keep receipt admission short; put expensive checks after durable admission in `turn.start.before` | [The local T3 contract](SPEC-CONTRACT.md) expects the command receipt within 10s. This is a project constraint, not a cloud-webhook analogy. |
| Observers/finalizers default to 5s; failures diagnostic | Retain non-blocking notification semantics and configurable execution; distinguish finalization/shutdown waits from ordinary observer work | Already-committed outcomes cannot be vetoed by a logger. Expensive completion checks belong before settlement. |
| Awaited hook failures generally reject the operation | Add per-registration `failurePolicy: block | continue`; preserve blocking defaults for gates/mutators and diagnostic behavior for observers | Optional transformations can explicitly continue on infrastructure failure. Continue means keep the previous valid value and normal permission flow; explicit denials still deny. |
| Runner capacity 8 by default; fixed process-wide ceilings of 16 awaited hooks/4 observers | Keep bounded separate pools; make trusted process-wide policy configurable and visible | Prevent excessive subprocesses without a hidden ceiling that overrides configuration. |
| Observer queue 128 entries/4 MiB; overflow drops incoming events; large events become marked references | Keep bounded best-effort default; add explicit overflow policy and retrievable payloads. Consider optional durable delivery for selected terminal/audit events | A marker is useful only when consumers can recover the necessary data. No general replay guarantee exists today. |
| End-hook continuations default to 3; hard maximum 100 | Keep a finite default; allow a trusted explicit override/opt-out, with visible count and cancellation | This counts hook-requested follow-ups, not ordinary tool/model steps. There is no evidence that 100 is a universal correct ceiling. |
| Input serialization happens before acquiring process permits | Bound waiting invocations and obtain a serialization-memory budget before allocating encoded input; reuse immutable bytes where safe | A subprocess cap alone does not bound buffers held by all queued callers. |

These budgets protect ocmini's supervisor and queues. They do **not** limit arbitrary child-process CPU, heap, filesystem, or network use; the hook runner is not an OS sandbox. Kernel pipe capacity is also not a cumulative stdout/stderr allowance: continuously draining a pipe permits much larger total streams without keeping them in RAM.

## Implementation sequence for the revisions

### 1. Diagnostics and explicit failure handling

Change `src/hooks/process.rs` to return a typed outcome containing exit status, elapsed/queue time, complete bounded stdout, stderr tail, and total diagnostic bytes. Separate stream-read failure from a full diagnostic buffer. Keep draining when the tail is full. On a failure, expose a useful bounded diagnostic; optional full logs need retention and disk budgets. Do not put captured secrets into broad observer events.

Add the failure policy in `src/hooks/config.rs` and apply it in `src/hooks/mod.rs`: intentional block, startup failure, timeout, malformed result, capacity exhaustion, and cancellation must remain distinct. Explicit cancellation always cancels. Continuing after a failed transformation must discard that transformation completely and retain all ordinary validation/authorization.

Verify with a hook that emits many megabytes of stderr and valid JSON, a crashing optional hook, a crashing required gate, and an intentional rejection under each policy.

### 2. Time and capacity policy

Remove the 120s validator ceiling for trusted execution configuration; preserve checked duration conversion and cancellation. Define queue age, per-command execution time, optional chain deadline, and the mandatory admission deadline separately. Display effective budgets in `ocmini debug hooks`, including which deadline wins. Admission retains abort fencing and FIFO expiry.

Verify an explicitly configured check lasting beyond two minutes, saturation where a hook has not started, chain expiry, cancellation of the process group, and the ten-second receipt contract. A benchmark can use a shortened clock; an end-to-end smoke check should confirm real process behavior.

### 3. Large payloads with bounded resident memory

Add a versioned, opt-in reference protocol so existing scripts continue receiving their current schema. Supply stable resource IDs for retrievable stored records and private temporary files for data without a durable representation. References must identify the exact invocation snapshot, with byte length, content type, and lifetime; reading today's live file is not equivalent to reading the candidate state a gate checked.

Stream/spool large data once and reuse it across handlers where immutable. Keep a small complete control response; large mutations return a reference or a compact, explicitly defined patch. Validate the entire effective result and file/permission preconditions before committing. Reject malformed or expired references; never convert missing data into implicit approval. Bound disk retention, pending encodes, and total resident bytes rather than relying only on process count.

Verify byte-exact multi-file changes and large contexts, snapshot consistency, oversized/invalid control responses, cleanup on cancellation, disk exhaustion, and peak resident memory under concurrent queued calls.

### 4. Event delivery policy and tuning

Keep SSE and ordinary observers best-effort and cheap. Permit coalescing only for replaceable progress/state updates, with gap reporting and a state-resynchronization path. For integrations needing reliable terminal/audit delivery, add an opt-in SQLite outbox committed with the domain change, bounded retention, stable event IDs, backoff, and inspectable exhausted deliveries. Document at-least-once delivery and require idempotent consumers; do not promise exactly-once external effects. Do not persist every token delta.

Measure serialization bytes/time, queue age, dropped/coalesced events, hook runtime, and peak supervisor memory with representative repositories before changing numeric defaults. User-facing configuration should emphasize timeout, failure policy, payload mode, and delivery mode; low-level cache/argv/schema bounds can remain documented engineering guardrails.

The implementation sequence above is complete. The current configuration contract is recorded in [HOOKS-REFERENCE.md](HOOKS-REFERENCE.md).

## Comparison with OpenCode

The primary comparison is OpenCode **v1.19.4**, matching this repository's advertised API target. The current development API was also checked; it is a different plugin surface and should not silently become our compatibility target.

| Capability | OpenCode 1.19.4 | Implemented ocmini behavior |
| --- | --- | --- |
| Extension runtime | In-process JavaScript/TypeScript plugins; local modules and npm loading | Explicit executable/argv commands; one bounded JSON request/response per invocation |
| Input preparation | `chat.message`, `command.execute.before` | `prompt.accept.before` and command preparation hooks before durable admission |
| Model customization | `chat.params`, `chat.headers`, experimental message/system transforms | Typed context, parameters, and header transforms shared across adapters |
| Tools | `tool.execute.before`, `tool.execute.after`, `tool.definition`, `shell.env` | Similar boundaries; final arguments always pass permissions again; terminal tool failures are observable |
| Permission interception | `permission.ask` is declared; declaration alone does not establish an active call site | `permission.decide` has an explicit call site and can only tighten the runtime decision |
| Compaction | Experimental compaction prompt/context and auto-continuation hooks | Before/after compaction hooks; all normal model hooks carry a request kind |
| Generated text | `experimental.text.complete` runs at a text-part boundary | `message.text.complete` has the same granularity and streaming limitation |
| Before whole-turn completion | No dedicated stop/turn-end gate in the checked hook interface | `turn.end.before` can request bounded continuation of the same turn |
| After whole-turn completion | Idle/status and message events provide observations | Per-prompt `turn.end.after` plus `ocmini.turn.ended`; idle remains a queue event |
| Delivery guarantees | Event subscription and awaited plugin triggers are different mechanisms | Awaited hooks, best-effort observers, and opt-in durable terminal-event delivery |
| New development API | Domain hooks such as session prompt/context/retry, HTTP and WebSocket hooks, tool execution hooks | Borrow useful semantics; defer raw transport and SDK replacement interfaces |

Sources: [1.x hook definitions](https://github.com/anomalyco/opencode/blob/v1.19.4/packages/plugin/src/index.ts), [1.x plugin dispatch](https://github.com/anomalyco/opencode/blob/v1.19.4/packages/opencode/src/plugin/index.ts), [1.x processor](https://github.com/anomalyco/opencode/blob/v1.19.4/packages/opencode/src/session/processor.ts), [1.x session status](https://github.com/anomalyco/opencode/blob/v1.19.4/packages/opencode/src/session/status.ts), [plugin documentation](https://opencode.ai/docs/plugins/). Development reference, pinned to the checked commit: [session hooks](https://github.com/anomalyco/opencode/blob/f20760b8bf3bced0fa5620b6afe7b78dd615ee36/packages/plugin/src/promise/session.ts), [tool hooks](https://github.com/anomalyco/opencode/blob/f20760b8bf3bced0fa5620b6afe7b78dd615ee36/packages/plugin/src/promise/tool.ts).

## Upstream catalog reference

OpenCode v1.19.4 declares these **21 plugin extension fields**. Some register capabilities or lifecycle cleanup rather than intercepting an operation; `permission.ask` is declared but not dispatched by the checked permission implementation. There is no `command.execute.after`.

```text
dispose, event, config, tool, auth, provider,
chat.message, chat.params, chat.headers, permission.ask,
command.execute.before, tool.execute.before, tool.execute.after, shell.env,
experimental.chat.messages.transform, experimental.chat.system.transform,
experimental.provider.small_model, experimental.session.compacting,
experimental.compaction.autocontinue, experimental.text.complete, tool.definition
```

The compaction auto-continue hook controls a synthetic continuation after successful compaction; it is not a general completion gate. The event callback is dispatched without awaiting its returned promise, whereas mutation hooks are awaited in registration order. Sources: [dispatch](https://github.com/anomalyco/opencode/blob/v1.19.4/packages/opencode/src/plugin/index.ts#L244), [permission implementation](https://github.com/anomalyco/opencode/blob/v1.19.4/packages/opencode/src/permission/index.ts#L61), [compaction call site](https://github.com/anomalyco/opencode/blob/v1.19.4/packages/opencode/src/session/compaction.ts#L497), [command event timing](https://github.com/anomalyco/opencode/blob/v1.19.4/packages/opencode/src/session/prompt.ts).

The latest tagged SDK v2 union has **89 event names**, including parallel migration events. Brace notation below expands each suffix. This is an upstream reference, not the ocmini implementation backlog:

```text
installation.{updated,update-available}
project.{updated,directories.updated}
server.{connected,instance.disposed}
global.disposed
permission.{asked,replied}
permission.v2.{asked,replied}
question.{asked,replied,rejected}
question.v2.{asked,replied,rejected}
lsp.updated
file.{edited,watcher.updated}
message.{updated,removed,part.updated,part.delta,part.removed}
session.{created,updated,deleted,status,idle,compacted,diff,error}
todo.updated
command.executed
tui.{prompt.append,command.execute,toast.show,session.select}
mcp.{tools.changed,browser.open.failed}
vcs.branch.updated
workspace.{ready,failed,status}
worktree.{ready,failed}
pty.{created,updated,exited,deleted}
models-dev.refreshed
integration.{updated,connection.updated}
catalog.updated
plugin.added
reference.updated
session.next.{agent.switched,model.switched,moved,prompted,
              prompt.admitted,context.updated,synthetic,retried}
session.next.shell.{started,ended}
session.next.step.{started,ended,failed}
session.next.text.{started,delta,ended}
session.next.reasoning.{started,delta,ended}
session.next.tool.input.{started,delta,ended}
session.next.tool.{called,progress,success,failed}
session.next.compaction.{started,delta,ended}
session.next.revert.{staged,cleared,committed}
```

Source: [v1.19.4 SDK v2 event union](https://github.com/anomalyco/opencode/blob/v1.19.4/packages/sdk/js/src/v2/gen/types.gen.ts#L7). The older SDK event union imported by the 1.x plugin interface is stale, and the documentation mixes notifications with mutation hooks; neither is a reliable exhaustive runtime catalog.

The checked development API has **18 runtime hooks**: session `prompt`, `context`, `compaction`, `generate`, `title`, `model.request`, `http.request`, `http.response`, `experimental.ws.handshake`, `experimental.ws.send`, `experimental.ws.receive`, `retry`; tool `execute.before`, `execute.after`; permission `evaluate`; shell `create.before`; AI SDK `sdk`, `language`. Only tool-before has a typed rejection channel in that dispatcher; other awaited hooks are not automatically rejection gates. Additional domain transforms and event subscriptions are separate extension mechanisms. Sources: [development dispatcher](https://github.com/anomalyco/opencode/blob/f20760b8bf3bced0fa5620b6afe7b78dd615ee36/packages/core/src/plugin/hooks.ts#L21), [permission](https://github.com/anomalyco/opencode/blob/f20760b8bf3bced0fa5620b6afe7b78dd615ee36/packages/plugin/src/promise/permission.ts), [shell](https://github.com/anomalyco/opencode/blob/f20760b8bf3bced0fa5620b6afe7b78dd615ee36/packages/plugin/src/promise/shell.ts), [AI SDK](https://github.com/anomalyco/opencode/blob/f20760b8bf3bced0fa5620b6afe7b78dd615ee36/packages/plugin/src/promise/aisdk.ts).
