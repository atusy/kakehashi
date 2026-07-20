# Single Tree Worker Stage 26 Hazard-Set Measurement

## Scope

Stage 26 makes worker-loss attribution set-valued. After a worker loss, the parent
waits for that generation's response reader to drain every complete committed
arm/release frame. It then snapshots the exact active leases, sorts and
deduplicates their `(grammar_symbol, artifact_digest)` identities, installs the
complete quarantine set, and performs one restart and quarantine-aware resync.
Queued commands are not evidence and cannot invent a quarantine.

The ledger remains generation-local. A replacement cannot inherit releases or
leases from its predecessor, and a grammar already quarantined in the same
session escalates a repeated native-evidenced failure to systemic breaker
accounting.

## Correctness result

The RED E2E warmed Rust, Lua, and YAML replicas, held a committed Rust lease,
then aborted the worker with a committed Lua lease. Before Stage 26, the idle
worker-loss path observed no implicated grammar and produced zero quarantines.

The GREEN E2E observes both complete arm frames before EOF, quarantines exactly
Rust and Lua, restarts once, and full-resynchronizes only the healthy YAML
document. A second server session serves Rust and Lua again, proving the
quarantine is session-local. A separate hard-deadline E2E hangs only after its
Rust arm frame is committed; it quarantines Rust from the ledger rather than
inferring it from the timed-out command.

The focused 61 worker and 34 supervisor tests, the single-crash, multi-hazard,
and hard-deadline E2Es, all-target Check, and warning-denying Clippy pass
locally.

## Recovery measurement

The first measurement compares immutable Stage 25 (`f6e0c2125`) and Stage 26
(`370d13d2c`) with the same single-hazard E2E on macOS 26.5.1
(`Darwin 25.5.0`, Apple M4). After one warmup per stage, seven pairs run Stage
25 then Stage 26. The supervisor's `recovery_ms` covers failure recovery through
replacement spawn and full resync; it is not end-to-end request latency.

| Metric | Stage 25 | Stage 26 | Difference |
| --- | ---: | ---: | ---: |
| Median recovery | 1266 ms | 1281 ms | +15 ms (+1.2%) |
| Mean recovery | 1270 ms | 1342 ms | +72 ms (+5.7%) |
| Range | 1250–1299 ms | 1251–1571 ms | — |
| Median paired delta | — | — | +11 ms (+0.9%) |

Stage 26 has two slow samples, 1450 and 1571 ms, so seven runs are insufficient
to claim a tail-latency bound. Its median is 15 ms higher and the median paired
difference is 11 ms, but the fixed Stage-25-first order and the combined
spawn/backoff/resync timer do not isolate reader drain or set construction.
Treat the delta as inconclusive rather than as their causal cost.

The second measurement runs the two-active-grammar E2E seven times. Every run
quarantines two grammars, restarts once, and resyncs one healthy document
(14 bytes). Recovery is 959–1126 ms, with a 974 ms median and 996 ms mean. This
fixture has a different failure trigger and document set, so its absolute time
must not be compared directly with the single-hazard fixture; it establishes
that two attributions remain one recovery action rather than two restarts.

Raw samples are in
`benches/profile/results/single_worker_stage26_hazard_set_2026-07-21.json`.

## Decision

Keep complete-set quarantine and the reader-drain barrier. The failure-path
cost is bounded by active leases and avoids both under-quarantine and command
inference. The production request hot path adds no per-request synchronization;
the new condition variable is signaled only when a worker reader terminates,
and quarantine work runs only during recovery. Stage 25's steady-state
throughput result therefore remains the current scheduling baseline.

Do not claim a tail-recovery or complete performance-gate pass from seven debug
E2E samples. Runtime injection identities, independently bounded control
transport, native segment deadlines, explicit workload-aware admission,
protocol/compatibility gates, and legacy-path removal remain open. The ADR
remains `proposed`.
