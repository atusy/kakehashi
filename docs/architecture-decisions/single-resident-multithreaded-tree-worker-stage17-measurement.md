# Single Tree Worker Stage 17 Memory-Admission Measurement

## Scope

Stage 17 replaces document-size proxies with worker-wide estimated-memory
accounting. It separately tracks non-evictable document state, reusable result
caches, and recomputable auxiliary caches. A 128 MiB soft budget triggers a
deterministic largest-first cache sweep, while a 512 MiB independent hard budget
rejects growth of retained source and host-tree state transactionally.

The Tree estimate uses 64 bytes per visible node because Tree-sitter does not
expose native allocation size. The hard limit is therefore a deterministic
admission contract over a conservative model, not a claim that process RSS is
exactly bounded to 512 MiB.

## Method

### Memory

The Stage 16 baseline and Stage 17 candidate release binaries ran in
authoritative mode with one worker and four compute threads. Each batch opened
three copies of the 1,121,182-byte, 5,000-injection Markdown fixture, warmed
semantic tokens for every document, round-robined six measured requests, and
held the documents open for stable sampling.

Six batches alternated baseline-candidate and candidate-baseline order. The
collector sampled aggregate RSS every 20 ms and macOS shared-aware physical
footprint every 250 ms. Raw results are retained in
`benches/profile/results/single_worker_stage17_memory_admission_multidoc_2026-07-20.json`.

### Cache-hit latency

Latency was measured separately without `footprint` sampling. Six
order-reversed runs per binary used one 5,000-injection document and 20 measured
exact-version semantic requests after warmup. Raw per-run results and binary
hashes are retained in
`benches/profile/results/single_worker_stage17_memory_admission_cache_hit_2026-07-20.json`.

### Correctness

Unit tests cover every accounted memory class, deterministic eviction order,
soft-budget enforcement, transactional hard admission, counter replacement and
release, and the exact-version checkpoint bypass. The process and
authoritative/shadow E2E suites cover the unchanged public lifecycle.

## Result

Three-document memory medians were:

| Metric | Stage 16 | Stage 17 | Change |
| --- | ---: | ---: | ---: |
| Stable physical footprint | 463.75 MiB | 472.49 MiB | +8.74 MiB (+1.9%) |
| Peak physical footprint | 493.99 MiB | 491.89 MiB | -2.10 MiB (-0.4%) |
| Stable aggregate RSS | 545.77 MiB | 533.81 MiB | -11.96 MiB (-2.2%) |
| Peak aggregate RSS | 546.73 MiB | 541.74 MiB | -4.98 MiB (-0.9%) |

The maximum stable physical footprint narrowed from 494.33 MiB to 487.52 MiB,
while the peak maximum widened from 500.30 MiB to 505.36 MiB. The metrics move
in opposite directions and vary more between runs than between medians; this
fixture does not show a material memory change.

Unsampled exact-version latency medians were:

| Metric | Stage 16 | Stage 17 | Change |
| --- | ---: | ---: | ---: |
| p50 | 75.45 ms | 76.25 ms | +1.1% |
| p95 | 79.85 ms | 80.30 ms | +0.6% |
| Wall time per cycle | 117.78 ms | 119.10 ms | +1.1% |

These differences are below the observed run-to-run variation. Skipping the
worker-wide pressure scan on an exact-version result-cache hit keeps the hot
path effectively unchanged.

Validation passed:

* `cargo fmt --all -- --check`;
* `cargo check --lib --tests --features e2e`;
* `cargo test --lib`;
* `cargo test --features e2e --test tree_worker_process` (4 passed);
* `cargo test --features e2e --test e2e_tree_worker_shadow` (37 passed); and
* `cargo clippy --all-targets --all-features -- -D warnings`.

## Findings

### Accounting can improve the contract without lowering ordinary RSS

The measured three-document fixture stays below the new soft budget after
Stage 16 has already removed oversized auxiliary caches. Stage 17 should not
evict additional state in this case, and the mixed physical-footprint and RSS
deltas confirm no repeatable memory reduction. Its value here is making memory
classes visible and enforceable before the process reaches an OS-defined
failure state.

### The hot-path checkpoint must remain conditional

The first implementation checkpointed every semantic request, which would scan
all open documents even when the exact-version result already existed. Stage 17
instead checkpoints only requests that can grow caches. The measured +0.6% to
+1.1% medians are noise-scale evidence that the admission machinery does not
materially tax the common cache-hit path.

### Native Tree memory remains the largest uncertainty

Rust-owned buffers can be counted from capacities, but Tree-sitter exposes node
count rather than allocation bytes. Weighting each visible node by 64 bytes
makes admission deterministic and conservative enough to prevent unbounded
growth, but it can both overestimate compact trees and underestimate hidden
allocator overhead. RSS remains supporting measurement evidence, not a runtime
input or the hard-limit definition.

### A hard-limit stress benchmark is still useful

Tests prove rejection and rollback at a synthetic boundary without allocating
hundreds of MiB. This measurement deliberately exercises the admitted common
case, so it does not quantify latency or reclamation when the 128 MiB soft
budget or 512 MiB hard budget is crossed. A configurable-limit stress fixture
would make that path observable without destabilizing the benchmark host.

## Decision

Keep the Stage 17 accounting and admission policy. It establishes deterministic
soft eviction and transactional hard rejection without a material common-case
memory or latency regression. Do not claim an RSS cap or a memory reduction from
this fixture. Failure injection, configurable-boundary stress, protocol and
compatibility gates, and legacy-path removal remain required before accepting
the ADR.
