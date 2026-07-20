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
client sends cancellation at the deadline, waits 250 ms for a terminal
response, and systemically terminates the generation if native work does not
cooperate. Parent deadline termination has its own cause and cannot quarantine
an active grammar. The saturation E2E parks four distinct committed grammar
jobs on all four compute threads, then proves that one generation restart
releases every route, quarantines nothing, full-resynchronizes four documents,
and keeps LSP service available.

Reader and writer cleanup do not use the deadline cause. A protocol-failure E2E
commits a Rust hazard and then sends an invalid release frame. The reader
classifies the failure as systemic, preserves and quarantines the complete
active Rust set, restarts once, and serves the healthy Lua document. A
transport error received during cancellation grace is propagated instead of
being overwritten as a generic timeout.

The focused 64 worker and 40 supervisor tests, the single-crash, protocol-
failure, multi-hazard, single-timeout, and four-thread saturation E2Es,
all-target Check, warning-denying Clippy, and formatting checks pass locally.

## Recovery measurement

The final-HEAD measurement compares immutable Stage 25 (`f6e0c2125`) and Stage
26 (`30ae589b3`) with the same single-hazard E2E on macOS 26.5.1 (`Darwin
25.5.0`, Apple M4). After one warmup per stage, 12 pairs alternate revision
order. The supervisor's `recovery_ms` covers failure recovery through
replacement spawn and full resync; it is not end-to-end request latency.

| Metric | Stage 25 | Stage 26 | Difference |
| --- | ---: | ---: | ---: |
| Median recovery | 1202 ms | 1182 ms | -20 ms (-1.7%) |
| Mean recovery | 1231 ms | 1216 ms | -15 ms (-1.2%) |
| Range | 1164–1394 ms | 1120–1549 ms | — |
| Median paired delta | — | — | -47 ms (-3.8%) |

The medians show no regression, but paired differences span -234 to +385 ms.
The debug E2E combines process spawn, hashing, backoff, and resync, so this run
does not isolate hazard-set construction and cannot support a claimed 3.8%
improvement.

The two-active-grammar E2E also ran seven times. Every run quarantined two
grammars, restarted once, and resynchronized one healthy document (14 bytes).
Recovery was 954–978 ms, with a 962 ms median and 963 ms mean. This fixture has
a different failure trigger and document set, so its absolute time is not
directly comparable with the single-hazard fixture. It establishes that two
attributions remain one recovery action rather than two restarts.

The four-thread saturation E2E ran seven times after one warmup. Its configured
2,000 ms request deadline and 250 ms cancellation grace are included in the
measurement. Replacement readiness was 3176–3204 ms after request issue, with a
3197 ms median and 3196 ms mean. The narrow 28 ms range shows that four leaked
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
| Sequential parent p50 | 415.334 us | 436.667 us | -0.167% |
| Sequential parent p95 | 474.438 us | 474.250 us | +0.383% |
| Four-way parallel parent p50 | 554.876 us | 546.188 us | -0.292% |
| Four-way parallel parent p95 | 632.479 us | 632.688 us | +0.087% |
| Four-way parallel throughput | 6614 req/s | 6617 req/s | +0.582% |

Directions are mixed and every paired median is below 1%. Absolute p50 medians
also move differently from their paired medians because the alternating
worker/direct order makes these small samples bimodal. The atomic admission
check therefore has no distinguishable steady-state cost in this fixture. One
Stage 26 parallel p95 sample reached 1116 us (+77% paired), while another pair
favored Stage 26 by 24%; this small run cannot establish a tail-latency bound.

Raw samples, artifact roles and hashes, stable source paths, exact build/run
commands, metric ordering, and commit identities are in
`benches/profile/results/single_worker_stage26_hazard_set_2026-07-21.json`.

## Decision

Keep complete-set quarantine, the reader-drain barrier, bounded systemic
cancellation recovery, the termination-cause split, and the request-admission
fence. Recovery work is bounded by active leases, while the measured request-
path change is noise-scale in both sequential and four-way workloads.

Do not claim a tail-recovery or complete performance-gate pass from these
samples. Runtime injection identities, independently bounded control transport,
native segment deadlines, explicit workload-aware admission, protocol and
compatibility gates, and legacy-path removal remain open. The ADR remains
`proposed`.
