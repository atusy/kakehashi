# Single Tree Worker Stage 24 Grammar Hazard Measurement

## Scope

Stage 24 prototypes the ADR's parent-visible grammar hazard lease. Before a
worker thread enters a grammar-backed operation, it reports the request's
resolved host grammar symbol and artifact digest. The parent records that lease before acknowledging
it; only then may the worker proceed. A release frame removes the lease before
the terminal response. If the worker dies after acknowledgment, the supervisor
uses the still-active lease instead of inferring a grammar from the failed
request.

This stage uses the existing unbounded control writer rather than the ADR's
future independently bounded control transport. It also selects only the first
active lease for quarantine when several concurrent leases survive a crash.
Discovering and leasing every runtime injection grammar separately also remains
future work. Those limitations remain explicit follow-up work.

## Correctness result

The protocol version advances to 17 and round-trip tests cover the arm,
acknowledgment, and release frames. The worker rejects stale-generation and
unknown acknowledgments. The parent rejects duplicate arms, stale releases, and
unknown releases.

The crash E2E aborts the worker only after it has consumed the parent's
acknowledgment. The parent logs the acknowledged Rust grammar identity,
quarantines that grammar for the current session, restarts the worker, full
resynchronizes the open document, and continues serving a healthy Lua grammar.
The focused worker suite passed 61 tests, and the crash E2E completed recovery
in 1.375 seconds in the validation run.

## Performance result

The measurement compares immutable release binaries and matching Stage 23 and
Stage 24 (`f8b647522`) benchmark harnesses on macOS 26.5 (`Darwin 25.5.0`, Apple M4). Both use
the same Rust parser artifact, four worker threads, 200 source lines, and 500
requests per run. After one warmup per generation, two batches of twelve
measured pairs alternate baseline-candidate and candidate-baseline order. The
second batch reverses the first order of the first batch.

| Metric | Stage 23 median | Stage 24 median | Median paired delta |
| --- | ---: | ---: | ---: |
| Sequential parent-observed p50 | 1.435 ms | 1.512 ms | +5.382% |
| Sequential parent-observed p95 | 1.486 ms | 1.579 ms | +5.795% |
| Sequential queue/arm wait p50 | 0.008 ms | 0.059 ms | +566.414% |
| Four-way parallel parent-observed p50 | 1.841 ms | 1.792 ms | -2.358% |
| Four-way parallel parent-observed p95 | 2.352 ms | 2.070 ms | -10.021% |
| Four-way parallel throughput | 2040 req/s | 2133 req/s | +3.898% |

The sequential p50 absolute paired increase is 77 microseconds, and Stage 24 is
slower in 20 of 24 pairs. The queue/arm wait increases in all 24 pairs, so the
measured sequential regression is concentrated in the parent-visible
arm/acknowledgment wait.

Under four-way concurrency, Stage 24 improves throughput in 20 of 24 pairs and
p95 in 20 of 24 pairs. The ACK serializes the start of otherwise simultaneous
compute work just enough to act as accidental admission pacing: its wait can
overlap another request while reducing contention among the four parses. This
is a real result for this benchmark, but it does not turn the safety handshake
into a sound scheduling policy. Raw samples and artifact digests are in
`benches/profile/results/single_worker_stage24_grammar_hazard_2026-07-20.json`.

## Decision

Keep the acknowledged lease as the correctness reference, but do not count
Stage 24 as passing the fine-grained latency gate. An extra synchronous control
round trip for every grammar-backed high-level request produces a small but
repeatable sequential regression, and the ADR requires additional native
segment arms inside those leases. Adding those handshakes unchanged would
compound the same cost.

The next stage must preserve the invariant that the parent records activity
before native entry while removing the per-request pipe round trip from the hot
path, for example through the ADR's shared-memory activity-slot optimization.
It must separately test explicit admission pacing, because blindly removing the
handshake may give back the measured parallel contention reduction. Dedicated
control capacity is still required for saturation correctness, but queue
isolation alone cannot remove the causal context switch measured here. The ADR
remains `proposed`.
