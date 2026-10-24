# Hook architecture

Status: implemented. This document describes the current responsibilities and invariants. User entry points are in [HOOKS.md](HOOKS.md); exact boundary contracts and settings are in [HOOKS-REFERENCE.md](HOOKS-REFERENCE.md). Upstream comparisons and research are in [HOOKS-RESEARCH.md](HOOKS-RESEARCH.md).

## Public model

There are two extension interfaces: awaited operation hooks, registered in `handlers`, and event subscriptions, registered in `observers` or consumed through SSE. `turn.end.after` is an awaited, read-only operation hook. Durability is an observer delivery policy; file payloads are a transport option. Neither introduces another kind of extension.

The 26 hook names form four navigation families: conversation, tools, models, and resources. These groups do not add configuration objects, aliases, or alternate dispatch rules. The names and writable-field schemas in `src/hooks/types.rs` remain the executable catalog.

The common registration surface is `id`, `on`, and `command`. `match`, execution timeout, optional-failure behavior, payload transport, and observer delivery are independent options. Shared process/queue/storage budgets are advanced runtime policy. The existing flat `ocmini.hooks` schema is retained.

## Responsibilities

### Operation integrations own timing and native validation

The engine, providers, tools, and session services decide when an operation reaches a boundary and construct the required public input. After hook preparation they enforce native permissions, path/conflict rules, provider replay constraints, and immutable runtime fields. They do not spawn hook commands or implement observer retry loops.

- `src/engine_impl.rs`: admission, turn execution, completion candidates, settlement, finalizers, command preparation, shared tool orchestration, permissions, retries, and compaction.
- `src/provider/hooks.rs`: logical-request context/parameter/header preparation and completed-text handling, shared with title and compaction calls. Adapter code preserves provider replay and opaque state.
- `src/tools/mod.rs`, `src/tools/changes.rs`, and the tool modules: task/question/shell/skill preparation and validated file transactions.
- `src/sessions.rs`: lifecycle services shared by HTTP, terminal/headless entry points, and child tasks.

### The hook runtime owns registration, dispatch, and command execution

- `src/config.rs` records configuration provenance and trust.
- `src/hooks/config.rs` resolves registrations, merges IDs, validates policy, and produces effective inspection data.
- `src/hooks/types.rs` defines names, matchers, writable fields, and response validation.
- `src/hooks/mod.rs` caches scoped runtimes, dispatches ordered handlers, tracks invocation identity, and emits sanitized diagnostics.
- `src/hooks/process.rs` owns bounded pending/execution capacity, queue/execution deadlines, stdio capture, cancellation, process-group cleanup, and response parsing.
- `src/hooks/payload.rs` owns optional file transport, temporary-file leases/cleanup, and retained diagnostic artifacts.

A boundary is a catalog entry and a validated integration call. Its policy uses the shared runner; it does not get its own process supervisor.

### Event delivery and storage own observation and recovery

- `src/bus.rs` publishes existing event envelopes and shares immutable payloads.
- `src/hooks/events.rs` matches observers, accounts queued bytes, batches/coalesces eligible events, and runs best-effort or durable delivery through the shared command runner.
- `src/server.rs` exposes directory-scoped SSE with optional exact `types` filtering.
- `src/store/hook_state.rs` owns admission reservations, saved registration snapshots, turn results, feedback, finalization, and recovery.
- `src/store/hook_events.rs` owns transactional outbox inserts, quotas, lease fencing, acknowledgements, retries, and expiry.

Both observer delivery policies consume facts. Neither can mutate a settled operation or become an alternate authority over its outcome.

## Execution and state

A prompt is admitted input. A turn is the execution owned by one admitted user `messageID`; `noReply` stores context without creating execution. A step is a logical model response and its tool calls; transport retries are attempts within that step. A turn's kind is primary or manual compaction. Each model request separately identifies primary, compaction, or title work. Session idle means its queue drained.

Admission reserves a globally unique prompt ID before executing input hooks. Preparation runs outside the running-state lock; the final validated receipt, visible user message, and queued work commit together. Same-owner duplicates attach to existing work, and rejected reservations do not rerun preparation on retry. Abort fences reservations, and the admission deadline includes FIFO/capacity waiting and input hooks. Long checks belong after admission.

Execution starts once per turn and runs the model/tool loop. Only a natural completion candidate reaches `turn.end.before`. Continuation feedback is stored on its owning response and included in later model context under the original prompt and usage attribution. The durable continuation count is separate from an agent's step limit. New steering or cancellation preempts the gate. Manual compaction uses its dedicated hooks and bypasses that gate.

Settlement saves the exact response reference or error plus prepared terminal facts and enters `finalizing`. The read-only after hook runs next. Finalization commits the done state and accepted durable notifications in one transaction, then publishes terminal facts and releases synchronous callers. A later queued turn may begin without making the session idle. Queued cancellations finalize with `started: false`; they never run the start hook. Turn hooks default to primary turns; start/after can explicitly opt into manual compaction.

Terminal reasons are `completed`, `steered`, `cancelled`, `error`, `step_limit`, `hook_rejected`, and `hook_limit`. Finalizer diagnostics are separate from the selected outcome.

## Invariants

- The engine authorizes the final effective operation after all preparation. Hooks cannot broaden permissions, change call identity, fabricate user answers, replace runtime authentication, or alter measured usage and immutable execution results.
- Model transforms run once per logical request. Transport retries do not repeatedly append transformed context. Completed-text changes are reconciled with provider replay while opaque reasoning and signatures remain protected.
- A failed post-execution transform reports that effects already ran. It does not cause the runtime to replay the operation. A failed compaction transform preserves the prior recoverable checkpoint.
- A live duplicate joins the existing work. A settled duplicate returns its exact recorded result; a duplicate handled by another process waits for durable finalization. Polling that state does not reconstruct a large response on each wakeup.
- During abort finalization, local admission is fenced under the running lock and sibling writes are fenced by the owner lease. The heartbeat remains active through cleanup. New admissions receive busy responses instead of becoming orphaned queued turns.
- Finalizers have at most one automatic dispatch attempt. A crash can mark one interrupted/skipped. Queued unstarted turns can resume; interrupted started turns do not replay possibly completed effects.
- Recovery uses saved subscriptions to enqueue terminal notifications independently of live observer scopes. Recovered facts preserve model and turn-kind metadata. Workers execute only the matching active registration; consumer event-ID deduplication handles possible redelivery.
- Outbox quota rejection produces a diagnostic. Storage errors propagate immediately, preserving the transaction and recovery snapshot; cleanup never continues in autocommit after a rollback.
- Observer queues account exact retained event bytes. File mode does not charge an oversized-event sentinel as if it were the full payload. Coalescing applies only to replaceable state updates and preserves retained order.
- Handler schemas and ordered transformations preserve IDs, authority, and ordinary validation. Optional failure policy can discard a failed transformation; it cannot ignore an explicit denial or cancellation.

## Resource and diagnostic policy

With no active registrations the runtime starts no hook processes or observer workers. Handler lookup is indexed by name. Directory runtimes are cached with bounded retention; unmatched events avoid encoding, and event payloads share immutable ownership. Pending/execution capacity is acquired before serialization and retained through response validation.

Operation hooks and observers have separate global process/pending pools. Execution time and queue waiting are separate; chain deadlines are optional except for short admission. File transport spools snapshots rather than growing inline control messages. Stderr drains continuously into a retained tail. Commands are terminated and reaped on timeout, cancellation, or shutdown.

Ordinary observer queues and SSE are best-effort. Durable terminal delivery uses explicit attempt, retention, and quota policy. Durable workers need the matching directory configuration loaded; there is no installed background service. Neither finalizers nor observers promise exactly-once external effects.

Diagnostics carry invocation/handler/boundary identity, timings, sanitized failures, and optional private stderr-artifact paths. Credentials, opaque reasoning, and environment/header dumps are excluded. Hook diagnostics require explicit observer subscriptions, and invocation identity prevents supported synchronous re-entry paths from recursively waiting on themselves. Arbitrary scripts making unrelated HTTP calls cannot be perfectly identified.

Current defaults, platform checks, and all tuning fields are maintained in the [reference](HOOKS-REFERENCE.md#settings-by-concern) and inspection output.

## Scope

All 26 hooks, 49 events, inline/file transports, and best-effort/durable observer policies remain implemented. The organization above adds no runtime mode, public naming migration, or second configuration schema.

OpenCode JavaScript plugin loading, arbitrary custom tool/provider implementations, raw transport mutation, executable per-token hooks, general event replay, external filesystem watching, and buffered-output redaction are outside this interface. Already streamed text remains visible during completion validation.
