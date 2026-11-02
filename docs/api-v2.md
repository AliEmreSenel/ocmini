# OpenCode v2 HTTP/SSE contract

ocmini advertises 2.2.5-ocmini.<package version> and implements the native v2 operations used by T3 Code. Set the OpenCode provider binary to ocmini. Launch flags and the flushed server-listening banner remain unchanged:

~~~sh
ocmini serve --hostname 127.0.0.1 --port 4096
~~~

The existing Rust engine, SQLite transcript store, hooks, provider protocols, streaming checkpoints, tool concurrency and ARM targets are retained. V2 adds session state to the same database. No JavaScript runtime, device detection, hardware-specific mode or new performance restriction is required. The JavaScript packages under tests/v2-client are optional development tooling.

## Discovery, authentication and catalogs

GET /api/info returns version, pid, urls and paths.tmp. OPENCODE_PASSWORD takes precedence over OPENCODE_SERVER_PASSWORD. A nonempty configured password requires UTF-8 HTTP Basic credentials for user opencode, including on discovery and event requests. V2 authentication and operation errors use tagged JSON objects with _tag and message.

GET /api/model, /api/provider, /api/agent, /api/skill and /api/command return location-scoped catalogs. The query key is location[directory], with a canonical absolute directory. Catalogs use {location, data}; model/agent/skill IDs and variants follow v2 shapes. Only connected providers contribute selectable models. Existing provider discovery and supported Chat Completions, Responses and Anthropic transports remain shared with CLI use.

## Sessions and prompts

POST /api/session creates a session with optional id, parentID, title, model, agent, location, permissions and metadata. Model references use {providerID, id, variant?}. Permissions use ordered {action, resource, effect} records; shell/subagent actions map to the runtime's existing shell/task policy checks.

GET/PATCH/DELETE /api/session/{sessionID} read, update and remove sessions. GET /api/session/active returns {data: {sessionID: {type: "running"}}} for this process's active executions. Session responses use {data: session}; selection, update, move and removal return 204. Removing a session stops its work and removes descendants.

Agent and model selections persist through POST /api/session/{sessionID}/agent and /model. POST /move admits a directory move at a step boundary. Session reads expose saved location, model/agent selection, usage, busy/idle timestamps and the latest outcome.

POST /api/session/{sessionID}/prompt accepts:

~~~json
{
  "id": "msg_client_supplied_id",
  "text": "Inspect this project",
  "files": [{"uri": "file:///workspace/notes.txt", "name": "notes.txt"}],
  "delivery": "steer",
  "resume": true
}
~~~

The response contains the admitted inbox item. Admission is durable before acknowledgment; pending input is excluded from public v2 history until execution takes it. Reusing the same user ID returns the original receipt without another provider call. Files retain the existing attachment snapshot behavior. Explicit skill mentions load discovered skill content into the admitted request.

Delivery defaults to steer. Steers are considered at safe step boundaries; queue items wait for the current turn to finish. Compaction/move controls form ordering barriers. resume:false records input without starting execution, including after restart; it becomes eligible when another input resumes execution. GET /inbox inspects pending items and DELETE /inbox/{inboxID} retracts one.

POST /synthetic admits synthetic text and metadata through the same inbox. Background subagent reports carry source:subagent and childID metadata so T3 can attribute autonomous follow-up work.

POST /command accepts name/text and executes existing configured command templates. POST /compact admits a manual compaction control and uses the existing summarizer. V2 compaction.buffer accepts a token count or percentage from 1% to below 100%; compaction.keep.tokens selects the retained recent-context budget. Existing compaction settings and shrinking-overflow recovery remain supported.

POST /interrupt returns {interrupted: boolean}. It cancels locally owned execution, shell work and approval/form waits while preserving pending input. T3 can retract unwanted steers explicitly. Existing v1 abort behavior remains available on the legacy route.

## History, rewind and fork

GET /api/session/{sessionID}/message returns {data, cursor} with typed user, synthetic, assistant, compaction, selection and idle records. Use order and limit for the first page, then cursor.next for subsequent pages. Text/reasoning and tool content are embedded in assistant content; native provider replay data stays separate from UI metadata. Existing sessions remain readable through this projection.

Rewind uses POST /revert/stage with {messageID, files:false}, POST /revert/commit and DELETE /revert. It changes history in the same session and preserves files managed by T3 checkpoints. A staged revert is applied before a subsequent prompt. Clearing a revert wakes the existing engine and delivers pending input, matching upstream execution settlement. File-restoring revert is outside this implementation and returns an explicit error rather than pretending to restore files.

POST /fork accepts an optional before message ID, which is excluded from the copy. It remaps copied message/part identities, records independent fork provenance, inherits current instructions/selections and avoids billing copied history as new work. T3 can move the resulting session to a destination worktree.

## Approvals, forms, instructions and MCP

GET /api/session/{sessionID}/permission lists pending requests. POST /permission/{requestID}/reply accepts {decision: "once" | "always" | "reject", message?}. Rejection with feedback allows the runtime to continue without that tool; rejection without feedback interrupts execution. Runtime session policy still controls subsequent decisions.

Questions appear through GET /api/session/{sessionID}/form and form.created events. String and multiselect fields use stable string keys. POST /form/{formID}/reply accepts {answer: {key: string | string[]}}. DELETE /form/{formID} cancels the question. Session ownership and answer validity are checked before settlement.

PUT /api/experimental/session/{sessionID}/instructions/entries/{key} stores a keyed JSON value. Entries affect subsequent model steps and survive restart, compaction, history rewind and fork. GET the entries collection to inspect them or DELETE an entry to remove it. Updating instructions does not start a model turn.

PUT /api/experimental/mcp/{server}?location[directory]=... accepts {config} and performs the real MCP handshake. An unchanged registration reuses its connection; a failed handshake returns an error. DELETE stops and removes the runtime registration. T3's unique per-thread server names and ordered permission rules retain workspace/thread isolation.

V2 model-facing tool catalogs use shell and subagent, retaining bash/task input aliases. Subagent inputs support agent, description, prompt, model, sessionID and background. Tool progress and results publish metadata.sessionID; background launches publish status:running and later deliver synthetic completion reports. Existing foreground/background execution and parallel tool behavior remain shared with the terminal client.

## Event stream

GET /api/event is global across server locations. It emits an initial server.connected event, 15-second SSE keep-alives and JSON events with id, created, type, data and optional location. Session facts have monotonically persisted per-session durable sequence metadata. Text/reasoning deltas remain live-only and use existing batched transcript persistence; frames are encoded once and shared between subscribers. The feed is allocated on first subscription.

Lifecycle events cover input enqueue/delivery/cancellation, execution start/success/failure/interruption, step start/streamed/end/failure, text/reasoning start/delta/end, tool input/call/progress/outcome, retries, compaction, permissions, forms, selections, movement and revert. Text ordinals and assistant/tool IDs retain their native attribution. A slow subscriber fails visibly using the existing event-lag policy; the live stream does not advertise replay. History, active state and session outcomes support reconnection.

A full replay log endpoint is not implemented. Existing hook events and command-hook payload protocols remain unchanged; their versions are independent of the OpenCode HTTP generation.

## Compatibility scope

The [endpoint inventory](opencode-v2-endpoints.csv) identifies implemented operations and deferred surfaces. All 34 direct calls traced in the pinned T3 provider have routes; the broader OpenCode desktop, hosted services, plugin ecosystem, PTY/schedule/artifact APIs and provider SDK integrations remain excluded.

T3's pinned 2.0.28 schemas require legacy compaction fields removed by v2.2.5. Native compaction is implemented without a compatibility shim. As accepted by the user, older T3 builds may miss compaction lifecycle updates or fail to decode history after compaction. This is independent of retaining v1 HTTP routes.

Legacy HTTP routes and internal event consumers remain available during the transition. They share the same runtime and storage. Public v1 retirement remains a follow-up decision; no second engine or dual transcript database was introduced. Startup retains ocmini's existing interruption recovery and ownership leases, including the rule against directly replaying ambiguous tool effects.

## Verification

~~~sh
cargo fmt --all -- --check
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo test --all-targets --locked
cargo build --release --locked
bun install --cwd tests/v2-client --frozen-lockfile --ignore-scripts
cargo test --locked --test contract official_v2_client_and_schema_conformance -- --ignored
~~~

The optional last test exercises both the current official client and T3's pinned client against a live local fixture server, plus their event/history/catalog schemas, tool outcomes, permission replies and forms. The development fixture shares Effect 4.0.6 between the two client versions. Normal Rust integration tests cover state, streaming, cancellation, compaction, background children, forms, permissions, configuration and history controls without a JavaScript runtime or paid model credentials.
