# Validation

Requires Rust 1.85+ and a C compiler for bundled SQLite. Dependencies are pinned in `Cargo.lock`.

## Checks

```sh
cargo fmt --all -- --check
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo test --all-targets --locked
cargo build --release --locked
```

CI runs formatting, Clippy and all unit/contract tests on stable for pull requests and pushes to `main`, with warnings treated as errors. Pushes to `main` also run `cargo +1.85.0 check --all-targets --all-features --locked` to check minimum compiler compatibility. Rust dependencies and build outputs are cached; only `main` saves caches, which PRs can restore. Superseded runs are cancelled. Feature-branch pushes, tags, and changes limited to Markdown or `validation/` reports do not trigger runtime CI. The workflow can also be run manually, including the minimum compiler check.

Optimized release builds (fat LTO) run only in the release workflow on `v*` tags, which builds the distributed binaries. Before tagging, run the release build locally with the command above. To check the minimum compiler locally, install it with `rustup toolchain install 1.85.0 --profile minimal` and use `cargo +1.85.0`.

Checked on Linux on 2026-10-17: all 100 tests and Clippy passed on Rust 1.85.0 and stable. The four process tests also passed 25 consecutive runs. Pseudo-terminal checks verified history and context reuse, saved model/agent/directory, model overrides, Unicode, terminal restoration and immediate resume after exiting during a stalled request.

## Test layout

- Unit tests stay inline in their implementation modules. MCP retains its existing `src/mcp/tests.rs` module.
- `tests/contract/`: CLI, sessions, streaming, permissions, commands, compaction, provider failures and MCP, with shared local fixtures in `support.rs`.

Tests use temporary state, bundled SQLite and local HTTP/MCP peers. They cover prompt admission, streaming order, attachments, cancellation, steering, approvals, child tasks, forks, restart recovery, session resume and compaction. Process tests allow bounded time for descendant termination after SIGKILL.

## Resource checks

Profiled on Linux on 2026-10-18 against commit `28a5fe3`, using the same release profile and Rust 1.101.0-nightly compiler. After rebasing onto `059d67b` (the clipboard-image change), the resource changes pass all 127 tests (100 unit, 27 contract), formatting and Clippy. Tests cover limited history ordering, session isolation, unchanged saves and journal reclamation; the contract suite also exercises cancellation, steering, child tasks and MCP on the default current-thread runtime. A server health probe with `TOKIO_WORKER_THREADS=2` confirmed that the multithreaded override still works.

Local synthetic measurements use the median of three fresh processes per build. A session with 2,000 messages containing 8 KiB of text received ten single-message and ten `limit=1` reads: elapsed time fell from 0.993 seconds to 0.008 seconds, and process peak resident memory at that point fell from 56.1 MiB to 11.8 MiB. A local provider then streamed 8,192 chunks of 256 bytes into a separate session: CPU time fell from 1.74 seconds to 0.08 seconds, and elapsed time fell from 1.79 seconds to 0.096 seconds. Retained session WAL size after this workload fell from 17.5 MiB to 1 MiB; this measures the journal, not the transcript database. Default idle thread count fell from 13 to 1 on this 12-core host. Blocking-pool and MCP threads may still start when needed.

Idle resident memory remained in roughly the 8–10 MiB range, and executable size remained about 11.2 MiB. These measurements establish improvements for this workload, not universal performance ratios; checkpointing can retain a larger journal while readers or active transactions prevent reuse.

## Multi-turn tool profiling

On 2026-10-18, PLLM5's configured Qwen model completed a live 12-turn conversation with 32 model requests and 27 tool executions across 11 tools: read, glob, grep, edit, write, apply_patch, bash, todowrite, webfetch, question and skill. The disposable project included unit tests, Unicode text, 300 small files and a 400,000-line log (28 MB). The workload also exercised a missing-file error, a shell exit code of 7, output truncation, automatic question replies and conversation recall. Two SSE subscribers and health requests every 100 ms ran throughout. The application stayed responsive and reported no SSE errors. The second live run with buffered scans completed another 12 turns, 34 model requests and 29 tool executions. Peak RSS fell from 41.5 MiB in the first live run to 16.2 MiB in the second; health-request p95 latency was 1.57 ms and 1.22 ms respectively, with no SSE errors. Provider waiting and model call counts differed, so live elapsed times are not used to claim a speedup. Across both live runs, all 24 turns, 66 model requests and 56 tool executions completed; the only tool error was the deliberately missing file in each run.

This exposed whole-file allocations in read and grep. Both now use standard buffered I/O and reuse a line buffer, preserving complete-file validation, line numbering, counts, output limits and cancellation. Memory scales with the longest line rather than total file size, subject to the existing 32 MiB file limit. Regression tests cover CRLF, Unicode, offsets, long-line regex matching, invalid data beyond the requested output, binary files and cancellation.

For controlled comparisons, the recorded Qwen SSE responses were replayed without provider delays against the original build, the previous optimization and the buffered-scanning change. Three fresh-process runs per build produced median peak resident sizes of 46.4 MiB, 39.4 MiB and 16.0 MiB respectively. The last change reduced median ocmini CPU time from 0.27 to 0.25 seconds and replay wall time from 0.469 to 0.446 seconds; these small timing differences are less decisive than the memory reduction. Reaped tool subprocesses consumed roughly another 0.14–0.15 CPU seconds. Metrics came from /proc status, stat and smaps_rollup; remote model inference is excluded.

Controls using MALLOC_ARENA_MAX=2 on the previous build left peak RSS at 39.4 MiB. The new build with TOKIO_WORKER_THREADS=2 used 17.7 MiB versus 16.0 MiB by default, with similar replay latency. All 15 replays across these five configurations produced identical final file hashes, assistant answers and tool coverage, with no stream or health errors. These controls support the default runtime for this workload; they do not establish behavior under a 40-request load or many concurrent project directories.

## Concurrency, churn and boundary checks

A subsequent load test seeded 5,001 sessions and a 300-message transcript of roughly 1.2 MiB. Each configuration ran in three fresh release processes. Before session-list pagination was fixed, `/session?limit=20` still decoded every session and SQLite sorted the complete result. Reading matching rows only until the page is full, backed by an index on time_updated, reduced serial-list p95 latency from 10.19 ms to 0.55 ms. For 40 simultaneous requests, unfiltered-list p95 fell from 312.8 ms to 13.2 ms, and directory-scoped root-list p95 fell from 461.5 ms to 14.3 ms. The new index occupied 61,440 bytes in the seeded database; EXPLAIN QUERY PLAN confirmed that it removed the temporary sorting B-tree. Contract tests cover limits applied after directory, root and Unicode search filters, and rejection of unrelated records before counting page matches.

With the new query, one versus two async workers gave similar scoped-list p95 results (14.3 ms versus 14.1 ms). Forty simultaneous full-history reads still benefited modestly from two workers (173 ms versus 157 ms p95) and reached around 55 MiB RSS. The blocking pool reached 41 threads in the default runtime during that burst: a single async event loop does not impose a one-thread limit on blocking jobs. No health request failed in these load tests.

A separate glibc allocator control under the same 40-request burst did show a benefit: MALLOC_ARENA_MAX=2 reduced median peak RSS to 25.3 MiB across three runs, compared with roughly 55 MiB using the default allocator policy. One idle follow-up waited 12 seconds for all blocking workers to retire: default RSS remained 54.2 MiB with one thread, while the arena-limited control retained 24.9 MiB. This differs from the single-session conversation, where the setting had little effect. It was tested as an existing deployment environment variable; no allocator policy or new application setting was added.

A churn test created, admitted a 1 KiB no-reply prompt to, and deleted 2,000 sessions in one process. Both directory-scoped SSE subscribers received all 8,000 lifecycle/message events; all six session-related tables were empty after each batch. RSS rose from 11.5 MiB initially to 12.7 MiB after all 2,000 cycles, with one thread and an 80 KiB session database. This checks the same-directory workload; it does not establish bounded cache or MCP resources across unlimited distinct directories.

Fourteen tool boundary cases matched the original build's outputs, metadata and errors: empty files, UTF-8 crossing an I/O buffer boundary, CRLF, invalid/binary tails beyond a requested window, a 32 MiB single line, full-line regex matching, oversized files/media, media attachments, last/out-of-range offsets, and 16 MiB shell output. The shell output remained bounded below 52 KiB with correct omitted-byte accounting. Cancelling a 400,000-line read when its running event arrived reached idle in 1.26 ms versus 18.95 ms in the original build in this check. A maximal single line still used about 43 MiB peak RSS; the buffered-read improvement applies most strongly to files with ordinary line lengths.

## Long sessions and repeated compaction

The next comparison isolates context-loading and request-copy changes on top of the preceding resource improvements, before verbatim recent-message retention was added. Both executables used an isolated Cargo release target to avoid interference from builds in other checkouts. A local HTTP/SSE provider drove 20 turn/summary operations, including two explicit and two automatic compactions. Each case ran in three fresh processes; reported values are medians. Provider request sizes, summary flags, final user content and inclusion/exclusion of archived payloads matched across every paired run.

With 500 saved tool results of 200 KiB each and five existing summaries, peak RSS fell from 114.7 MiB to 13.5 MiB, CPU time from 5.73 seconds to 0.10 seconds, and wall time from 5.908 seconds to 0.146 seconds. Previously each turn decoded the entire saved transcript before discarding the portion covered by a summary. Context loading now examines message headers, selects the latest completed, successful, nonempty summary, and reads full payloads only from that point onward. Queued prompts and their responses retain their existing eligibility rules. All 500 archived results remained in SQLite; browsing and forks still retain the complete transcript. This reduces repeated reads and allocations, not archive storage.

Model fallback and acknowledged-prompt retries also previously loaded all message parts. Reading metadata for model selection and fetching only the retried prompt's latest response reduced peak RSS in that case from 217.8 MiB to 13.5 MiB. Twenty acknowledged retries fell from 5.584 seconds to 0.011 seconds without making new provider requests; total CPU including the preceding 20 turn/summary operations fell from 16.53 seconds to 0.10 seconds.

Requests now borrow the constructed context, reuse it on retries, move constructed message arrays into protocol bodies, and release the request before tool execution. With 100 initially active 200 KiB results and no existing summary, peak RSS fell from 129.7 MiB to 95.0 MiB and total CPU from 1.35 seconds to 0.31 seconds. The fixture deliberately accepts a roughly 20 MB request; this does not establish that a real 32k-token model accepts that input.

A separate three-run check with 1,000 tiny tool results and no initial summary measured the first four uncompacted turns at 131 ms before versus 148 ms after in total; peak RSS across all 20 operations fell from 29.1 MiB to 23.6 MiB. Individual indexed message reads have a small cost here (about 4 ms per turn). A single larger check with 10,000 tiny results measured those first four turns at 1.288 seconds versus 1.247 seconds and peak RSS at 198.1 MiB versus 121.8 MiB. No extra query fast path or context cache was introduced for this small timing difference. Header scans still scale with message count, and large active outputs, tool-call inputs such as patches, and media still consume context until compaction; these results do not imply a fixed process-memory ceiling. Unknown model context windows still rely on overflow recovery unless a limit is configured.

Live PLLM5/Qwen verification completed another 12 user turns, two explicit compactions, 37 model calls and 30 tool executions across 11 tool types, with two SSE subscribers. Peak RSS was 15.2 MiB and health-request p95 was 1.13 ms, with no stream or health errors. The only tool failure was the intended missing-file check; final file checks and project tests passed, and the final answer recalled the guide rule across compaction and the later question choice. The configured PLLM5 model already has a context limit, so automatic compaction is enabled without changing user configuration.

Profiling also reproduced stalled automatic compaction ignoring abort while awaiting provider output. Cancellation now interrupts both opening and reading the summarizer stream. Contract tests cover proactive and overflow-triggered compaction, require idle within one second of abort, verify that no partial summary was saved, and complete a subsequent turn. Repeated-compaction tests verify that requests use the latest summary while archived history remains browsable.

## Large tool output files

Tool text now has a shared 50 KiB preview budget, plus truncation/path markers. Larger results are saved to private files under the ocmini data directory, and only their beginning/end preview and file path enter the conversation. Web and MCP text is saved before the former 200 KiB truncation; existing read/search paging, individual-line limits and media handling remain in effect. Attachments and other tool metadata are preserved. Saved paths use the existing file/tool permissions when read later.

Shell stdout and stderr go directly to anonymous temporary files rather than through custom stream buffers. After exit, one blocking job reads at most 64 KiB from each stream and saves oversized results with the existing stderr label and exit-status annotation. Smaller results leave no saved output file. Cancellation and timeout still stop the process group and reap the shell; temporary captures disappear without publishing partial results. No dependencies, output settings or background cleanup workers were added. Output files older than seven days are removed at startup, so a continuously running server retains them until it restarts. Preserving full results uses additional disk space; these files are separate from the transcript database and are not permanent session history.

Unit checks cover Unicode preview boundaries, exact shell bytes around the preview/capture thresholds, recovery of omitted middle content through read, stderr, nonzero exits, failed saves, cancellation and expiry. Contract checks exercise both shell and web results, compare saved content with the original, restart the server, and verify that subsequent model requests and saved tool parts contain only the preview and path.

A controlled comparison used three fresh release processes per case. Across eight turns fetching a 2 MiB text result each time, stored tool-output text fell from 1.56 MiB to 0.39 MiB and total provider request bytes from 13.50 MiB to 3.46 MiB. CPU time fell from 0.12 seconds to 0.05 seconds, with wall time from 0.199 seconds to 0.086 seconds; peak RSS was 20.0 MiB before and 20.8 MiB after. The full results occupied another 16 MiB in eight output files, each verified byte-for-byte. A single 16 MiB shell result was preserved in full with roughly 50 KiB of saved context: CPU time remained 0.01 seconds, wall time rose from 0.019 seconds to 0.032 seconds, and peak RSS from 10.5 MiB to 13.5 MiB. Eight small shell results produced identical provider request sizes and saved text, no retained output files, and batch times of 77 ms versus 67 ms; peak RSS rose from 11.1 MiB to 14.2 MiB. These checks show the context/storage tradeoff and a small resident-memory increase. All health probes succeeded. Fixtures used tmpfs-backed temporary storage, so these times do not measure physical-disk throughput or remote inference.

The output-file implementation also completed a live PLLM5/Qwen conversation with 12 user turns, two explicit compactions, 39 model calls and 31 tool executions across 11 tool types. Peak RSS was 15.3 MiB, peak thread count two, and health-request p95 1.10 ms. Both SSE subscribers stayed connected without errors; the sole tool error was the intended missing-file check. Final file checks and project tests passed, and the final answer recalled the guide rule across compaction and the later question choice. The large shell result was saved with a bounded preview. These live timings are not a controlled before/after comparison because the model's calls differed.

## Verbatim recent conversation

Manual, proactive and overflow-triggered compaction now keep recent structured messages alongside the summary. Their original roles, text, tool arguments/results and provider metadata are replayed without turning them into summary prose. The target is 60,000 serialized JSON bytes (roughly 15,000 tokens), reduced to roughly a quarter of smaller configured context windows. The latest user message takes priority even if it alone exceeds that soft target. The tail begins at a complete user or assistant message, preserving following tool results as a group; older steps in a long tool turn can be summarized while its user instruction remains verbatim. Large individual prompts and media can still exceed the target.

Older messages feed the summarizer, and the previous checkpoint stays available when that input must be shortened. Short conversations can be both summarized and kept verbatim. The recent messages are stored with the checkpoint's text part, avoiding transcript-wide reloads and a database migration. Subsequent compactions roll older retained messages into the new summary rather than retaining previous checkpoints inside the tail. Existing checkpoints without recent-message data remain readable. The bounded snapshot adds some storage per compaction in exchange for preserving exact recent context.

Overflow recovery also now keeps the answer that finishes after its checkpoint, even when its message row was created before it. The checkpoint records the unfinished response ID, context loading includes that response, and forks remap the ID to their copied message. This prevents a successfully recovered reply from disappearing on the next turn. Contract tests exercise that reply after restart and fork, exact tool-message replay after manual compaction and restart, repeated checkpoints without duplicate recent messages, proactive retention, and cancellation. Unit checks cover the byte budget, large tool arguments, complete call/result groups, Unicode, and latest-user priority.

Three fresh-process runs of the 20-operation long-session fixture, each completing four new compactions, measured median peak RSS of 13.4 MiB and CPU time of 0.09 seconds with roughly 100 MB of archived tool results. All 500 results remained saved and their payloads stayed out of outgoing requests. The saved-model/retry variant peaked at 13.6 MiB and used 0.10 CPU seconds; the initially active 20 MB variant peaked at 75.0 MiB and used 0.24 CPU seconds. These are measurements of the final retention behavior; outgoing requests intentionally differ from the earlier summary-only version. In the active fixture, manual summary requests also benefit from the bounded older-context input.

A final live PLLM5/Qwen run completed 12 turns, two explicit compactions, 37 model calls and 29 tool executions across 11 tool types, with two SSE subscribers. Peak RSS was 15.7 MiB with two threads; health-request p95 was 1.14 ms and no stream or health request failed. The intended missing-file error was the only tool error. Final project tests and file checks passed, and the answer retained the earlier guide rule across compaction and recalled the later question choice.

## Runtime limits and longer conversations

This initial audit stage preceded the requested OpenCode parity follow-up below; its counts and measurements describe that earlier build.

On 2026-10-18, an audit found that proactive compaction could run only once per turn and overflow recovery only twice over the whole turn. Both counters prevented otherwise successful tasks from continuing as their context filled repeatedly. Proactive compaction can now repeat, and the overflow recovery budget resets after a successful model step. Two failed overflow recoveries of the same step still terminate an irreducibly large request. Contract tests reproduce both former failures, verify repeated recovery and retained tool results, and verify that an irreducible overflow stops.

Explicit shell, web and MCP timeouts no longer have additional upper ceilings of ten minutes, two minutes and ten minutes respectively. Defaults and cancellation remain. Tests execute tools with longer budgets, check MCP configuration, and retain the existing timeout/cancellation checks. Full saved tool outputs also no longer expire after seven days by default: that broke references when resuming an old conversation. Expiry is now opt-in through `OCMINI_TOOL_OUTPUT_RETENTION_DAYS`. Tests resume with an eight-day-old full output and cover optional expiry and invalid settings.

Context loading now scans the existing session index backwards to the latest usable checkpoint instead of decoding every archived message header on each round. Eligibility still excludes queued user prompts and their descendants, includes parentless messages, skips empty/failed summaries, and retains the response that finishes after an overflow checkpoint. Existing context, compaction, fork and restart tests cover those rules; an additional check verifies that archived headers are not read. Subagent model selection also now uses the existing metadata-only model lookup instead of loading the parent's complete transcript.

The repeatable local fixture is `tests/stress_conversations.py`. It requires Python 3 on Linux, uses temporary isolated databases and a localhost SSE provider, and needs no credentials. It checks the current user prompt and goal marker in active requests, complete tool call/result pairing, expected persisted message/tool counts, and continuation after restarting the process. The synthetic provider reports an 8,192-token context window. These are runtime and context-transport checks, not measurements of a real model's recall or summarization quality.

```sh
python3 tests/stress_conversations.py /path/to/ocmini --mode turns --sizes 100 500 1000 2000 5000 10000
python3 tests/stress_conversations.py /path/to/ocmini --mode steps --sizes 100 500 2000 5000
python3 tests/stress_conversations.py /path/to/ocmini --mode archive --sizes 1000 10000 100000
```

The corrected release build completed 10,000 turns in one session, saving 20,555 messages including 555 summaries, and completed a single task with 5,000 tool rounds and 312 summaries. Both resumed after restart; all original messages remained saved. Request sizes stayed below 32 KiB in these fixtures, with zero context overflows. The prior build, with only the implicit 100-step cutoff already removed, failed the single-task fixture after 67 model/tool rounds because it exhausted the turn-wide overflow recovery budget.

Three fresh processes per build and archive size each sent 20 new turns after a checkpoint, with 3,072-byte archived text parts. Median per-request latency at 1,000, 10,000 and 100,000 archived messages changed from 4.15, 17.09 and 142.24 ms to 2.55, 2.42 and 2.69 ms. At 100,000 archived messages, median peak RSS fell from 32.70 to 13.60 MiB; CPU for the 20 turns fell from 2.92 to 0.10 seconds. At 1,000 archived messages the new build used more memory, 13.34 versus 11.66 MiB, so this is not a universal memory reduction. All 100,000 archived records remained present, and their payloads were excluded from requests. The tool definitions differ slightly because timeout ceilings were removed; active conversation content is unchanged. Timing includes local HTTP/provider fixture work; CPU and memory cover ocmini, excluding the Python fixture and remote inference.

Full measurements and baseline details are saved in [validation/long-conversation-audit.json](validation/long-conversation-audit.json). The release build, formatting, Clippy and all 141 Rust tests pass on the current compiler. No new context cache, database index, connection pool or global allocator setting was added.

The original audit left 32 MiB text-file and 100,000-candidate search ceilings and a 20,000-line read-page clamp; the parity follow-up removes all three. Media and transport bounds remain: 20 MiB native attachments, 16 MiB read-media attachments, 5 MiB web responses and 16 MiB MCP messages. Search results remain capped at 100. Nesting now has a configurable depth defaulting to eight, and provider retries increase to five. Skill resource listings still stop at 100 entries; instructions are loaded in full.

The existing 50 KiB previews with full output files, buffered scans, 250 ms stream-persistence batching and configurable single-worker default retain their measured benefits. Their tradeoffs remain: saved output consumes disk, stream persistence can lag live deltas until flush, unknown context windows rely on overflow/manual compaction, and high concurrency or many distinct project directories need separate load testing. Automatic cleanup of output files no longer referenced by any retained or forked session would improve disk use without breaking resumed conversations.

## OpenCode parity follow-up

The requested follow-up implements `compaction.auto`, `prune`, `reserved`, `preserve_recent_tokens` and `tail_turns`. Usable context reserves model output, using the upstream input-limit reserve when provided; the default verbatim tail is a quarter of usable context, clamped to roughly 2,000–15,000 tokens. Zero token/turn budgets disable the tail. Explicitly disabled automatic compaction also disables overflow-triggered summaries but leaves manual compaction available. Repeated overflow compaction has no fixed counter; it continues while checkpoints shrink the overflowing request and stops on a failed summary or no progress. Tests complete four recoveries in one model step and cover the irreducible case.

Opt-in pruning follows the upstream protection/minimum thresholds: the latest two user turns, roughly 40,000 tokens of tool results and skill output are protected; older output is marked compacted only when more than 20,000 tokens can be cleared. Context then contains a placeholder, while saved original outputs remain browsable. Contract tests compare enabled/disabled pruning and verify both model context and archived results.

Search now streams the external `rg` process, uses native ripgrep ignore/glob semantics and stops after 100 results plus a lookahead. No candidate-file ceiling remains. Explicit globs may override ignore rules; unignored `node_modules` and `target` directories are no longer forcibly excluded. Parsing retains at most 64 KiB per ripgrep record and 8 KiB of error output, following upstream bounds, and cancellation terminates/reaps the process. CI installs ripgrep explicitly. Unused in-process ignore/globset dependencies were removed without changing remaining dependency versions.

Text operations no longer reject files at 32 MiB. Read pages default to 2,000 lines without a separate maximum requested page, cap each line at 2,000 Unicode scalar characters and each file page at 50 KiB of text, and report the next offset. A reused bounded line buffer validates split UTF-8, CRLF and binary content without retaining an arbitrarily long line. Upstream JavaScript counts UTF-16 code units for line slicing; ocmini preserves complete Unicode scalar characters. Tests cover a paged read beyond a 33 MiB line, a 20,001-entry directory page, byte-page continuity, zero limits and Unicode boundaries.

Web fetch timeouts again default to 30 seconds and clamp larger requests to 120 seconds; the 5 MiB body bound remains. Provider retries now allow five attempts after the initial request, including recovery on the fifth retry. `subagent_depth` defaults to eight, accepts zero to disable children and accepts larger values. Tests run nested tasks at the default depth, zero, two and ten levels. Optional seven-day full-output expiry remains available through `OCMINI_TOOL_OUTPUT_RETENTION_DAYS=7`; unset or zero retains files. Tests check expiry, retention and malformed settings.

Formatting, the release build, Clippy and all 155 tests (114 unit and 41 contract) pass. Both Rust 1.85.0 and the current compiler pass all tests and Clippy with warnings denied. The updated local fixture completed 10,000 turns with 624 compactions and a single task with 5,000 tool rounds and 357 compactions, with no overflows or lost prompts/tool results; both resumed after restart. These counts differ from the initial audit because compaction now reserves output headroom.

The search stress case completed across 100,005 files, returned one grep match and 100 glob results with truncation, and read the final line of a 33 MiB text file. ocmini peak RSS was 13.39 MiB in that case; this excludes the ripgrep child and Python fixture. Archive checks through 100,000 saved messages retained every seeded record, allowed new compaction checkpoints during 20 new turns, and resumed after restart. Full measurements are in [validation/opencode-parity-stress.json](validation/opencode-parity-stress.json); the pinned source comparison is [validation/opencode-limit-comparison.md](validation/opencode-limit-comparison.md).

```sh
python3 tests/stress_conversations.py /path/to/ocmini --mode search --sizes 1000 100005
```

## MCP startup and directory scopes

On 2026-10-18, an isolated stdio fixture compared installed OpenCode 1.19.2 with ocmini before and after lazy MCP initialization. With a launch directory distinct from the requested projects, OpenCode and the fixed ocmini launched zero MCP children at boot, one after the first project's MCP request, still one after a repeated request, and two after a second directory's request. The original ocmini launched one child at boot and accumulated two and three respectively. Fixture logs recorded each child's actual working directory. No user MCP configuration or running server was changed by these checks.

A second comparison used a Git repository, two subfolders, a linked worktree and an unrelated repository. OpenCode reported the same project ID for the first four directories, but created a separate MCP child for each requested directory; the unrelated repository had a different project ID. The fixed ocmini matched all five child counts and working directories. This verifies directory-scoped MCP behavior, not full project-metadata compatibility: ocmini still stores `projectID: "global"` and scopes sessions by directory. OpenCode also supports explicit instance disposal; this change does not add that API or automatic eviction to ocmini.

OpenCode's v1.19.2 source confirms two separate keys: [request routing](https://github.com/anomalyco/opencode/blob/v1.19.2/packages/opencode/src/server/routes/instance/httpapi/middleware/workspace-routing.ts) chooses the directory query parameter, then `x-opencode-directory`, then the process working directory; [instance state](https://github.com/anomalyco/opencode/blob/v1.19.2/packages/opencode/src/effect/instance-state.ts) caches MCP state by the resolved instance directory. [Project resolution](https://github.com/anomalyco/opencode/blob/v1.19.2/packages/core/src/project.ts) discovers the Git worktree/common directory and derives the project ID from the normalized origin remote, falling back to the cached common-directory ID or a root commit; non-Git directories use `global`. Repository project IDs therefore do not determine MCP connection sharing.

Contract regressions check that metadata and `noReply` do not start MCP, twelve simultaneous first requests start only one child, launch-directory access still works on demand, project environment overrides and working directories remain isolated, disconnect stays scoped, normalized path aliases reuse a hub, and a first model turn discovers and calls MCP without a prior status request. The model-turn test also verifies reuse on a subsequent turn. Both new tests fail against the original executable: the first detects eager metadata startup, and the second records two children instead of one.

After rebasing onto the current main branch, all 157 tests (114 unit, 43 contract), formatting, Clippy with warnings denied, and the locked release build passed. The three MCP contract tests also passed against the final release executable.

## Shell background jobs and database startup contention

Two harness defects were reproduced, fixed, and checked against OpenCode v1.19.2 (`packages/opencode/src/tool/shell.ts`, `packages/core/src/tool/bash.ts`, `packages/core/src/background-job.ts`, `packages/opencode/src/session/run-state.ts`).

`bash` killed its whole process group when a command exited, so ordinary backgrounding did not survive the tool call. Measured in this repository's own shell tool: `cmd &`, `nohup cmd & disown` and a backgrounded subshell loop all stopped when the call returned, leaving no log lines. A shell tool `background: true` parameter now starts the command in a new session (`setsid` in `pre_exec`), sends stdout and stderr to one log in the tool-output directory, and returns the pid and log path at once.

Upstream v1.19.2 has no model-facing background launch: the V2 core bash tool lists it as planned work "only with owner-bound get/wait/cancel tools and completion delivery", and its background-job registry serves task children. So `background: true` is recorded as an ocmini extension, not parity. Ownership follows that registry, which is process-local and documented as not durable: a background job belongs to the session that started it, and `SessionRunState.cancel` plus session deletion end a session's jobs. ocmini matches that by ending the starting session's jobs on turn abort and on session delete, and every job on runtime exit. The get/wait/cancel tools, completion delivery and durable status upstream requires for this feature are not implemented; there is no live progress stream, only the log file.

Upstream also keeps captured output on timeout: the V1 shell tool returns a completed call whose output is what the command printed, ending with a `<shell_metadata>` notice, and leaves `metadata.exit` null (V2 core is stricter and returns only a timeout message). ocmini previously discarded everything captured before the limit. It now returns the upstream V1 shape, with `metadata.timeout: true` added, so a cancelled build stays visible.

Unit checks cover the timeout preview, the silent-command `(no output)` marker, the process-group kill on timeout, background survival with session-scoped and runtime-exit cancellation; contract checks drive the real server, require the background tool's `metadata.pid` and `metadata.outputPath`, verify the job keeps writing after the turn ends, verify `POST /session/:id/abort` ends the job, and confirm a timed-out command completes and its captured output reaches the next model request with the `exceeding timeout 700 ms` notice.

The reported `Session database: database is locked` came from startup recovery.
`recover_interrupted` ran its two full-table `json_extract` scans inside its
write transaction, so every instance start held the write lock while scanning.
Measurements used a `VACUUM INTO` snapshot of the live session database (645 MB,
38,324 messages, 157,172 parts, 625 sessions); the scans cost 0.18 s and 0.49 s.
A writer probing lock acquisition saw a median of 0.3 ms and a maximum of
3,632 ms during twelve sequential instance starts. Eight concurrent starts plus
a concurrent writer drove the writer's slowest write to 6,334 ms, and the same
burst against the pre-fix executable produced 60 failed starts and one writer
failure, both reading `Session database: database is locked` — the reported
error. Recovery scans now run on the read connection and only the guarded
updates use the write transaction, which re-checks each scanned condition so a
row a sibling changed in the meantime is left alone. The write busy timeout is
30 seconds, and `Store::open` retries a locked open five times over about three
seconds, since opening is idempotent. After the change the same probe measured a
1 ms maximum lock hold (median 0.3 ms, p95 0.5 ms), eight concurrent recoveries
finished with a slowest start of 1,255 ms instead of 5,630 ms, and 120 starts in
six bursts of twenty with a concurrent writer produced no failed start, no
failed write and a 3 ms slowest write. Startup recovery semantics, leases and
the schema are unchanged.

Formatting, Clippy with warnings denied, the locked release build, and all 163
tests (117 unit and 46 contract) pass.

## Draft erase over committed transcript rows

Pressing Up in a multi-line draft made the next redraw delete lines the user
never wrote. The redraw erases the draft block before printing it again, and it
climbed `drawn_rows - 1` rows, which only reaches the block's first row when the
caret sits on the block's last row. With the caret parked higher, the climb
landed above the draft and the clear-to-end-of-screen removed the transcript
rows printed there.

Reproduced and re-checked in tmux (a real terminal emulator, 100x24, a
three-line draft): before the change, Up, Up and a typed character removed two
help lines above the draft, six more Up presses removed the rest of the
transcript, and Ctrl+D left an empty pane. After the change the transcript rows
stay intact through the same keystrokes, insertions land at the caret, Ctrl+L
redraws and Ctrl+D leaves the transcript on screen. The caret's row is now
tracked, so a transcript row flushed above the draft shifts the block down
instead of putting the caret one row above where it was drawn.

## Startup recovery scans on a large store

Startup felt slow on a real store: 656 sessions, 39,056 messages and 160,180
parts (713 MB, `user_version` 2). Every `Store::open` ran startup recovery, and
its two scans filtered `json_extract(json, ...)` row by row with the session
scope driving the plan: 199 ms over messages and 546 ms over parts to match
three unfinished messages and two unfinished tool parts. On a copy of that store
a process measured 746–758 ms for `session list`, 750–933 ms from start to
`/global/health`, and 758–842 ms from a tmux spawn to the first TUI frame. The
installed opencode 1.19.3 took 4.4 s to its first frame in the same project.

The recovery filters now live in plain columns mirrored at write time
(`message.completed`, `part.kind`, `part.status`) with two partial indexes over
unfinished work. `EXPLAIN QUERY PLAN` seeks both indexes and the scans measure
0.1 ms; the same three commands measure 7 ms, 9–17 ms and 19–20 ms. The indexes
occupy 8 KB on this store and add no measurable write cost (20,000 finished
part inserts measured 2.88 µs per row with and without them, because finished
rows keep no index entry). Recovery semantics and leases are unchanged: the
mirrored columns are written in the same transactions as the JSON, and the
recovery updates keep their existing guards.

Migration runs once at `user_version` 3. It adds the columns, backfills from
JSON with `json_valid` guards so a legacy malformed row cannot abort startup,
and builds the indexes: 6.4 s for the first open of this store against 2.1 s for
the same build at `user_version` 2, then 7 ms per open. Two unit tests pin the
plan, requiring both scans to use the indexes, and the migration, checking
mirrored values, an untouched corrupt legacy row and the created indexes.

Formatting, Clippy with warnings denied, the locked release build, and all 176
tests (130 unit and 46 contract) pass.

## Chain-of-thought replay follows OpenCode per turn

Reasoning models on Chat Completions endpoints showed every step's "Thought"
as the same boilerplate ("There is no previous thinking to reproduce...") and
kept answering their own chain of thought. The round trip landed on 10-17 with
the Responses API compatibility pass (506b44c), which began attaching the
provider's assistant payload to each step as `providerMetadata` and replaying
it into later requests. The session store pins the onset: zero boilerplate
reasoning parts before 10-17, the first at 10-18 14:42, then 170 inside one
hour in a chat-protocol session on 10-21. The earlier builds (the v2 releases)
have no round trip and show none, matching memory.

OpenCode's `@ai-sdk/openai-compatible` converter replays chain of thought too;
the difference is shape. OpenCode has one assistant message per prompt and
sends one `{role:"assistant", content, reasoning_content, tool_calls}` per
turn. ocmini creates one assistant message per step, so a multi-step turn sent
N assistant messages whose reasoning the model treated as unfinished thinking
to continue. Chat requests now merge a turn's steps into a single assistant
message, OpenCode's shape: every step's chain of thought concatenated in step
order (whichever key the provider used: `reasoning_content`, `reasoning` or
`reasoning_text`), all `reasoning_details` and tool calls appended in order,
with tool results following. The merge happens only for Chat protocol
providers, resolved through the adapter's own protocol selection. Responses
item replay and signed Anthropic thinking are untouched: both need their
per-step opaque payloads intact.

A unit pair pins both shapes (merged for chat, one message per step for
Responses). The contract test asserts the wire shape at every step: step two
sends exactly one assistant message carrying step one's reasoning and call,
and step three carries both steps' reasoning concatenated with both calls and
their results. The compaction contract keeps its verbatim guarantees under the
merged shape (tool-call identifiers, Unicode tool results). A live run
against a Chat Completions reasoning model through a request-logging proxy
shows the second step's request carrying exactly one assistant message with
the first step's `reasoning` and tool call, the final answer reproducing the
file content, and no boilerplate reasoning.

Stored reasoning parts, the SSE stream, the API and the display of "Thought"
are unchanged. Formatting, Clippy with warnings denied, the locked release
build, and all 179 tests (132 unit and 47 contract) pass.

## One-shot prompts (`ocmini run`)

Goal: `ocmini run "prompt"` prints the answer and exits, like `opencode run`
(v1.19.3), sharing ocmini's engine, sessions, tools and permissions.

The turn runs in-process: `run` resolves the session (new, `--continue`,
`--session`, `--fork`, `--dir`, `--title`), calls the engine's `start_turn`
and renders bus events until the turn's completion receiver fires. Text mode
prints the `> agent · model` header, `→ tool …` lines and the answer;
`--thinking` adds chain of thought. `--format json` emits JSONL
(`step_start`, `text`, `step_finish`, `tool_use`, `reasoning` with
`--thinking`). Piped stdin appends to the prompt; `-f/--file` attaches files
as data-URL parts; `-i/--interactive` keeps prompting on the same session.
Permissions are non-interactive-safe: `--auto` allows, a TTY prompts
`[y]/[a]/[n]`, a non-TTY refuses so the command never hangs.

Deltas from opencode: `-u/--attach` and `--port` are dropped (in-process
engine, no server needed); JSON events are ocmini's own set, not byte-identical
to opencode's. Rendering mirrors the TUI: the engine announces streaming parts
by emitting the accumulated text on the bus, so the printer re-renders the
growing part and prints only the suffix per update. Completion races the last
events — the queue is drained before the completion receiver is reported, so
final part events always render first.

Three contract tests pin the behavior against the mock provider: text mode
prints the answer and `-c` continuation sends the previous exchange as
history; `--format json --thinking` produces parseable `step_start`/
`reasoning`/`text`/`step_finish` lines; piped stdin reaches the model inside
the prompt. Formatting, Clippy with warnings denied, and all 182 tests
(132 unit and 50 contract) pass.

## Session titles for resume

Every launcher titled a fresh session with a fixed string — `tui session`,
`run` or `New session` — so `--resume`, `Ctrl+R` and `session list` showed
identical rows and gave no way to tell conversations apart. A session that
still carries a launcher title is now renamed by the engine's shared admission
path: the first non-empty line of the first prompt becomes the title (first 80
characters), and the new session is published on the bus as
`session.updated` so OpenCode clients see it live. The rule runs once because
a real title is never a placeholder, so later prompts, `--title`,
`PATCH /session` and task-child titles are kept; a fork of a placeholder
session is a placeholder, and a prompt with no text (image-only, compaction)
leaves the title alone.

The prompt's first line is the safety net. With `title_model` set in
`opencode.json` (`provider/model`, or `small` to start at `small_model`), the
first prompt starts one tool-free title request on a background task and the
reply's first line becomes the title; the request keeps `variant: None`, since
a leaked session variant is a 400 on a different model. Each candidate that is
unavailable, errors or answers with nothing usable moves to the next
(`title_model` → `small_model` → the session's model), the deterministic title
closes the chain, and chat-template control tokens a reasoning model leaks
into the text channel are stripped before a title is saved. Terminal chat
shows `◦ generating title…` and the chosen title, run mode stays silent, and
the bus carries `session.title` start/done events.

Admission checks the placeholder rule before naming, not only before saving: a
prompt in a session that already has a real title starts no title request, so
the model path never paid a side request per message and never printed
`◦ generating title…` for a rename `set_title` would refuse — a start event
with no matching done, on every message.

Seven unit tests cover title derivation, the placeholder rule, the once-only
rename, control-token stripping and the model chain; three contract tests pin
the server paths: `POST /session` starts as `New session`, the first
`prompt_async` renames it and emits `session.updated` with the new title, and
a session created with a title keeps it; with `title_model` set the session
takes the model's title, the title request carries the prompt tool-free, and a
title request that dies falls back to the first prompt line; a titled session's
next prompt is answered by the model alone, with no title request and no start
event. Formatting, Clippy with warnings denied, and the full suite pass.

## Configured reasoning options

OpenCode 1.19.3 added `provider.<id>.models.<id>.reasoning_options`: the reasoning
controls a model accepts, which build its variants and **replace** the
catalog-derived ones, with configured `variants` merged on top and
`reasoning_options: []` meaning no variants at all. ocmini parsed the key as an
unknown model field, so a configuration written for upstream showed no reasoning
variants in T3's picker, and `reasoning_options: []` — "this model takes no
variants" — left the discovered ones in place.

Variants are built at the end of model discovery, the single point every surface
already reads: `GET /provider`, `models --verbose` and the engine's request
options. The picker and the engine therefore see one list. A declared `effort`
list yields one variant per value, with `null` as `none`; the payload follows the
model's protocol the way discovered variants already do — `reasoningEffort` for
Chat Completions and Responses, adaptive thinking plus an output effort for
Messages — and the protocol comes from the same detection a request uses
([`protocol`](src/provider/mod.rs)), so a model's variant payload matches its
transport. `budget_tokens` builds `high`/`max` thinking-budget variants on
Messages endpoints, bounded by the model's output limit and upstream's
32,000-token ceiling; OpenAI-style endpoints have no budget mapping, so, as
upstream does, options no supported protocol maps leave the discovered variants
untouched. Configured `variants` merge on top of the generated list and `disabled`
entries drop out, so an empty list leaves only the variants the model declares
itself.

Four unit tests cover the effort mapping, the protocol split, clearing and
keeping, and the budget arithmetic; one contract test pins `GET /provider`, where
a configured `reasoning_options` model replaces the discovered variants, keeps a
configured variant's extra options on top, and `reasoning_options: []` yields none.
Formatting, Clippy with warnings denied, and the full suite pass. The version
string moves to `1.19.4-ocmini.0.3.4`: upstream v1.19.4 upgrades the bundled Bun
runtime and the musl cross-build install flags, which has no counterpart in a
Rust runtime with no JS dependencies, and changes no part of the 1.x HTTP/SSE
surface.

## Tool calls in the resumed transcript

A resumed session showed every tool call as one dim line — `● bash · completed` —
while the same call running live printed `● bash ls -la` and a `✓` line with the
output. The saved part keeps `state.input`, `state.output` and `state.error`
(pruning only marks older output for the provider and leaves it browsable), so
the data was there; `render_history` simply formatted only the tool name and
status. Every command, path and pattern in the history was invisible after
`--continue`, `--session` or `Ctrl+R`.

One renderer now formats a tool call — `tool_line` for the call with its input
summary, `tool_result` for the `✓`/`✗` line — and both paths call it, so a
resumed transcript is byte-identical to the live one. A call that never
finished (`pending`/`running`, stored as `Aborted` on cancel) has no result, so
it shows the status it stopped at.

Three unit tests pin the resumed block (command and output, path and error, the
status of an unfinished call) and assert the live event path emits exactly the
same text, once per part, for this session only. Formatting, Clippy with
warnings denied, and the full suite pass.

## Manual checks

Use a terminal to check Unicode input, multiline paste, Ctrl+C abort, Ctrl+D exit and terminal restoration. Send a prompt, exit, then resume with `--continue` or `--session ID` and verify the next response uses the earlier conversation. Exercise the session selector with `--resume` and `Ctrl+R`: search by title/ID/directory, Tab through sort orders, arrow/page navigation, Enter resume, Esc cancel.

Live-provider calls and full client-application workflows require validation in the target environment. Local tests do not establish performance or compatibility with unsupported provider protocols. See [SPEC-CONTRACT.md](SPEC-CONTRACT.md) for the API surface.
