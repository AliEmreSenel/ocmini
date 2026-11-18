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

## Release classification to date

| Release | Finding and current treatment |
| --- | --- |
| 2.2.6 | MCP pooling/events and connection behavior belong to the concurrent MCP assignment. Webfetch rejects initial URL credentials already (`crates/ocmini-tools/src/web.rs`); this change removes fragments from permission matching. Browser-origin/file-download restrictions need further server review. CLI worktree/archive, cache warming, Code Mode and upstream product-specific features require scope decisions, not wholesale porting. |
| 2.2.7 | Negative-offset reads, paging and discovery belong to the concurrent read/search assignment; shell output belongs to the concurrent shell assignment; MCP refresh and output handling belong to the concurrent MCP assignment. Legacy provider/config migration fixes still require individual verification. |
| 2.3.0 | Subagent background lifecycle and restart behavior require further verification. Shell handoff/output belongs to the concurrent shell assignment. MCP events/Code Mode belong to the concurrent MCP assignment. Video, mutual TLS and freeform tools are optional capabilities, not proof of a T3 requirement. |
| 2.3.1 | Background compaction belongs to the concurrent compaction assignment; CLAUDE.md fallback, read, glob and grep belong to the concurrent scope-aware search/read assignment; shell timeout/output belongs to the concurrent shell assignment. Permission rejection scope, cross-model reasoning metadata, non-ASCII session search and JSON server errors need separate verification. Automations, Code Mode and desktop/TUI changes are upstream product features. |
| 2.3.2 | Responses prompt caching is owned by the concurrent GPT-6.2 assignment. Webfetch structured output is a Code Mode/plugin interface change; ocmini has no equivalent structured Code Mode surface in the baseline scope. |

The pinned T3 source and all changed schemas, endpoint behavior, migration paths,
and release fixes have not yet been checked exhaustively. Do not treat the table
as a complete classification or infer that an item assigned elsewhere is fixed.

## Additional source-level findings

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
| 2.2.7 | Read offsets, diagnostics and paging; shell output; MCP refresh/output are concurrent assignments. Config normalization preserves inline model variants and merges native settings with legacy options. Partial capabilities, legacy `api` versus `options.baseURL`, credential import and provider cost/refresh need targeted verification. Generation usage and warming are deferred with `session.generate`. |
| 2.3.0 | Subagent always-background/report/restart semantics remain a larger gap requiring behavioral tests. Shell handoff/output and narrow MCP changes are concurrent. Provider retry/interruption and local catalog refresh warrant targeted verification; freeform tools, TLS, video, Code Mode and process daemon are optional. |
| 2.3.1 | Nested instructions/read/glob/grep, diagnostics, shell output and narrow MCP are concurrent. Rejection scope and cross-model reasoning replay are unresolved below. Unicode title search and JSON panic errors need targeted verification. Background compaction is a larger optional behavior; the accepted older T3 compaction schema limitation remains. Automations and Code Mode are deferred. |
| 2.3.2 | Responses caching is PR #68 (OpenAI and Copilot); other upstream provider routes are outside the selected provider scope. Webfetch structured output is a Code Mode/plugin interface, deferred. |

### First-token and replay

The 2.3.2 schema defines durable `session.step.first-token` with session and
assistant message IDs, and adds optional assistant `time.firstToken`. ocmini's
`v2/events.rs` sends durable `session.step.streamed` at step finish but has no
first-token emission; its history projection has no first-token timestamp. The
pinned T3 adapter at `47f2c8d29e981c7723467efe137ee19e91df924e` handles
step started/ended/failed and does not reference first-token. Thus this is a
native 2.3.2 event/history parity gap, not evidence of a pinned T3 regression.
Correct implementation requires recording the first actual provider output once
per physical attempt, retaining its timestamp in the durable replay/projection,
and tests across restart and empty/failed streams. Emitting at step finish or
fabricating historical timestamps would be incorrect. This has not been
implemented or end-to-end tested here.

### Rejection scope

`v2_http.rs` replies to the selected request and interrupts the whole session
when a rejection has no message. Session interruption invokes `reject_all` for
that session. This differs from 2.3.1's same-step rejection scope. A rejection
with feedback instead returns an error to that tool. The pinned adapter tracks
permission replies by request ID. Fixing the no-message case needs a step-scoped
cancellation/settlement path, including parallel tool requests and children;
simply removing the interrupt would leave sibling requests pending. This remains
an in-scope behavioral blocker and requires regression tests.

### Provider migration and replay

`v2/config.rs` splits inline `#variant` in global, agent and command selections,
merges native provider settings into legacy options, and keeps model `limit` as
JSON. It does not establish that partial capability overrides preserve the
catalog defaults, or that legacy `api` and explicit `options.baseURL` resolve
correctly in every combination. Legacy OpenCode `auth.json` import is distinct
from preserving ocmini's own credentials and is not established by these checks.
`provider/openai_compat.rs` replays all four saved reasoning fields for any
`openai-chat` provenance; it does not distinguish the destination model's
reasoning field. Cross-model replay can therefore send `reasoning_details` to
a model expecting `reasoning_content`. A fix needs destination capability and
provenance tests without discarding same-model signed reasoning.

### Remaining acceptance work

1. Exercise durable first-token replay and history semantics, or explicitly
   retain the native 2.3.2 gap in the supported contract.
2. Implement and test same-step permission rejection without cancelling other
   steps; verify child and parallel requests.
3. Verify and fix cross-model reasoning-field replay and the listed provider
   migration cases with actual catalog/config fixtures.
4. Verify the 2.2.6 move/origin and run/retry behavior, 2.3.0 child recovery,
   and 2.3.1 Unicode search/server error behavior against the selected scope.
5. Integrate and validate the separately owned changes and run the existing T3
   acceptance matrix; do not assume their assignment means they have landed.
