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
