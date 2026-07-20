# Single Tree Worker Stage 25 Committed Hazard Measurement

## Scope

Stage 25 removes the parent-to-worker half of the Stage 24 grammar-hazard
handshake without weakening its crash ordering. A compute thread sends the arm
frame with a local completion receipt. The single worker writer fulfills that
receipt only after the complete frame has been written to the OS pipe. The
compute thread may enter grammar-backed work only after receiving the receipt.

If the worker then crashes, the parent reader must decode the already-committed
arm frame before a later release or EOF. A partial arm cannot produce a receipt,
so the worker cannot enter the hazardous scope after a torn frame. This removes
the protocol acknowledgment, its waiter map, and the parent reader's dependency
on the outbound request writer. Protocol version 18 makes the change explicit.

## Correctness result

The RED E2E made a worker abort on receipt of a Stage 24 hazard acknowledgment.
The existing retry path eventually returned a semantic response, but only after
16.72 seconds, failing the ten-second latency bound. With Stage 25 no
acknowledgment exists; the same request completes within the bound without a
worker restart.

The independent post-commit crash E2E still observes the arm, logs the exact
Rust artifact, quarantines it only for the current session, restarts and full
resynchronizes the worker, and serves the healthy Lua grammar. Check, Clippy,
the 61 focused worker unit tests, and both E2E cases pass locally.

## Performance result

The measurement compares immutable Stage 24 (`f8b647522`) and Stage 25
(`857362c9e`) release binaries and matching harnesses on macOS 26.5
(`Darwin 25.5.0`, Apple M4). Both use the same Rust parser artifact, four worker
threads, 200 source lines, and 500 requests per run. After one warmup per
generation, two batches of twelve measured pairs alternate order; the second
batch reverses the first order of the first batch.

| Metric | Stage 24 median | Stage 25 median | Median paired delta |
| --- | ---: | ---: | ---: |
| Sequential parent-observed p50 | 1.494 ms | 1.438 ms | -3.300% |
| Sequential parent-observed p95 | 1.556 ms | 1.511 ms | -2.831% |
| Sequential queue/arm wait p50 | 0.048 ms | 0.025 ms | -46.909% |
| Four-way parallel parent-observed p50 | 1.780 ms | 1.828 ms | +2.226% |
| Four-way parallel parent-observed p95 | 2.005 ms | 2.162 ms | +9.200% |
| Four-way parallel throughput | 2175 req/s | 2096 req/s | -3.151% |

Stage 25 improves sequential p50 in 21 of 24 pairs, p95 in 23, and arm wait in
all 24. It regresses parallel p50 in 21 pairs, p95 in 20, and throughput in 20.
The result confirms both Stage 24 findings: the cross-process acknowledgment is
a measurable hot-path cost, and its extra serialization also acted as
accidental admission pacing under four simultaneous parses. Raw samples and
artifact digests are in
`benches/profile/results/single_worker_stage25_committed_hazard_2026-07-20.json`.

## Decision

Keep the pipe-commit ordering. Safety protocol should establish only the causal
crash-attribution boundary; it should not depend on a parent scheduler round
trip to shape compute contention. Stage 25 recovers most of the measured
sequential acknowledgment cost while preserving post-arm crash evidence.

Do not claim a complete performance-gate pass. The next scheduling stage must
test an explicit, workload-aware admission policy against both the sequential
and parallel gates. It must not reintroduce the parent acknowledgment or use a
fixed sleep as a hidden pacing mechanism. Independently bounded control
transport, runtime injection identities, multiple-hazard quarantine, and native
segment deadlines remain open. The ADR remains `proposed`.
