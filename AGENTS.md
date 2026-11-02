# Project constraints

ocmini is a lightweight standalone runtime, including for single-core machines with about 500 MB of total system RAM and embedded Linux devices using SD cards. The user confirms the current implementation already meets the performance requirements. Preserve its existing lightweight approach while adding v2 support.

- Prefer the smallest implementation that satisfies the selected contract. OpenCode/T3 compatibility does not require reproducing upstream's architecture or optional product features.
- Keep the existing runtime stack, concurrency behavior, lazy initialization and supported ARM targets. Avoid unnecessary dependencies, helper services, allocations and storage writes in new v2 code.
- Do not add arbitrary resource limits, queue/cache caps, concurrency restrictions, throttling or truncation to make small-machine operation appear successful. Preserve functionality and throughput on capable machines.
- Do not detect device capabilities or vary behavior according to available hardware. Use the same implementation across machines.
- Reuse existing streaming, paging and storage mechanisms where appropriate. Keep necessary v2 persistence consistent with existing durability and recovery behavior.
- Keep work focused on v2 compatibility. Do not introduce resource-budget systems, mandatory benchmark phases or a performance redesign without a concrete regression caused by the change.

The OpenCode v2 migration scope and accepted compatibility limitations are recorded in docs/opencode-v2-plan.md. Public v1 HTTP retirement is the proposed target; existing session/config migration and ocmini hook payload compatibility remain required. The user has accepted the older T3 client's compaction-schema limitation; do not add a shim or separate wire dialect for it.
