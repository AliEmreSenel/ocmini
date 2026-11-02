# OpenCode v2.2.5 parity and migration plan

Research date: 2026-11-02. Status: P0 implemented on the existing runtime; see [the v2 contract](api-v2.md) for current behavior. The audit below records the starting point and wider backlog.

**Recommendation:** make v2 the runtime and public HTTP target. Build it beside the existing v1 implementation during development, preserve existing sessions/configuration/hooks, and retire public v1 after the v2 acceptance gates pass. Do not commit to permanent dual HTTP support without an identified consumer. Target native v2.2.5 and current T3 behavior, with the user-accepted older-client compaction limitation below.

## Preserve the current lightweight implementation

**User clarification, 2026-11-02:** the current implementation already meets the performance requirements, including single-core machines with roughly 500 MB of total RAM and embedded devices using SD cards. Add v2 support while preserving that implementation approach.

Use the same behavior across machines. Do not detect hardware capabilities, add small-device modes, impose new resource caps or concurrency restrictions, throttle work, or truncate results as a performance shortcut. Capable machines must retain existing functionality and throughput.

Reuse the Rust runtime, SQLite, streaming/paging mechanisms, lazy initialization and current concurrency behavior. Keep new v2 code economical in dependencies, allocations and storage writes. Preserve ARMv6/ARMv7/ARM64 support. Implement required protocol and durability behavior directly without reproducing upstream's service architecture.

The migration does not include a resource audit, numerical budgets, a mandatory benchmark phase or a performance redesign. Investigate a concrete regression if v2 work introduces one. Existing unrelated limits and settings are outside this migration's scope.

## Sources and reproducibility

The real repositories were cloned outside the project. No vendored upstream code or upstream dependencies were added to ocmini.

| Checkout | Pinned revision | Local checkout |
| --- | --- | --- |
| ocmini 0.3.5 | 4159234bce06befb5d68842f9847d9e07da7e404 | This worktree |
| anomalyco/opencode v2.2.5 | 9bbb8ce18c80f099e0e448775f59d965ead40a21 | /tmp/ocmini-parity-20261102/opencode-v2.2.5 |
| pingdotgg/t3code main | 47f2c8d29e981c7723467efe137ee19e91df924e | /tmp/ocmini-parity-20261102/t3code |
| OpenCode v2.0.28, matching T3's client dependencies | 026b106bc5d41b679ffc413264229c340871e4d3 | /tmp/ocmini-parity-20261102/opencode-v2.0.28 |
| OpenCode v1.19.4, our advertised baseline | 148842ef7cb1bc34192b3fff57d0f9721ab9c801 | /tmp/ocmini-parity-20261102/opencode-v1.19.4 |

These /tmp checkouts are disposable. Recreate them with git clone --depth 1 --branch <tag> <repository> <directory>; for T3, fetch/checkout the pinned SHA rather than assuming main is unchanged.

Primary source index:

- [OpenCode v2.2.5 release](https://github.com/anomalyco/opencode/releases/tag/v2.2.5) and [versioned changelog](https://github.com/anomalyco/opencode/blob/9bbb8ce18c80f099e0e448775f59d965ead40a21/CHANGELOG.md).
- [Upstream OpenAPI](https://github.com/anomalyco/opencode/blob/9bbb8ce18c80f099e0e448775f59d965ead40a21/packages/protocol/openapi.json): method/path inventory; protocol groups own payloads and response status.
- [Session protocol](https://github.com/anomalyco/opencode/blob/9bbb8ce18c80f099e0e448775f59d965ead40a21/packages/protocol/src/groups/session.ts), [message protocol](https://github.com/anomalyco/opencode/blob/9bbb8ce18c80f099e0e448775f59d965ead40a21/packages/protocol/src/groups/message.ts), [permission protocol](https://github.com/anomalyco/opencode/blob/9bbb8ce18c80f099e0e448775f59d965ead40a21/packages/protocol/src/groups/permission.ts), and [MCP protocol](https://github.com/anomalyco/opencode/blob/9bbb8ce18c80f099e0e448775f59d965ead40a21/packages/protocol/src/groups/mcp.ts).
- [Public session events](https://github.com/anomalyco/opencode/blob/9bbb8ce18c80f099e0e448775f59d965ead40a21/packages/schema/src/session-event.ts), [message types](https://github.com/anomalyco/opencode/blob/9bbb8ce18c80f099e0e448775f59d965ead40a21/packages/schema/src/session-message.ts), and [event envelope](https://github.com/anomalyco/opencode/blob/9bbb8ce18c80f099e0e448775f59d965ead40a21/packages/schema/src/event.ts).
- [Session semantics](https://github.com/anomalyco/opencode/blob/9bbb8ce18c80f099e0e448775f59d965ead40a21/specs/v2/session.md) and [public event delivery](https://github.com/anomalyco/opencode/blob/9bbb8ce18c80f099e0e448775f59d965ead40a21/specs/v2/event-stream-architecture.md). Exact schemas/code take precedence over illustrative specification snippets.
- [T3 v2 adapter](https://github.com/pingdotgg/t3code/blob/47f2c8d29e981c7723467efe137ee19e91df924e/packages/provider-opencode/src/server/v2/adapter.ts), [v2 client](https://github.com/pingdotgg/t3code/blob/47f2c8d29e981c7723467efe137ee19e91df924e/packages/provider-opencode/src/server/v2/OpenCode2Client.ts), [v2 server](https://github.com/pingdotgg/t3code/blob/47f2c8d29e981c7723467efe137ee19e91df924e/packages/provider-opencode/src/server/v2/OpenCode2Server.ts), and [generation helper](https://github.com/pingdotgg/t3code/blob/47f2c8d29e981c7723467efe137ee19e91df924e/packages/provider-opencode/src/server/v2/textGeneration.ts).
- [T3 dependency pins](https://github.com/pingdotgg/t3code/blob/47f2c8d29e981c7723467efe137ee19e91df924e/packages/provider-opencode/package.json), [version detection](https://github.com/pingdotgg/t3code/blob/47f2c8d29e981c7723467efe137ee19e91df924e/packages/provider-opencode/src/server/versionProbe.ts), [catalog](https://github.com/pingdotgg/t3code/blob/47f2c8d29e981c7723467efe137ee19e91df924e/packages/provider-opencode/src/server/openCode2Catalog.ts), [driver](https://github.com/pingdotgg/t3code/blob/47f2c8d29e981c7723467efe137ee19e91df924e/packages/provider-opencode/src/server/driver.ts), and [usage reader](https://github.com/pingdotgg/t3code/blob/47f2c8d29e981c7723467efe137ee19e91df924e/packages/provider-opencode/src/server/usage.ts).

The companion [endpoint inventory](opencode-v2-endpoints.csv) contains every one of the 169 method/path operations in the pinned v2.2.5 OpenAPI, its operation ID, priority and T3 callsite. Counts come from enumerating path entries with operationId, not counting paths alone:

| Scope proposed here | Operations | Meaning |
| --- | ---: | --- |
| P0-T3 | 34 | Directly called by T3's current provider, including conditional and auxiliary calls |
| P1-headless | 41 | Additional session/runtime management and inspection parity |
| Deferred | 94 | Broader product/infrastructure surface, outside the first migration |

All 169 exact v2 HTTP operations are absent from our current router. This measures wire coverage, not engine capability: many operations have reusable v1 implementations. The priority assignments are recommendations, not upstream classifications.

## What changed

The release is v2.2.5, published October 30, 2026. The CLI package is now @opencode/cli. The v2 implementation separates schema, protocol, core, server, client and CLI packages. Our Rust project should follow the contracts, not reproduce their TypeScript/Effect package graph.

The migration from our 1.19.4 API involves these substantive changes:

| Area | Current ocmini | v2 target |
| --- | --- | --- |
| Discovery | --version reports 1.19.4-ocmini.0.3.5; /global/health; /api/info deliberately 404 | 2.x identification; /api/info with version, pid, urls and paths.tmp |
| HTTP routes | /session, /event, /provider, /question, /mcp | /api/... with separate catalogs, forms and experimental routes |
| Public events | type + properties | id + created + type + data; optional location/metadata; durable envelope on durable events |
| Session placement | directory/path fields | location.directory, project identity, optional subpath; persisted moves |
| Input | PromptRequest parts, model, system; immediately visible admitted user message | text/files/skills; durable inbox item before transcript delivery; model/agent/instructions are session state |
| Running work | Status/idle events; existing durable admission/ownership | Explicit execution, step, retry and terminal lifecycle; active map is process-local |
| History | info/parts records | Typed user, synthetic, assistant, compaction, idle and selection/control records; cursor pagination |
| Permissions | permission/pattern/action | action/resource/effect; per-session requests and replies |
| Questions | question arrays and string[][] replies | Keyed forms and typed answer maps |
| Tools/children | bash and task; metadata.sessionId | shell and subagent; metadata.sessionID; typed tool content and separate UI metadata |
| Rewind | Fork-and-replace implementation | Revert stage/commit/clear on the same session, plus independent fork operation |
| Configuration | provider, agent, command, permission; old compaction keys | providers, agents, commands, permissions; model/settings/variant changes; compaction.buffer and keep.tokens |
| Provider replay | Protocol-specific providerMetadata | Public native fields with provenance-sensitive replay; UI state/metadata remain separate |

The v2.2.5 patch adds percentage compaction buffers, better missing-folder and failed-edit diagnostics, a bash-to-shell fallback, large stdio MCP fixes, an idle-location MCP shutdown fix, a context/input ceiling correction, LiteLLM overflow recognition and reasoning preservation fixes. Desktop shortcuts and quote-composer changes do not require ocmini server work.

Since early v2, the changelog also includes persistent terminals, artifact APIs, schedules, image/decision APIs, stored media, managed policies, GUI plugins and provider SDK work. Those are separate scope choices. They are not prerequisites for the 34 T3 calls.

## What T3 actually uses

T3 supports both generations and labels OpenCode 1.x as limited support. Its documentation lists 2.0.18+ support, but the checked-in dependencies pin @opencode/client, @opencode/protocol and @opencode/schema to **2.0.28**. Its old adapter imports @opencode-ai/sdk/v2; that package subpath does **not** mean the OpenCode 2 server protocol.

### Startup and discovery

T3 runs --version and selects v2 for major >= 2. For an external URL it probes /api/info first, then /global/health. The lightweight /api/info probe needs JSON version and integer pid; the real SDK subsequently requires the full ServerInfo shape. A version bump alone would immediately select an adapter our server cannot satisfy.

For managed mode, T3 still launches:

~~~text
<binary> serve --hostname=127.0.0.1 --port=<chosen-port>
~~~

It parses the existing server-listening banner. It does not invoke service installation, service discovery/election, a process daemon, pairing, or a web UI to run ordinary threads.

V2 managed startup supplies OPENCODE_PASSWORD; upstream prioritizes it over OPENCODE_SERVER_PASSWORD and otherwise generates a password. We currently read only the latter. Keep UTF-8 Basic auth with username opencode and the current launch flags/banner. V2 catalog discovery uses HTTP; the old models --verbose, agent list and debug skill formats are not reasons to retain v1 HTTP.

### Current T3 v1 requests and what we already cover

The pinned T3 v1 adapter and runtime still use the following operations, all implemented by our current router. This is source-level coverage plus the passing local contract tests, not a live run of every T3 feature:

| Current v1 request | v2 replacement or change |
| --- | --- |
| GET /global/health | GET /api/info |
| GET /event | Global GET /api/event and a different event model |
| GET /provider | Separate GET /api/provider and GET /api/model |
| GET /agent, /skill, /command | Location-scoped /api catalogs |
| POST /session | POST /api/session with persistent model/agent/placement |
| GET /session/{id} | GET /api/session/{sessionID} |
| PATCH /session/{id} | PATCH /api/session/{sessionID}, plus separate selection/move operations |
| DELETE /session/{id} | DELETE /api/session/{sessionID}; descendant semantics differ |
| GET /session/{id}/children | Children discovered through native events/session reads; no matching v2 /children route |
| GET /session/status | GET /api/session/active plus execution/outcome data |
| GET /session/{id}/message | Typed, paginated GET /api/session/{sessionID}/message |
| POST /session/{id}/prompt_async | POST /api/session/{sessionID}/prompt; returns admitted inbox item |
| POST /session/{id}/message | Same v2 prompt/events flow, including auxiliary text generation |
| POST /session/{id}/command | POST /api/session/{sessionID}/command with name/text payload |
| POST /session/{id}/abort | POST /api/session/{sessionID}/interrupt plus explicit inbox cancellation |
| POST /session/{id}/summarize | POST /api/session/{sessionID}/compact; async lifecycle |
| POST /session/{id}/fork | V2 fork for copying; staged revert for in-place history rewind |
| GET /mcp, POST /mcp | V2 list plus PUT/delete experimental registration |
| POST /permission/{requestID}/reply | Session-scoped reply using decision |
| POST /question/{requestID}/reply | Session forms with keyed answers |

Our existing diagnostic routes also include pending permission/question lists, question rejection, MCP connect/disconnect, session listing and individual message lookup. They should not all be mistaken for direct calls by the current T3 v1 adapter. Current T3's runtime discovers the v1 provider/agent/skill/command inventory over HTTP too; some CLI-probe descriptions in our existing compatibility document describe older integration expectations.

### Complete direct v2 HTTP set

Paths below were resolved through generated clients and protocol definitions, not inferred from SDK method spelling.

| Method and path | Operation |
| --- | --- |
| GET /api/info | server.info |
| GET /api/event | event.subscribe |
| GET /api/model | model.list |
| GET /api/provider | provider.list |
| GET /api/agent | agent.list |
| GET /api/command | command.list |
| GET /api/skill | skill.list |
| POST /api/session | session.create |
| GET /api/session/active | session.active |
| GET /api/session/{sessionID} | session.get |
| PATCH /api/session/{sessionID} | session.update |
| DELETE /api/session/{sessionID} | session.remove |
| POST /api/session/{sessionID}/agent | session.switchAgent |
| POST /api/session/{sessionID}/model | session.switchModel |
| POST /api/session/{sessionID}/move | session.move |
| POST /api/session/{sessionID}/prompt | session.prompt |
| POST /api/session/{sessionID}/command | session.command |
| POST /api/session/{sessionID}/compact | session.compact |
| POST /api/session/{sessionID}/synthetic | session.synthetic |
| POST /api/session/{sessionID}/interrupt | session.interrupt |
| GET /api/session/{sessionID}/message | session.message.list; T3 calls client.message.list |
| POST /api/session/{sessionID}/fork | session.fork |
| POST /api/session/{sessionID}/revert/stage | session.revert.stage |
| POST /api/session/{sessionID}/revert/commit | session.revert.commit |
| DELETE /api/session/{sessionID}/revert | session.revert.clear |
| DELETE /api/session/{sessionID}/inbox/{inboxID} | session.inbox.cancel |
| GET /api/session/{sessionID}/permission | session.permission.list; T3 calls client.permission.list |
| POST /api/session/{sessionID}/permission/{requestID}/reply | session.permission.reply |
| GET /api/session/{sessionID}/form | session.form.list |
| POST /api/session/{sessionID}/form/{formID}/reply | session.form.reply |
| DELETE /api/session/{sessionID}/form/{formID} | session.form.cancel |
| PUT /api/experimental/session/{sessionID}/instructions/entries/{key} | experimental.session.instructions.entry.put |
| PUT /api/experimental/mcp/{server} | experimental.mcp.add |
| DELETE /api/experimental/mcp/{server} | experimental.mcp.remove |

T3's older client.session.instructions.entry.put and client.mcp.add/remove names already resolve to these experimental paths in 2.0.28. Do not invent /api/mcp registration aliases because the SDK method lacks the word experimental.

Calls cover catalog/health, normal chat, plan mode, model switching, worktree relocation, streaming, commands, manual compaction, steering, stale approval cleanup, interruption, history recovery, rewind, fork, child work, MCP bridge cleanup, and temporary sessions used for title/commit/PR text generation.

T3's v2 text-generation helper deliberately uses create/prompt/events/delete rather than the standalone experimental generate APIs. Implementing those generation APIs is not necessary for that helper.

### Response, input and event details that block compatibility

- Server info is a bare object. Most session reads/creation/fork return an HTTP data envelope; catalogs return location plus data. Update, selection, move, removal, command, approval and form settlement have endpoint-specific no-content responses. The generated Effect client unwraps some envelopes, which is why its callsites sometimes read created.id directly. Copy the HTTP schema, not the resulting JavaScript value.
- Model refs use providerID, id and optional variant. Prompts use id, text, files with uri/name, skills with id, delivery and resume. Selection/instructions must persist without being repeated in every prompt.
- Location-scoped catalog query encoding is location[directory]. V2 /api/event is global across locations; reusing our directory-filtered /event implementation would hide children and other threads. Session-ID operations use saved placement.
- Durable events need actual per-session sequence/version metadata and transactionally consistent projections. Text/reasoning/tool-input deltas are live-only. Reconnect uses history/active/outcome; /api/event does not promise replay.
- T3 subscribes before submitting work. Emit execution start, inbox delivery, step start, text/reasoning start/delta/end, tool input/called/progress/success/failure, step end/failure, retries, compaction lifecycle and exactly one terminal execution outcome in the proper order.
- Preserve assistantMessageID plus ordinal for text/reasoning, and assistantMessageID plus tool id for calls. Tool call IDs alone are not globally unique. Emit all terminal tool outcomes before terminating their step.
- T3 specifically recognizes the tool name subagent. Its running metadata reads sessionID, and it consumes synthetic inbox items as background reports. Merely exposing our existing task tool with old metadata would fail child-thread/background behavior.
- V2 rules use shell and subagent actions. Normalize bash/task legacy configuration before permission evaluation. T3 installs per-thread MCP server names and rules so sibling threads cannot call one another's bridge.
- Permission replies use decision, with optional message. Decline with feedback and cancel are different runtime outcomes. Forms use frm_ IDs, keyed fields and answer maps. T3 can render string/multiselect fields but rejects unsupported numeric/conditional/external forms; our question tool should produce the supported subset.
- Keyed instructions must survive compaction and reverts, update at a step boundary without generating a turn, and be inherited correctly on fork. A successful empty PUT handler is insufficient.
- V2 errors are typed tagged objects from Protocol, not our current name/data.message wrapper. Match error discriminators and HTTP codes so T3 can recognize missing sessions, settled requests, invalid input and conflicts.
- Native usage remains input/output/reasoning/cache with measured provider values. T3 also reads local SQLite history, accepting either message or session_message tables. Our existing opencode-ocmini.db sidecar can remain; changing HTTP generations does not require overwriting upstream opencode.db or duplicating fork costs.

### The 2.0.28 versus 2.2.5 issue

This is a separate compatibility problem from retaining v1. Source comparison establishes:

| Surface | T3's pinned 2.0.28 schema | v2.2.5 schema | Consequence |
| --- | --- | --- | --- |
| Compaction start/end events | Required recent string | recent removed; retained history uses metadata.keepID | Older strict event decoder can reject these lifecycle frames |
| Compaction history records | Required recent string on running/completed records | recent removed; native/metadata carry newer representation | Whole message-list response can fail old schema decoding after compaction |
| Provider replay | Optional state/providerState/providerContext/resultState fields | native, including native.messages | Renamed optional fields may be ignored by old clients; preserve canonical data internally |
| MCP and instruction SDK namespaces | Nonexperimental-looking methods | Experimental namespace plus deprecated aliases | Required HTTP paths are already the same |

T3 has an unreadable-terminal fallback, but it does not reconstruct rejected compaction lifecycle events. Its recorded v2 replay fixtures mostly originate from 2.0.18, so passing those alone cannot prove 2.2.5 compatibility.

**User decision, 2026-11-02:** accept this older-client compaction limitation and proceed with native v2.2.5. Do not implement legacy recent/replay aliases, a compaction compatibility shim or a separate wire dialect to address it. Native automatic/manual compaction remains in scope and must work correctly under v2.2.5.

The accepted limitation includes potentially missing compaction lifecycle displays and failed older-client message-list decoding after compaction, which can affect history snapshots or resume. It is not limited to the started/ended display. These older-client compaction cases are documented exceptions to T3 acceptance gates, not migration blockers. The risk was established by static schema comparison; its actual effect in the user's installed T3 build has not been measured.

## Reuse and gap assessment

| Subsystem and local evidence | Already present | Missing or changed work |
| --- | --- | --- |
| src/server.rs; src/main.rs | Axum, serve flags/banner, auth, directory checks, all contracted v1 routes | v2 router, tagged errors, discovery, auth env precedence, complete wire serializers |
| src/types.rs; src/messages.rs | Sessions, assistant/part storage, usage, model refs | Current domain structs encode v1 names; separate internal types from wire types; typed v2 history/control records |
| src/store.rs; src/engine_impl/admission.rs | SQLite, durable receipts/pending work, duplicate rejection, leases, interruption recovery | Inbox-before-history, delivery modes/control barriers, session outcomes, durable event log and projections, resumable execution claims |
| src/events.rs; src/bus.rs; src/client.rs | Ordered local events, shared payloads, visible lag failures, TUI event reduction | v2 envelopes/lifecycles/global feed and replay log; preserve the TUI and existing hook observers |
| src/engine_impl/model_step.rs; turn.rs | Streaming, cancellation, parallel tools, retries, queued steering, explicit step limits | v2 physical-attempt/step lifecycle, execution boundaries, typed failures/idle projections, exact scheduling semantics |
| src/sessions.rs; src/store.rs | Persistent metadata, real child sessions, fork copy/remap | Durable model/agent changes, location movement, staged revert state; separate fork provenance from task parentage |
| src/permission.rs; src/tools/question.rs | Ordered rules, approvals, workspace checks and questions | v2 names, request/source shapes, feedback versus interruption behavior, forms and saved grants |
| src/tools/task.rs; process.rs | Real foreground/background children and shell jobs | shell/subagent naming; child linkage fields, background report payloads, control lifecycle; background registry APIs |
| src/config.rs; src/inventory.rs | Layered JSON/JSONC, agents/commands/skills/models and variants | Native plural v2 config keys and typed settings; normalize old files; array variants/reasoning capability metadata |
| src/engine_impl/instructions.rs | Ambient instructions and per-prompt system support | Persistent keyed entries, instruction value deltas/baselines, fork/revert/compaction lifecycle |
| src/engine_impl/compaction.rs; history.rs | Actual summaries, retained context, pruning, overflow recovery | buffer and keep.tokens, percentage math/input ceiling, native messages/keepID and typed lifecycle; no old-client compaction encoding |
| src/provider/* | Chat Completions, Responses and Anthropic; opaque/signed replay handling | New public native representation and provenance checks; v2 replay/model-switch/partial-response regression coverage |
| src/mcp/* | Real stdio/Streamable HTTP, cancellation, supplied headers, directory hubs | PUT/delete registration contract, v2 status/catalog shapes, phase timeout config; complete resource/form features for expanded scope |
| src/hooks/*; src/store/hook_* | Validated command hooks, durable terminal subscriptions, completion gate/finalizer | Preserve ordering and exactly-once admission/finalization guards during inbox/step refactor; hook payload versions are independent of OpenCode server versions |
| src/usage_db.rs | Isolated T3-readable usage sidecar | Verify v2 usage attribution and keep copied history from being billed twice |

Local evidence is backed by the existing test suite: cargo test --all-targets --locked passed 293 unit tests and 103 contract tests during this audit. This is a v1/runtime baseline, not a v2 conformance result.

### Patch-level improvements to take while migrating

| v2.2.5 change | Our current status | Planned treatment |
| --- | --- | --- |
| compaction.buffer numeric/percentage; keep.tokens from v2 | Only reserved/preserve_recent_tokens/tail_turns/prune | Add native keys and validation, normalize legacy settings with documented precedence |
| Cap input window at context | CompactionCfg.usable can trust input above context | Use upstream ceiling math; test input > context, tiny/unknown windows, zero and oversized buffers |
| Missing-folder read diagnostics | Generic filesystem error path | Name missing folder and nearest existing ancestor |
| Failed edit hints / already-applied detection | Generic oldString-not-found error | Return useful nearby context without changing edit safety or existing per-path locking |
| bash fallback to shell | bash is already canonical here; shell is absent | Make shell canonical for v2; retain an input alias with identical policy and collision precedence |
| Large stdio MCP messages | Explicit 16 MiB transport cap; thus not the exact upstream 10 MiB failure | Test the upstream >10 MiB case using the existing transport; address any demonstrated parity failure directly without adding or lowering limits |
| Idle MCP shutdown | Lazy per-directory hubs; different lifecycle | Verify shutdown does not trigger reinitialization; keep lazy startup/isolation |
| LiteLLM overflow | Generic string markers; no dedicated ContextWindowExceededError marker | Add the upstream case and test normalization through overflow recovery |
| Reasoning preservation | Already retains signed/encrypted replay; public shape differs | Keep that machinery, test model switches and interrupted response replay; never flatten reasoning into text |

Upstream v2's retry/recovery policy differs from our current behavior: its documented generic budget is one request plus four retries, and overflow recovery is bounded per logical step. Our code permits five retries and repeated shrinking compactions. Preserve our current recovery behavior and expose the required v2 lifecycle events; do not import stricter upstream execution limits merely to match an implementation detail. Document this behavioral difference.

## Scope boundaries

**P0 means T3 behavior with the accepted older-client compaction exception**, not a demo that sends text. All 34 calls plus their streaming, persistence, approval, child, restart and auxiliary text-generation behavior are release gates, except failures specifically caused by that documented schema mismatch.

**P1 means candidate additional headless parity.** The CSV lists the exact 41 operations: session lists and message lookup; inbox inspection/editing; instruction listing/removal; form/request creation and lookup; saved permissions; default models/provider details; MCP list/connect/disconnect/resources; context/diff; wait/log/import/export/stats; location reload; shell/background controls; environment; and limited config inspection/update. These are not a commitment to implement all 41; select them individually based on project needs while preserving the current lightweight approach.

Some P1 features are substantial: filesystem-backed session diffs require snapshots; saved permissions require persistence; imported transcripts require validation; logs require a real durable event store. A 204 stub does not count as parity. File-restoring revert with files=true belongs with snapshot support; T3's files=false history-only flow is required in P0.

**Deferred means an explicit product decision.** The remaining 94 include standalone generation/image/decision/voice, pairing/browser sessions, credential/integration onboarding and OAuth, GUI/plugin management, generic filesystem APIs/watchers, PTYs/process-daemon features, artifacts, schedules, worktree/VCS management, RPC, web search services, debug/migration and hosted usage-limit surfaces. Retain the existing small-binary, three-provider-protocol scope unless a separate feature is selected.

Our own TUI, run command, hooks, background-shell extension and retained tool-output files remain project features. They are not reasons to adopt upstream desktop, hosted billing, npm plugin loading or every service-management feature. Describe release coverage as “T3 OpenCode v2 compatibility, based on v2.2.5,” with listed exclusions; “full OpenCode parity” would be inaccurate.

## V1 compatibility decision

| Option | Benefits | Costs and risks | Assessment |
| --- | --- | --- | --- |
| Keep v1 only, backport selected features | Least immediate churn; all current tests stay directly relevant | T3 limited-support path; no native v2 forms/inbox/instructions/reverts | Does not meet the migration goal |
| Permanently serve both HTTP generations | Older T3 clients and external integrations keep working | Two event models, approval models, admission semantics and error contracts; every future fix needs both tested | Only justified by known continuing consumers |
| Move public HTTP/runtime to v2, preserve data/config migration | One long-term contract; current T3 uses its v2 adapter; smaller maintenance surface | Older clients need the last v1 binary; migration/rollback must be explicit | Recommended target |
| Frozen, opt-in v1 transport adapter over v2 | A bridge for a named old consumer | Still must translate “admitted and visible” v1 prompts, stop semantics and event ordering | Contingency, not the default commitment |

Repository evidence identifies the TUI, CLI, contract tests and T3 as consumers; it cannot prove there are no external scripts or pinned old T3 installations. Nothing traced in current T3 requires us to keep v1 HTTP once v2 is complete. Preserve a tagged final v1 release and explain the supported T3 baseline.

Distinguish three independent commitments:

1. Retaining a v1 HTTP server indefinitely is optional and currently unjustified.
2. Reading existing ocmini sessions and supported old configuration is essential migration work. Upstream itself normalizes v1 config while exposing v2.
3. Our command-hook inline v1/file v2 payload formats are a separate extension API. An OpenCode server migration must not remove either accidentally.

Keep public v1 available on the development branch until gates pass, then switch the default protocol. Remove v1 routes/serialization in a follow-up after a successful release or keep only a specifically required adapter. Do not run two independent engines or dual-write competing transcript databases.

## Implementation sequence and acceptance gates

### Phase 0 — Freeze the native v2.2.5 contract

Deliverable: checked-in conformance fixtures/schema checks using @opencode/client/protocol/schema 2.0.28 and 2.2.5, isolated to development tooling. Record the supported T3 SHA/release. Use T3's recorded transcript scenarios as behavioral references with attribution if copied.

Validate native v2.2.5 compaction event/history decoding, exact response wrapping/status/error shapes, query encoding and auth. Validate T3's older client for the remaining required flows, keeping compaction-schema failures as an accepted exception. Include a request capture test using the actual clients, because fixtures containing sdk.response can mask HTTP envelope errors. No older-client compaction fix or separate dialect is required.

Gate: there is one agreed wire contract for every P0 operation and required event; no dependency update or major-version bump can silently switch T3 into incomplete v2 code.

### Phase 1 — Separate domain state and migrate storage

Likely files: src/types.rs, store.rs, messages.rs, sessions.rs and a new v2 schema module.

Introduce protocol-neutral session/model/permission/tool state; additive inbox, instruction-entry, minimal durable event/projection, revert and execution-claim storage. Reuse the existing database and commit related facts/projections together; keep streamed deltas ephemeral and reuse existing text persistence mechanisms where compatible. Full log-endpoint retention is a separate P1 feature. Preserve original session/message identities needed by T3 native references, tool call linkage, usage, attachments, compaction tails and fork references. Existing IDs are strings with valid prefixes; retain ordering through persisted sequence rather than assuming random legacy IDs sort chronologically.

Provide versioned, transactional, restartable migration on a copy of legacy fixtures. Page/batch conversion within a resumable migration protocol so large histories do not require a full in-memory copy or an unbounded WAL transaction. Back up the existing ocmini DB before the first irreversible schema migration and record completion. Keep the real OpenCode database/config/auth files untouched. Rollback uses the final v1 binary plus the pre-migration DB; new v2 writes are not automatically backwards-readable.

Gate: old sessions resume under the new domain, rerunning migration is harmless, failed migration leaves recoverable state, and usage is unchanged. No fabricated historical event sequence: migrate a baseline projection and begin a documented log boundary.

### Phase 2 — Implement inbox and execution semantics

Likely files: src/engine_impl/admission.rs, turn.rs, model_step.rs, store.rs, engine_impl.rs and events.rs.

Separate admission from transcript delivery; implement steer/queue/resume=false and control-item barriers. Persist session selection/outcome; process active ownership separately from historical execution claims. Add execution/step/physical-attempt/retry lifecycles, durable-before-side-effect tool calls and orphan settlement.

V2 interrupt stops locally owned execution without deleting pending inbox input. Our existing abort cancels queued work, so direct aliasing would be wrong; T3 retracts stranded steers explicitly. Unknown sessions fail, known idle sessions return a truthful no-op. Preserve hook preparation, admission, completion-gate and finalizer ordering.

Gate: duplicates and concurrent prompts make one admission/execution; steering is delivered at safe boundaries; queue order and move/compact barriers hold; interruption reaches providers/tools/forms; restart never directly replays an ambiguous tool mutation. Test claim recovery against upstream semantics while retaining ocmini's multi-process lease protections.

### Phase 3 — Expose discovery, catalogs, history and global events

Likely files: src/server.rs, main.rs, inventory.rs, events.rs, bus.rs and new server/v2 modules.

Implement ServerInfo, password precedence/generation, all catalog read shapes, create/get/update/remove/active/selection/move, cursor history, native DTOs, typed errors and /api/event. Match deletion's descendant behavior and running-session rules to upstream, rather than reusing our v1 conflict contract blindly.

Emit one encoded event feed with independent lag failure and 15-second heartbeats. Serve events across locations, retain per-event placement, filter only requested event types, and keep durable logs separate from the volatile feed. Refactor our TUI/client event reducer or supply an internal adapter so terminal operation does not depend on retaining public v1 routes.

Gate: the v2.2.5 client decodes P0 reads and the older client decodes them outside the accepted compaction exception; T3 managed and external startup see the same models/variants/commands/skills; two workspaces and child events coexist; slow subscribers fail visibly without blocking healthy ones.

### Phase 4 — Complete T3 turn, approval and MCP behavior

Likely files: src/permission.rs, tools/question.rs, task.rs, process.rs, mcp/*, engine_impl/instructions.rs and config.rs.

Implement prompt/command/compact/synthetic/interrupt, per-session permissions/forms, keyed instruction PUT and per-thread MCP PUT/delete. Make shell/subagent canonical and map old names/config deliberately. Emit child metadata.sessionID and the expected background-report payload; retain real child execution, parallel tools and file commit locks.

Implement native plural config forms and model/settings/variant conversions for the three supported provider protocols. Normalize legacy singular keys without rewriting user files, with native-key precedence explicitly tested. Reject unavailable provider packages/protocols rather than dispatching them incorrectly.

Gate: T3 handles normal/plan mode, runtime permission changes, approve/decline/cancel, multiple pending forms, child approvals, sibling MCP isolation, attachment prompts, command callbacks, background completion and temporary generation-session cleanup.

### Phase 5 — Compaction, rewind, fork and recovery parity

Implement buffer/keep.tokens, percentage validation and ceiling math, native/keepID history, session outcome/idle history recovery and provider-native replay rules. Emit native v2.2.5 compaction shapes without a legacy encoding.

Implement stage/commit/clear for files=false on the same session, preserving filesystem state owned by T3 checkpoints; failed/interrupted staged revert cannot leak into the next prompt. Fork uses before as an excluded boundary, copies only settled history, remaps IDs and keepID references, preserves the newest instruction baseline and relocates to the target worktree when asked. Upstream stores fork provenance in fork.sessionID/boundary while leaving task parentID absent; copied history starts with zero newly billed cost/tokens. Follow that distinction rather than treating a fork as a running subagent.

Gate: native v2.2.5 compaction and history snapshots after compaction pass; T3 retry, interrupted/restarted execution, fork, history-only rewind and destination-worktree scenarios pass outside the accepted older-client compaction exception. Explicitly test clear-revert lifecycle against T3's older-client workaround, which waits for execution settlement. Finish patch-level read/edit/MCP/reasoning fixes with focused fixtures.

### Phase 6 — Release v2 and retire v1 HTTP

Run the conformance matrix against a built ocmini binary through T3's actual v2 adapter, not just matching Rust JSON. Cover its recorded simple/tool/skill/command/compaction/inbox/permission/question/interrupt/subagent/background/nested-background/resume/fork/revert cases plus our 2.2.5 schema fixtures. Record older-client compaction-schema failures as the accepted limitation; they do not block release.

Run existing runtime/hooks/storage tests, updated CLI/TUI contracts and standard fmt/clippy/tests/release build. Publish the supported endpoint/exclusion matrix and data/config migration/rollback notes. Only now change CLI/discovery identification consistently to a 2.2.5-based compatibility version and the v2 default. Keep ocmini's Cargo package version distinct from API compatibility.

Gate: all 34 P0 operations and corresponding behavioral scenarios pass except the accepted older-client compaction-schema cases; neither lost events nor failed provider streams can appear as successful completion; existing history/hooks/usage and concurrency behavior remain intact. Preserve the current lightweight implementation without new resource restrictions or hardware-dependent behavior. Publish the compaction limitation alongside the supported T3 baseline. A version string is not the gate.

### Phase 7 — Expand headless parity

Select P1 operations in coherent groups after P0, based on actual demand: inspectable inbox/instructions/requests; saved grants; durable log/import/export/stats; config/location reload; MCP resources; shell registry; snapshots/context/diffs and file-restoring revert. Use the existing lightweight implementation approach for each selected group.

Each group needs meaningful side effects/read models and focused failure/restart tests. Reclassify deferred features only when selected; endpoint counts are an inventory, not an obligation to turn ocmini into upstream Desktop.

## Remaining uncertainty

- Current T3 source is pinned; the user's installed build/release may differ. The plan does not assume this workspace's T3 runtime is built from that exact commit.
- Static schema differences establish a compaction compatibility risk, not a measured failure rate in deployed T3. The user has accepted that limitation; no shim or resolution is required for this migration.
- No inventory of external v1 HTTP consumers exists in this repository. An identified consumer can justify the contingency adapter without changing the v2 core direction.
- Full snapshot/file-revert, provider packages beyond our three protocols, and deferred product features are additional projects. They are explicitly outside the P0 claim.

The original audit preceded implementation. The P0 implementation now advertises v2.2.5, adds state to the existing database and preserves legacy routes during the transition. Verification uses local provider fixtures and official SDK/schema packages; the user's live sessions and installed T3 application are not modified.
