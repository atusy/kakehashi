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
proves semantic derivation does not populate snapshot-owned discovery before the
parallel pipeline. The existing discovery gate test covers one- and two-thread
pools, absent small work, absent eligible work, stale discovery, partial discovery,
below-threshold discovery, and generation-current eligible discovery. The focused
73 worker tests and warning-denying all-target/all-feature Clippy pass.

The optimization does not return tokens for an intermediate edit. The existing
serve-current fence, request supersession token, worker request cancellation, and
post-compute document/version validation are unchanged. It therefore improves the
time to the current token set without weakening typing follow semantics.

## Performance result

Release binaries for Stage 28 and Stage 29 were measured end to end over LSP in
authoritative-worker mode on macOS/Apple Silicon. Every edit creates a unique
document state and every measured response must advance a usable semantic-token
baseline. Six 30-iteration Rust pairs alternated binary order after six warmups.

For single-edit typing, Stage 29 was faster in five of six pairs. Stage-29 deltas
relative to Stage 28 were approximately -7.4%, -7.1%, +0.6%, -1.6%, -3.7%, and
-8.6%; the median improvement was about 5.4%. For eight-edit bursts followed by
one current delta, Stage 29 was faster in all six pairs, with a median improvement
of about 10%.

A separate 60-iteration validation with ten warmups preserved the intended
small/shallow-document path: injection-heavy Markdown measured -1.7% for
single-edit typing and -7.5% for the eight-edit burst in that run. These figures
are not claimed as a Markdown speedup because run order materially affects
absolute timings; they establish no observed regression after adding the host
node-count gate.

An earlier candidate retained contiguous pending byte edits to avoid full sync
after coalescing. It was rejected before publication: two-, four-, eight-, and
sixteen-edit measurements were noise-scale or changed sign across runs, and one
eight-edit Rust run regressed by 12.6%. Another discovery candidate removed the
serial preparation without opening the parallel gate and regressed single-edit
Rust by 16.7%; it was likewise rejected. The accepted design is the smallest
measured change that improves burst follow latency while preserving the existing
small-document path.
