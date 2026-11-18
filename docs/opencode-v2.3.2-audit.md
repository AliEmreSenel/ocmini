# OpenCode v2.3.2 compatibility audit

This audit compares ocmini commit
`f123b3f71d4a499dcdd34af41ee6ac33faa756e2` with the recorded native baseline
[2.2.5](https://github.com/anomalyco/opencode/releases/tag/v2.2.5) and the
versioned v2 tags published through 2026-11-18. GitHub's `latest` release points
at the parallel v1 stream, so it is not a v2 version probe. Enumerated subsequent
tags are [2.2.6](https://github.com/anomalyco/opencode/releases/tag/v2.2.6)
`dc2ffcf8bf26f9e669b6aa22b95f50e12be15e06`,
[2.2.7](https://github.com/anomalyco/opencode/releases/tag/v2.2.7)
`fedc6f208aa2bd54c1019227214568083f6380ad`,
[2.3.0](https://github.com/anomalyco/opencode/releases/tag/v2.3.0)
`a6b15d33daf6ab7d35569f6d711fdb630b9f04e7`,
[2.3.1](https://github.com/anomalyco/opencode/releases/tag/v2.3.1)
`c769de83007e3584b5b395176c9fcde2d3231e88`, and
[2.3.2](https://github.com/anomalyco/opencode/releases/tag/v2.3.2)
`c0ad18b402eb2827fc1ee71c171b1177b3fc02d1`. The v2 tag list was rechecked
after the fixes.

## Protocol findings

Comparing versioned `packages/protocol/openapi.json` files yields 169 operations
at 2.2.5 and 173 at 2.3.2. The ten added operations concern experimental
automation (six), blob retrieval (one), video generation (two), and jobs (one);
the six removed operations are experimental schedule operations. These products
are not automatically part of the selected T3 contract. See the
[OpenAPI comparison](https://github.com/anomalyco/opencode/compare/v2.2.5...v2.3.2)
(and compare `packages/protocol/openapi.json` directly; GitHub may not display
large generated diffs).

The [session schema changes](https://github.com/anomalyco/opencode/compare/v2.2.5...v2.3.2)
include durable `session.step.first-token`, optional assistant `time.firstToken`,
and `generate` as a usage source. `session.generate` now returns tokens and
cost. The baseline endpoint inventory defers generation and the pinned T3 text
helper uses session create, prompt, events and delete. First-token streaming has
a confirmed decoder compatibility issue described below. Freeform tool
capabilities, MCP pooling, provider TLS and background compaction describe
capabilities outside the selected endpoint inventory. The older T3 compaction
schema limitation remains part of that inventory's acceptance criteria.

## Release classification

The release notes for all five tags and the 2.2.5-to-2.3.2 OpenAPI were
reviewed against this branch. This is a compatibility classification, not a
claim that concurrent work has landed or that every upstream product is
implemented. Desktop, TUI, GUI/plugin, ACP, Windows, hosted integrations,
Code Mode, warming, standalone generation, job listing, video, automation,
provider mTLS and arbitrary provider packages are outside the selected T3
contract. The 2.3.0 change to always-background subagents and 2.3.1 default
background compaction are substantial behavior changes and remain explicit
parity gaps, not implied prerequisites for the existing 2.2.5 contract.

| Release | Selected runtime changes and disposition |
| --- | --- |
| 2.2.6 | This PR fixes webfetch fragment matching and adds browser request-site and raw-download protections. The provider transport retries connection errors. The external-directory approval applies to upstream's model-facing `session_move` tool. Standalone run background settlement remains a CLI difference. Partial provider limits already use JSON. Warming, MCP pool, worktree CLI, archive CLI, Azure Messages and VM are outside scope. |
| 2.2.7 | Read, shell output, and deferred MCP have separate feature PRs. This PR fixes title model variants, explicit URL precedence, and individual bad legacy credential entries. The runtime does not query the remote catalog for configured models, so partial model overrides can still omit provider metadata. Generation usage and warming are deferred with `session.generate`. |
| 2.3.0 | Always-background subagents, restart recovery and shell handoff remain behavioral differences. Catalog discovery uses saved credentials and parallel lookups through existing provider settings. See the known differences below for retry interruptions and catalog refresh. Freeform tools, TLS, video, Code Mode and the process daemon are outside scope. |
| 2.3.1 | This PR fixes step-scoped rejection, cross-model Chat reasoning replay and Unicode title search. Nested instructions, selective reads and grep, diagnostics, shell output and narrow MCP discovery are separate PRs. JSON panic errors, automatic shell handoff and background compaction remain different. Automations and Code Mode are outside scope. |
| 2.3.2 | Responses caching is PR #68 (OpenAI and Copilot); other upstream provider routes are outside the selected provider scope. Webfetch structured output is a Code Mode/plugin interface, deferred. |

### First-token and replay

The 2.3.2 schema defines durable `session.step.first-token` with session and
assistant message IDs, and adds optional assistant `time.firstToken`. ocmini's
`v2/events.rs` sends durable `session.step.streamed` at step finish. The latest
first-token event remains a native 2.3.2 parity gap. The actual OpenCode 2.2.5
and T3 2.0.28 protocol packages in the conformance fixture both reject a
`session.step.first-token` envelope at its `type` field when decoded via
`Schema.decodeUnknownSync(OpenCodeEvent)`. Emitting it on the shared stream
would fail the pinned clients' conformance test. The pinned T3 stream remains
the selected compatibility target.
Upstream publishes the event once per physical attempt before the first
text-start, reasoning-start, tool-input-start, tool-input-error, or tool-call;
its message updater projects the event creation time into `time.firstToken` and
clears that field on step start. Correct implementation needs the equivalent
persistence and ordering, plus tests across restart and empty/failed streams.
ocmini receives text/reasoning deltas and tool-call starts in `model_step.rs`,
but its current v2 event sender only publishes while subscribers are present,
so emitting there alone would not make the history timestamp durable. Emitting at step finish or
fabricating historical timestamps would be incorrect. This has not been
implemented or end-to-end tested here.

### Rejection scope

Before this PR, a v2 rejection without feedback interrupted the session and
invoked `reject_all`. The new v2 reply path settles pending requests sharing the
selected request's session and assistant message ID under one lock. Other
steps and child sessions keep their own requests. The v1 reply remains on its
existing path. The scoped unit test and parallel-tool v2 contract cover
these boundaries and continuing with the next step.

### Provider migration and replay

`v2/config.rs` already splits inline `#variant` in global, agent and command
selections, merges native provider settings into legacy options, and keeps
model `limit` as JSON. This PR picks up inline variants in `small_model` and
`title_model`, uses a nonempty explicit `options.baseURL` ahead of legacy
`api`, and reads valid entries from OpenCode's `auth.json` even when another
entry fails validation. Credential values stay out of the warning. No
credential migration writes to the source file.

Configured models skip remote catalog discovery. When a configured model
leaves a capability unspecified, the runtime therefore has no remote value
to merge. This remains a difference from upstream's catalog behavior.
`provider/openai_compat.rs` previously replayed all saved reasoning fields
for any `openai-chat` provenance. This PR filters incompatible saved reasoning
before merging Chat history, retains same-model signed reasoning and existing
legacy behavior when provenance is absent, and honors supported destination
reasoning field configuration. Focused tests cover the preserved tool linkage,
prompt-derived legacy provenance, and known versus unknown destination fields.

### HTTP and search

V2 API routes and the optional web API now reject cross-site browser
navigations and disallowed origins identified by Fetch Metadata. The optional
web editor's raw downloads now carry attachment, sandbox and no-sniff
headers. The official OpenCode file and blob routes are outside the selected
endpoint inventory. The user-facing HTTP session move continues to accept a
directory from T3, and ocmini has no model-facing `session_move` tool.
[Request-site and download changes](https://github.com/anomalyco/opencode/pull/62422)
· [External-directory permission](https://github.com/anomalyco/opencode/pull/62204)

V2 session listing now matches single-character upper and lower case forms
in SQLite, including Cyrillic and accented letters. Existing `%` and `_`
wildcards and cursor filters keep their meaning. The legacy session list and
TUI picker already normalize Unicode case.
[Unicode title search](https://github.com/anomalyco/opencode/pull/64259)

### Known behavioral differences

- Upstream's CLI waits for background jobs and processes their results before
  exiting. ocmini's one-shot `run` follows the admitted turn and stops
  runtime-owned shell jobs at shutdown.
  [Standalone run settlement](https://github.com/anomalyco/opencode/pull/61911)
- The provider transport marks connection failures retryable and the model
  loop retries failed steps with no tool calls. ocmini can clear partial text
  while retrying an incomplete step. Equivalent interruption handling for
  every upstream provider stream remains unverified.
  [Unreachable provider retry](https://github.com/anomalyco/opencode/pull/61880)
- The existing runtime supports foreground and explicitly background child
  tasks. Background delivery uses a task in the originating process.
  Startup finalizes interrupted turns through the persisted hook recovery
  path. Upstream resumes previously admitted and directly driven child work
  and can report completion after a restart.
  [Background subagent lifecycle](https://github.com/anomalyco/opencode/pull/63382)
  · [Upstream recovery cases](https://github.com/anomalyco/opencode/blob/v2.3.2/packages/core/test/session-execution.test.ts)
- Local provider discovery uses the same credential-keyed cache and timeout
  settings as other providers. One catalog request awaits its concurrent
  lookups, and configuration reload has no dependency on the discovery
  tasks. Upstream's provider-specific refresh behavior remains different.
  [Local catalog refresh](https://github.com/anomalyco/opencode/pull/63700)
- Ordinary server errors already return typed JSON. An arbitrary handler
  panic has no v2 JSON `UnknownError` recovery wrapper.
  [Server error response](https://github.com/anomalyco/opencode/pull/64022)
- The existing shell timeout and compaction contracts still use ocmini's
  runtime behavior. Incremental shell polling is a separate PR, while
  automatic timeout-to-background handoff and speculative summarization
  remain distinct upstream behavior.
  [Shell timeout handoff](https://github.com/anomalyco/opencode/pull/63699)
  · [Background compaction](https://github.com/anomalyco/opencode/pull/64290)

### Validation against main

The scoped fixes were validated after integrating the context inspection and
focused compaction changes from main. Context inspection and model requests
now use the same model-aware history projection. The contract suite and the
explicit OpenCode 2.2.5 and T3 2.0.28 SDK conformance test passed, as did
workspace Clippy. These checks cover the selected T3 contract. The native
v2.3.2 differences above remain applicable.
