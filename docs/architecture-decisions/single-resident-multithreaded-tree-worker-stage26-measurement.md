# Single Tree Worker Stage 26 Hazard-Set Measurement

## Scope

Stage 26 makes worker-loss attribution set-valued. After a worker loss, the
parent waits for that generation's response reader to drain every complete
committed arm/release frame. It then snapshots the exact active leases, sorts
and deduplicates their `(grammar_symbol, artifact_digest)` identities, installs
the complete quarantine set, and performs one restart and quarantine-aware
resync. Queued commands are not evidence and cannot invent a quarantine.

The ledger remains generation-local. A replacement cannot inherit releases or
leases from its predecessor, and a grammar already quarantined in the same
session escalates a repeated native-evidenced failure to systemic breaker
accounting. A request-admission fence prevents a quiescing generation from
accepting a new route after the supervisor observes it as idle.

## Correctness result

The multi-hazard RED E2E warmed Rust, Lua, and YAML replicas, held a committed
Rust lease, then aborted the worker with a committed Lua lease. Before Stage 26,
the idle worker-loss path observed no implicated grammar and produced zero
quarantines.

The GREEN E2E observes both complete arm frames before EOF, quarantines exactly
Rust and Lua, restarts once, and full-resynchronizes only the healthy YAML
document. A second server session serves Rust and Lua again, proving the
quarantine is session-local.

A generic request deadline remains insufficient parser-fault evidence. The
client now sends cancellation at the deadline, waits 250 ms for a terminal
response, and systemically terminates the generation if native work does not
cooperate. Parent-initiated termination cannot quarantine an active grammar.
The saturation E2E parks four distinct committed grammar jobs on all four
compute threads, then proves that one generation restart releases every route,
quarantines nothing, full-resynchronizes four documents, and keeps LSP service
available.

The focused 63 worker and 40 supervisor tests, the single-crash, multi-hazard,
single-timeout, and four-thread saturation E2Es, all-target Check, warning-
denying Clippy, and formatting checks pass locally.

## Recovery measurement

The first final-HEAD measurement compares immutable Stage 25 (`f6e0c2125`) and
Stage 26 (`33c61b5b6`) with the same single-hazard E2E on macOS 26.5.1
(`Darwin 25.5.0`, Apple M4). After one warmup per stage, seven pairs run Stage
25 then Stage 26. The supervisor's `recovery_ms` covers failure recovery through
replacement spawn and full resync; it is not end-to-end request latency.

| Metric | Stage 25 | Stage 26 | Difference |
| --- | ---: | ---: | ---: |
| Median recovery | 1170 ms | 1172 ms | +2 ms (+0.2%) |
| Mean recovery | 1186 ms | 1168 ms | -18 ms (-1.5%) |
| Range | 1146–1243 ms | 1133–1204 ms | — |
| Median paired delta | — | — | -13 ms (-1.1%) |

The absolute median and paired median have opposite signs. Seven debug E2E
pairs therefore provide no evidence of a recovery regression or improvement;
the difference is noise-scale and the fixed Stage-25-first order remains a
confounder.

The two-active-grammar E2E also ran seven times. Every run quarantined two
grammars, restarted once, and resynchronized one healthy document (14 bytes).
Recovery was 932–967 ms, with a 957 ms median and 951 ms mean. This fixture has
a different failure trigger and document set, so its absolute time is not
directly comparable with the single-hazard fixture. It establishes that two
attributions remain one recovery action rather than two restarts.

The four-thread saturation E2E ran seven times after one warmup. Its configured
2,000 ms request deadline and 250 ms cancellation grace are included in the
measurement. Replacement readiness was 3187–3211 ms after request issue, with a
3205 ms median and 3202 ms mean. The narrow 24 ms range shows that four leaked
native jobs are reclaimed by one bounded process restart rather than four
serial per-thread recoveries.

## Steady-state measurement

The admission fence adds one atomic load while the request route mutex is
already held. To measure that hot-path change, the unchanged Stage 25/26 release
harness ran 12 measured pairs after one warmup per stage. Revision order and
worker/direct order alternated. Each run used 500 incremental requests over 200
Rust source lines and four worker threads.

| Metric | Stage 25 median | Stage 26 median | Median paired delta |
| --- | ---: | ---: | ---: |
| Sequential parent p50 | 453.167 us | 452.771 us | -0.010% |
| Sequential parent p95 | 489.667 us | 494.979 us | +0.886% |
| Four-way parallel parent p50 | 558.626 us | 557.104 us | -0.616% |
| Four-way parallel parent p95 | 629.688 us | 638.125 us | +0.843% |
| Four-way parallel throughput | 6598 req/s | 6578 req/s | +0.273% |

Directions are mixed and every paired median is below 1%. The atomic admission
check has no distinguishable steady-state cost in this fixture. One Stage 26
parallel p95 sample reached 765 us (+21.7% paired) while its throughput fell to
5942 req/s, so this small run still cannot establish a tail-latency bound.

Raw samples, artifact hashes, metric ordering, and exact commit identities are
in `benches/profile/results/single_worker_stage26_hazard_set_2026-07-21.json`.

## Decision

Keep complete-set quarantine, the reader-drain barrier, bounded systemic
cancellation recovery, and the request-admission fence. The recovery work is
bounded by active leases, while the measured request-path change is noise-scale
in both sequential and four-way workloads.

Do not claim a tail-recovery or complete performance-gate pass from these
samples. Runtime injection identities, independently bounded control transport,
native segment deadlines, explicit workload-aware admission, protocol and
compatibility gates, and legacy-path removal remain open. The ADR remains
`proposed`.
