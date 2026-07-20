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
document. A second server session resolves non-null Rust and Lua nodes through
`kakehashi/node`, proving both grammars parse again and the quarantine is
session-local; an empty semantic-token result is not accepted as evidence.

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
being overwritten as a generic timeout. A `WorkerRestartRequired` response
received after the deadline but inside that grace likewise remains authoritative
and replaces the invalid generation rather than being overwritten as a generic
timeout.

A planned configuration transition first fences request admission and waits five
seconds for cooperative quiescence. If a healthy or non-cooperative published
request outlives that window, the old generation is forcibly retired and the
planned replacement continues. The regression E2E holds native work beyond the
full five-second window and proves one replacement, no quarantine, and no
configuration-gated breaker.

The focused 67 worker and 40 supervisor tests, the single-crash, protocol-
failure, multi-hazard, single-timeout, four-thread saturation, planned-restart,
and delayed-restart-response E2Es,
all-target Check, warning-denying Clippy, and formatting checks pass locally.

Cancellation delivery is also ordered without an unbounded unknown-ID
tombstone set. The parent registers and enqueues a semantic request before
arming its cancellation guard, while the worker stores tokens only for admitted
work. An E2E delays creation of the blocking response waiter, cancels during
that window, observes the cancel on the already-admitted worker request, and
then serves a healthy follow-up without restarting the generation.

## Recovery measurement

The performance measurement compares immutable Stage 25 (`f6e0c2125`) and the
measured Stage 26 candidate (`67c338ca7`) with the same single-hazard E2E on
macOS 26.5.1 (`Darwin
25.5.0`, Apple M4). After one warmup per stage, 12 pairs alternate revision
order. The supervisor's `recovery_ms` covers failure recovery through
replacement spawn and full resync; it is not end-to-end request latency.

Review convergence subsequently changed only deadline/restart branches and test
assertions. Those commits add no operation to the measured admitted-request hot
path, so the table remains evidence for the combined Stage 26 admission-path
cost; its binary
hashes and raw samples intentionally continue to identify `67c338ca7` rather
than being relabeled as a final-HEAD run.

| Metric | Stage 25 | Stage 26 | Difference |
| --- | ---: | ---: | ---: |
| Median recovery | 1153.5 ms | 1144.5 ms | -9 ms (-0.8%) |
| Mean recovery | 1186.8 ms | 1160.3 ms | -26.4 ms (-2.2%) |
| Range | 1137–1477 ms | 1132–1297 ms | — |
| Median paired delta | — | — | +3 ms (+0.26%) |

The medians show no regression, but paired differences span -342 to +123 ms.
The debug E2E combines process spawn, hashing, backoff, and resync, so this run
does not isolate hazard-set construction. The +3 ms paired median is
noise-scale and does not establish a regression.

The two-active-grammar E2E also ran seven times. Every run quarantined two
grammars, restarted once, and resynchronized one healthy document (14 bytes).
Recovery was 950–965 ms, with a 960 ms median and 959 ms mean. This fixture has
a different failure trigger and document set, so its absolute time is not
directly comparable with the single-hazard fixture. It establishes that two
attributions remain one recovery action rather than two restarts.

The four-thread saturation E2E ran seven times after one warmup. Its configured
2,000 ms request deadline and 250 ms cancellation grace are included in the
measurement. Replacement readiness was 3181–3218 ms after request issue, with a
3194 ms median and 3195 ms mean. The narrow 37 ms range shows that four leaked
native jobs are reclaimed by one bounded process restart rather than four
serial per-thread recoveries.

## Steady-state measurement

The Stage 26 admission path adds one atomic fence load while the request route
mutex is already held, classifies whether a request may use the supervisor
reserve, and applies the corresponding admission arithmetic. To measure these
combined hot-path changes, the unchanged Stage 25/26 release harness ran 12
measured pairs after one warmup per stage. Revision order and worker/direct
order alternated. Each run used 500 incremental requests over 200 Rust source
lines and four worker threads.

| Metric | Stage 25 median | Stage 26 median | Median paired delta |
| --- | ---: | ---: | ---: |
| Sequential parent p50 | 390.729 us | 383.313 us | -1.511% |
| Sequential parent p95 | 469.459 us | 434.875 us | -6.935% |
| Four-way parallel parent p50 | 552.855 us | 553.771 us | +0.512% |
| Four-way parallel parent p95 | 630.105 us | 634.125 us | +0.614% |
| Four-way parallel throughput | 6644 req/s | 6607 req/s | -0.590% |

Sequential medians move in the faster direction, while all three parallel
paired medians move by less than 1%. Run-to-run ranges overlap and sequential
p95 paired deltas span -17.4% to +17.7%, so the fixture does not distinguish a
causal speedup. It does show no measurable parallel regression from the combined
Stage 26 admission-path changes. This small run cannot establish a tail-latency
bound.

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
