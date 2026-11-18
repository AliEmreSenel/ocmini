# OpenCode v2.3.2 compatibility audit (in progress)

This is a preliminary audit, not an acceptance report. It compares ocmini commit
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
`c0ad18b402eb2827fc1ee71c171b1177b3fc02d1`. Recheck tags before review.

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
cost; it was already deferred in the baseline endpoint inventory and T3's pinned
text helper uses session create/prompt/events/delete instead. First-token event
and replay behavior still need an end-to-end compatibility assessment; ocmini's
`v2/events.rs` currently emits `session.step.streamed` but no first-token event.
Other schema changes include optional freeform tool capabilities, MCP pooling,
provider TLS and background compaction. Their T3 impact needs further checking.
The accepted older-client compaction schema limitation remains unchanged.

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
| 2.2.6 | Webfetch fragment matching fixed on this branch; initial credential rejection exists. MCP connection behavior is covered by concurrent narrow MCP work. External-directory approval for session moves, browser-origin/download restrictions, noninteractive run background settlement and provider unreachable retry require separate verification. Partial provider limits are represented as JSON, so are not rejected by deserialization. Warming, pool, worktree CLI, archive CLI, Azure Messages and VM are optional. |
| 2.2.7 | Read, shell output, and deferred MCP have separate feature PRs. This PR fixes title model variants, explicit URL precedence, and individual bad legacy credential entries. The runtime does not query the remote catalog for configured models, so partial model overrides can still omit provider metadata. Generation usage and warming are deferred with `session.generate`. |
| 2.3.0 | Subagent always-background/report/restart semantics remain a larger gap requiring behavioral tests. Shell handoff/output and narrow MCP changes are concurrent. Provider retry/interruption and local catalog refresh warrant targeted verification; freeform tools, TLS, video, Code Mode and process daemon are optional. |
| 2.3.1 | This PR fixes step-scoped rejection and cross-model Chat reasoning replay. Nested instructions/read/glob/grep, diagnostics, shell output and narrow MCP are separate PRs. Unicode title search and JSON panic errors need targeted verification. Background compaction is a larger optional behavior. The accepted older T3 compaction schema limitation remains. Automations and Code Mode are deferred. |
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

### Remaining acceptance work

1. Verify the 2.2.6 move/origin and run/retry behavior, 2.3.0 child recovery,
   and 2.3.1 Unicode search/server error behavior against the selected scope.
2. Integrate and validate the separately owned changes and run the existing T3
   acceptance matrix; do not assume their assignment means they have landed.
