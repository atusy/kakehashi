# Single Tree Worker Stage 18 Memory-Stress Measurement

## Scope

Stage 18 makes the Stage 17 memory budgets immutable worker-process inputs and
adds a deterministic internal inspection operation. The production defaults do
not change: derived caches have a 128 MiB soft budget and estimated
non-evictable state has an independent 512 MiB hard budget.

The configurable budgets exist on the private parent-to-worker boundary. They
allow process tests and release stress workloads to cross both boundaries with
small fixtures instead of consuming hundreds of MiB. Inspection reports the
current document estimates and cache classes; it does not add cumulative
telemetry state to the worker.

## Method

### Reduced-budget boundary stress

Six independent runs used the normal default-feature release binary with a
20-byte derived-cache soft budget and a 4,096-byte estimated non-evictable hard
budget. Each run:

1. opened two Rust documents and derived semantic tokens for both;
2. proved that pressure evicted the older document's result and auxiliary
   caches while retaining the newer result;
3. recomputed the evicted result and compared it byte-for-byte;
4. rejected an oversized full replacement and an oversized incremental edit;
5. verified the prior version, tree extent, and node identity after both
   rejections; and
6. applied a smaller recovery edit in the same worker generation.

The collector sampled the stress driver and its worker child together every
20 ms for aggregate RSS and every 250 ms for macOS shared-aware physical
footprint. Raw reports, hashes, environment, and artifact checksums are in
`benches/profile/results/single_worker_stage18_memory_boundary_stress_2026-07-20.json`.

The source commit was `9b46c7360661fdf1294914b27b4e3a3c86df6c29`.
The worker SHA-256 was
`ce06bbfd7bce95d7b0867142008e5a381a16d3ef26b6d0064c230d288b75dfc4`,
and the stress-driver SHA-256 was
`9ac0590725fd23ea16d57394a95d76ea629b1fbc82c5bdc53291501f8c2e91ae`.

### Default-budget A/B

Twelve batches compared the Stage 17 release binary with the Stage 18 release
binary in alternating order. Each process used one worker with four compute
threads, opened three copies of the 1,121,182-byte, 5,000-injection Markdown
fixture, warmed every semantic-result cache, round-robined six requests, and
held all documents open while the collector sampled memory.

An independent twelve-run, order-reversed measurement used one such document
and twenty exact-version semantic requests after warmup without memory
sampling. Raw results are in
`benches/profile/results/single_worker_stage18_memory_stress_default_multidoc_2026-07-20.json`
and
`benches/profile/results/single_worker_stage18_memory_stress_default_cache_hit_2026-07-20.json`.

## Result

All six reduced-budget runs observed the intended cache state and passed every
correctness invariant. Median operation times were:

| Operation | Median | Range |
| --- | ---: | ---: |
| First derivation for document A | 3.590 ms | 3.002–4.206 ms |
| First derivation for document B | 0.112 ms | 0.083–0.154 ms |
| Recompute evicted document A | 0.078 ms | 0.071–0.088 ms |
| Reject oversized full sync | 1.320 ms | 1.215–1.426 ms |
| Reject oversized incremental edit | 0.978 ms | 0.773–1.071 ms |
| Recovery edit | 0.097 ms | 0.080–0.125 ms |

The small boundary fixture's median aggregate RSS was 19.81 MiB and median
physical footprint was 6.56 MiB. These values describe the driver plus worker;
they are not the enforced limit or evidence of production-scale reclamation.

At the unchanged default budgets, three-document memory medians were:

| Metric | Stage 17 | Stage 18 | Change |
| --- | ---: | ---: | ---: |
| Stable physical footprint | 468.33 MiB | 480.77 MiB | +12.44 MiB (+2.7%) |
| Peak physical footprint | 485.94 MiB | 502.44 MiB | +16.50 MiB (+3.4%) |
| Stable aggregate RSS | 535.80 MiB | 550.62 MiB | +14.83 MiB (+2.8%) |
| Peak aggregate RSS | 537.50 MiB | 554.30 MiB | +16.80 MiB (+3.1%) |

Paired stable-footprint deltas ranged from -34.16 MiB to +68.54 MiB, and
paired stable-RSS deltas ranged from -17.45 MiB to +64.05 MiB. The candidate
medians are higher, but both ranges cross zero and are much wider than the
median differences. This run therefore records a possible noise-scale memory
increase, not a repeatable regression or improvement.

Exact-version latency medians were:

| Metric | Stage 17 | Stage 18 | Change |
| --- | ---: | ---: | ---: |
| p50 | 76.75 ms | 76.40 ms | -0.5% |
| p95 | 80.40 ms | 80.10 ms | -0.4% |
| Wall time per cycle | 120.59 ms | 119.22 ms | -1.1% |

Paired deltas were mixed: Stage 18 had lower p50 and wall time in 7 of 12 runs,
but lower p95 in only 4 of 12. Immutable budget injection and stateless
inspection are not on the ordinary request path, and this measurement finds no
material cache-hit latency change.

Validation passed:

* `cargo fmt --all -- --check`;
* `cargo test --lib` (3,146 passed, 2 ignored);
* `cargo test --features e2e --test tree_worker_process` (9 passed);
* `cargo test --features e2e --test e2e_tree_worker_shadow` (37 passed);
* the profile Python tests (120 passed); and
* `cargo clippy --all-targets --all-features -- -D warnings`.

## Findings

### The admission boundary is now observable end to end

Stage 17 proved accounting and rollback primarily through unit and process
tests. Stage 18 additionally drives a normal release parent and worker across
both pressure boundaries. The observed state distinguishes eviction from an
RSS coincidence: document A has zero derived-cache bytes after pressure,
document B retains its result, and A recomputes identical tokens.

### Rejection preserves useful resident state

Both mutation paths reject before committing an over-budget replacement. The
same worker generation subsequently serves the old tree and accepts a smaller
next-version edit. Memory isolation therefore does not require restarting the
worker or discarding every document when ordinary admission fails.

### Reduced limits validate policy, not a total-RSS cap

The hard budget remains an estimate over retained Rust state and a weighted
Tree node count. It excludes allocator overhead, loaded grammars, code pages,
stacks, transport buffers, and the parent process. Passing this fixture proves
deterministic policy and rollback at the configured estimate; it does not prove
that 512 MiB of configured state implies a 512 MiB process RSS ceiling.

### Default-budget memory remains noisy

Stage 18 does not change default budgets or add ordinary-path retained state.
Even so, its default A/B medians were higher in this run. The large sign-changing
paired ranges prevent attributing that difference to the implementation.
Future memory gates should continue reporting both physical footprint and RSS,
alternate execution order, and require repeatability before rejecting a stage.

## Decision

Keep Stage 18. It makes soft eviction and transactional hard rejection
repeatable across a real release process boundary without a material hot-path
latency change. Do not claim a total-RSS cap, lower default memory use, or a
production-scale pressure result from the reduced-budget fixture. Failure
injection, protocol and compatibility gates, and legacy-path removal remain
required before accepting the ADR.
