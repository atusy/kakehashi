# Single Tree Worker Stage 29 Semantic Discovery Measurement

## Scope

Stage 29 shortens the authoritative worker's semantic-token typing path without
changing edit admission, latest-wins, cancellation, or response currency.
Worker-owned semantic derivation still performs exactly one injection discovery
walk after each edit. When another reader already materialized generation-current
discovery it is reused as before. Otherwise, a sufficiently large and structurally
dense host runs inline discovery in the injection Rayon branch concurrently with
the independent host highlight query.

The gate requires at least three compute threads, 32 KiB of source, 4,096 host
tree nodes, an injection query, no partial discovery, and no competing work that
forces the scheduler to yield nested parallelism. Smaller or structurally shallow
documents retain the previous serial discovery preparation path. This avoids
hard-coding a language and preserves the low-overhead path for small files.

## Correctness result

A worker-unit regression constructs a large Rust tree with injection captures and
proves both sides of the admission boundary: a two-thread worker prepares and
retains discovery serially, while a three-thread worker leaves it to the parallel
pipeline. The shared discovery gate also covers sequential and yielding nested-work
policies, a missing injection query, both size thresholds, absent eligible work,
stale discovery, partial discovery, and generation-current eligible discovery.
The focused worker tests and warning-denying all-target/all-feature Clippy pass.

The benchmark now applies every full or delta response to a client-side token
baseline. A sample is accepted only if the first token on the tracked edit line
moves by exactly the inserted prefix length. This caught stale-content blind spots
that checking only `resultId` cannot detect. All 1,200 timed responses in the final
series represented the latest edit. The existing serve-current fence, request
supersession token, worker cancellation, and post-compute version validation are
unchanged.

## Performance result

Release binaries for Stage 28 and Stage 29 were measured end to end over LSP in
authoritative-worker mode on an Apple M4 with eight worker compute threads.
Four pairs alternated binary order; each binary received six warmups and 30 timed
iterations per scenario. The retained artifact contains all raw nanosecond samples,
p95 values, exact commits, binary and harness hashes, environment, and commands.

The observed median of paired Stage-29 deltas relative to Stage 28 was:

- Rust single-edit typing: median -3.2%, p95 -6.2%.
- Rust eight-edit burst followed by the current delta: median -3.8%, p95 -2.7%.
- Rust four-cancellation burst followed by the current full result: median -8.8%,
  p95 median-of-pairs -11.6% in the reviewed-binary validation series.

These are descriptive measurements, not tail-confidence claims. Single-edit and
eight-edit median latency each regressed in one of four pairs. With 30 samples per
run, p95 is only the second-largest observation: paired p95 deltas ranged from
-15.3% to +2.0% for single-edit typing, -11.6% to +25.0% for eight-edit bursts,
and -17.6% to +32.7% for cancellation bursts. The series therefore supports a
modest central-latency improvement in this workload, while tail improvement or
equivalence remains inconclusive.

The cancellation series uses the strengthened harness: before accepting each
sample it requires all four obsolete requests to return JSON-RPC
`RequestCancelled (-32800)` and verifies the final token set against the latest
edit. Across four alternating pairs it validated 960 obsolete cancellation
responses and 240 current final responses; all four paired medians favored Stage
29. This establishes cancellation attribution for the central-latency observation,
but the sign-changing p95 range still precludes a tail claim.

Markdown retained generation-current discovery (`regions_reused=182` in an
untimed debug validation), so it did not take Stage 29's new undiscovered-work
path. Its sign-changing pair deltas are the control for machine and ordering noise,
not evidence for either a speedup or regression. The authoritative evidence is
[`benches/profile/results/single_worker_stage29_semantic_discovery_2026-07-22.json`](../../benches/profile/results/single_worker_stage29_semantic_discovery_2026-07-22.json).

An earlier candidate retained contiguous pending byte edits to avoid full sync
after coalescing. It was rejected before publication: two-, four-, eight-, and
sixteen-edit measurements were noise-scale or changed sign across runs, and one
eight-edit Rust run regressed by 12.6%. Another discovery candidate removed the
serial preparation without opening the parallel gate and regressed single-edit
Rust by 16.7%; it was likewise rejected. The accepted design is the smallest
measured change that showed lower central follow latency without coalescing edits;
tail behavior remains an explicit subject for further measurement.
