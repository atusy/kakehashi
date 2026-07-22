# Single Tree Worker Stage 30 Semantic Query Metadata Measurement

## Status

Rejected.

Stage 30 tested request-local memoization of semantic-query pattern priorities
and capture roles. It did not change request admission, edit delivery,
latest-wins cancellation, worker ownership, or response currency. The final
candidate created lazy indexed tables only when a syntax tree had at least
4,096 descendants and at least 16 descendants per possible query metadata
slot.

This admission formula was a heuristic guardrail, not a proven cost model.
Pattern and capture slots have different allocation costs, and descendant count
does not guarantee matches or reuse. No claim is made for custom or
capture-heavy queries.

## Alternatives tested

Incremental candidates included:

- a 32 KiB source-only gate;
- a 32 KiB plus 4,096-node gate;
- admission after 64 and 256 observed matches/captures;
- memoizing only pattern priorities; and
- the final node-count/query-slot ratio outside the hot loops.

Observed-item admission put counters and branches in the match/capture loops and
regressed Markdown burst latency. Priority-only memoization did not preserve the
earlier gains. The final ratio avoided hot-loop admission overhead, but its
initial apparent typing improvement did not survive provenance-hardened
remeasurement.

## Measurement integrity

The final rerun used content-addressed Stage 29 and Stage 30 server binaries.
The benchmark harness computed each binary's SHA-256 before measurement and
stored it in both the raw binary manifest and every raw run. Every pair records:

- Stage 29:
  `421ecba956065305f382d2663cdfff2ac22729e60134fa47646cba7758aa9e75`
- Stage 30:
  `dcd5c1a9e7c52e827840fff2c7b689f925dad92f33deb01f1e73ab232b7ac6ee`

The Stage 29 source was
`510b8157d68a5c51d7b95883fe2a42a8a148afde`; the measured Stage 30
server source was
`88303574f14a2f80e12e36a87fc6ff00aff0bbc4`. The attesting harness
binary SHA-256 was
`349caff1a800098ffc899cbd1d5ca3fcd5950d1f0d1177a29c062714bcb618f4`.

Four pairs alternated binary order. Each binary received six warmups and 30
timed iterations per scenario under the authoritative worker with eight compute
threads. Every full/delta response was reconstructed client-side and accepted
only when its tracked token represented the latest edit. All 2,160 timed
responses passed. All 1,152 obsolete cancellation-burst requests, including
warmups, returned JSON-RPC `RequestCancelled (-32800)`.

## Result

The median of four paired Stage 30 deltas relative to Stage 29 was:

- Rust single-edit typing: **+10.0%** (range -26.0% to +28.7%).
- Rust eight-edit burst: **+2.0%** (range -15.6% to +9.1%).
- Rust four-cancellation burst: **-12.0%** (improvement).
- Sparse Rust 64 KiB control: **+11.6%** (regression in all four pairs).
- Markdown single-edit typing: **-0.03%** (range -0.44% to +13.1%).
- Markdown injection burst: **+1.7%** (range -13.8% to +21.8%).

The rerun demonstrated a cancellation-latency improvement and a consistent
sparse-64-KiB regression, but did not demonstrate a reliable primary typing-path
improvement. That evidence does not justify shipping the candidate for the
narrower cancellation win.

Authoritative raw evidence:

- [pair 1](../../benches/profile/results/single_worker_stage30_amortized_metadata_final_pair1_2026-07-22.json)
  (`c1baa481da14adf718f1667435f4de65239a62b1adfe8c4569110afcd5103f8a`)
- [pair 2](../../benches/profile/results/single_worker_stage30_amortized_metadata_final_pair2_2026-07-22.json)
  (`05a5061b409d50c00a34ffb8fcd654930a727c8fead01fb91cd4d5512e520eb8`)
- [pair 3](../../benches/profile/results/single_worker_stage30_amortized_metadata_final_pair3_2026-07-22.json)
  (`008bd56613124b4d1f34ea1473070850a03698d05293f7a947892130bc2c4f28`)
- [pair 4](../../benches/profile/results/single_worker_stage30_amortized_metadata_final_pair4_2026-07-22.json)
  (`9ab991a221960aeb49327bbbc4cfc15bdd5fe3040d2f9f050961bea32b55ecf3`)

The machine-readable aggregate is
[`single_worker_stage30_query_metadata_2026-07-22.json`](../../benches/profile/results/single_worker_stage30_query_metadata_2026-07-22.json).

## Decision

Reject semantic-query metadata memoization and restore the reviewed Stage 29
collector. Retain only the sparse benchmark controls, binary attestation, raw
evidence, and this rejection record. Do not introduce edit coalescing or
latest-wins suppression to rescue the result; those would change freshness
rather than make the same work faster.
