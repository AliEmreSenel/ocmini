# OpenCode v2 validation

The v2 implementation is checked against local provider/MCP fixtures and the official OpenCode v2.2.5 and T3-pinned v2.0.28 clients/schemas. It has not been exercised through the installed T3 graphical application or on a physical single-core/500 MB/SD-card device. Native compaction retains the accepted older-T3 schema exception.

## Correctness

- Rust unit and contract tests cover native streaming, usage notifications, selections, instructions, input admission/cancellation, interruption, question forms, approval feedback, foreground/background children, compaction success/failure, history, forks and directory moves.
- Legacy history remains readable, hook vetoes remain effective, and clearing a revert wakes real pending work.
- Session listing pages in SQL with stable timestamp/ID ordering, forward/backward cursors and directory/project/search/parent/archive filters. Project-relative subpath filtering is explicitly rejected because ocmini does not model upstream project-relative workspaces.
- Archive changes persist in the existing session record; repeated archive-only requests are no-ops. Native metadata, permissions, archive and usage events describe their actual changes.
- Event filters accept repeated query parameters and inspect the shared encoded frame's type without parsing JSON for every subscriber.
- Resource regressions have focused tests: saved session IDs are not loaded into an event cache at startup; idle execution indexes are released; an unobserved execution does not advance event counters; cached sequence IDs do not write SQLite; sibling reservations and restart invalidate old ranges; range exhaustion obtains more IDs; a blocked session event lock does not block another session's event processing.
- Official-client conformance includes catalog/history/event decoding, tools, forms, permissions, archive/list operations and the version-2 deletion event. T3's pinned Effect client also creates root/child sessions, updates a title and completes a prompt with omitted optional fields encoded as null.

## Event persistence and memory

The event bridge uses a shared registration snapshot instead of allocating a bridge list for every bus event. Only active executions retain streaming indexes. Session locks serialize that session's facts; the shared session-map lock does not cover database operations or encoding.

The live feed has no replay log. It does not allocate sequence IDs or serialize frames without listeners. Required session state and transcript updates still persist without listeners. Active observed executions reserve 4,096 sequence IDs at a time; unused IDs can be skipped after idle/restart. This is ID allocation, not a cap: exhausting the range obtains another immediately and never truncates events, limits work or throttles concurrency. A read checks the persisted high-water mark so a sibling writer invalidates an older range. Idle API notifications reserve only their required ID. Lifecycle/control records and their session-state changes share one transaction.

There are no new runtime dependencies, helper services, resource caps, hardware probes or alternate machine-specific behavior.

## Focused local resource comparison

Compared the original PR head `4383bbc` with the resource cleanup, using the same release toolchain and x86_64 Linux host. Each fresh temporary server completed one prompt containing 120 local read tools followed by a final answer, with a local provider fixture. Both binaries ran from the same temporary filesystem. Timing/RSS used three untraced runs; a separate strace run counted SQLite write syscalls across startup, session creation, execution and shutdown. All 120 tools completed in every case.

| Measure | Original PR head | Resource cleanup |
|---|---:|---:|
| SQLite `pwrite64` calls, live subscriber | 3,949 | 2,969 |
| Bytes returned by those writes | 8,227,168 | 6,220,584 |
| Sync calls, live subscriber | 17 | 17 |
| SQLite `pwrite64` calls, no subscriber | 3,950 | 2,963 |
| Persisted sequence high-water, no subscriber | 496 | 0 |
| Untraced process CPU per fixture | 0.15 s | 0.14 s |
| Untraced elapsed range | 0.140–0.152 s | 0.135–0.168 s |
| Idle heap + anonymous RSS | 2,332–2,356 KiB | 2,328–2,340 KiB |
| Sampled peak total RSS | 19,180–19,952 KiB | 23,912–24,436 KiB |

The write reduction is about 25%. These are logical write syscalls on a local filesystem, not measured physical SD-card wear or latency. The short timing samples do not establish a general throughput improvement.

Total RSS is approximately 4 MiB higher in this build. Mapping inspection attributes that increase to resident executable-file pages; idle heap/anonymous memory is essentially unchanged. This is recorded overhead, not a claim of zero memory regression. No allocator or linker redesign was introduced to disguise it.

With 20,001 saved v2 sessions, two additional startup samples took 0.068–0.075 s before and 0.068–0.075 s after. The event cache no longer loads those saved IDs, but total startup RSS was still about 1–1.4 MiB higher due to the executable mapping difference. The structural cache improvement should not be described as a measured reduction in total RSS.

These focused checks validate the changed paths. They are not a mandatory benchmark framework, a whole-system 500 MB test or certification of embedded-device performance.
