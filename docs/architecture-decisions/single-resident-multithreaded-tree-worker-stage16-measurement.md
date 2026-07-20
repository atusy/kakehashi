# Single Tree Worker Stage 16 Cache-Pressure Measurement

## Scope

Stage 16 adds document-local soft pressure hygiene. When a worker semantic-token
cache retains more than 2 MiB of allocated token storage, the document keeps
that exact-version semantic cache but drops recomputable auxiliary state:

* semantic injection discovery;
* resolved injection regions and virtual content;
* injection-region token caches; and
* lazily parsed injection layers.

The worker retains source text, the host Tree, semantic tokens, and node-tracker
identity. It therefore does not restart a healthy worker, invalidate public node
IDs, or sacrifice the large-result cache hit that avoids recomputation.

This is a soft cache policy, not a complete process-memory admission bound.

## Method

### Memory

The Stage 15 baseline and Stage 16 candidate release binaries both ran in
authoritative mode with one worker and four compute threads. The enhanced
Stage 15 collector opened three copies of the 1,121,182-byte, 5,000-injection
Markdown fixture. It warmed semantic tokens for each document, round-robined six
measured requests, and held all documents open for stable sampling.

Six batches alternated baseline-candidate and candidate-baseline order. The
collector sampled aggregate RSS every 20 ms and macOS shared-aware physical
footprint every 250 ms. Raw results are retained in
`benches/profile/results/single_worker_stage16_cache_pressure_multidoc_2026-07-20.json`.

### Cache-hit latency

Because macOS `footprint` inspection perturbs target timing, latency was measured
separately without a memory sampler. Six order-reversed runs per binary used one
5,000-injection document and 20 measured exact-version semantic requests after
warmup.

### Correctness

Unit tests establish both sides of the threshold. An oversized semantic cache
drops auxiliary state while retaining semantic tokens, Tree, document version,
and node identity; a small cache keeps auxiliary state. The full worker process
and authoritative/shadow E2E suites cover the unchanged public lifecycle.

## Result

Three-document memory medians were:

| Metric | Stage 15 | Stage 16 | Change |
| --- | ---: | ---: | ---: |
| Stable physical footprint | 488.99 MiB | 472.17 MiB | -16.82 MiB (-3.4%) |
| Peak physical footprint | 513.13 MiB | 494.60 MiB | -18.52 MiB (-3.6%) |
| Stable aggregate RSS | 531.17 MiB | 519.06 MiB | -12.11 MiB (-2.3%) |

The stable physical-footprint maximum narrowed from 503.39 MiB to 497.26 MiB,
while the peak maximum narrowed from 521.49 MiB to 514.00 MiB.

Unsampled exact-version latency medians were:

| Metric | Stage 15 | Stage 16 | Change |
| --- | ---: | ---: | ---: |
| p50 | 81.95 ms | 81.20 ms | -0.9% |
| p95 | 85.90 ms | 86.60 ms | +0.8% |

The latency differences are below the observed run-to-run variation. Preserving
the semantic cache avoided a measurable cache-hit regression.

Validation passed:

* `cargo fmt --all -- --check`;
* `cargo check --lib --tests --features e2e`;
* `cargo test --lib` (3,132 passed, 2 ignored);
* `cargo test --features e2e --test tree_worker_process` (4 passed);
* `cargo test --features e2e --test e2e_tree_worker_shadow` (37 passed); and
* profile Python tests (119 passed).

## Findings

### Logical eviction is not immediate RSS reclamation

A single-document trial did not improve physical footprint. The allocator can
retain pages after Rust drops many small cache allocations, so immediate OS
memory is a poor test of logical eviction. With three documents, later
allocations can reuse released space and the cumulative stable distribution
improves. This is why the final fixture measures multiple simultaneously open
documents rather than one post-request snapshot.

### Retaining the expensive result isolates a useful tradeoff

Dropping all worker semantic state would reduce more memory but force the next
exact-version request to repeat injection discovery, parsing, and tokenization.
Stage 16 instead keeps the 2.35 MiB semantic result in the measured large
fixture and removes auxiliary copies. The modest memory win arrives without a
latency loss.

### A token-size threshold is only a proxy

The semantic allocation is observable and cheap to count, but it does not
directly measure Trees, resolved virtual content, captures caches, or allocator
pages. Documents with small semantic output can still have expensive derived
state, and several individually sub-threshold documents can exceed a global
budget. Stage 16 therefore improves hygiene but does not close the ADR memory
gate.

### Hard pressure needs worker-wide accounting

The next memory step requires document-scoped estimates across all derived
caches, a worker-wide soft and hard budget, deterministic eviction order, and a
bounded rejection contract when non-evictable state alone exceeds the hard
limit. OS RSS must remain measurement evidence rather than an unstable runtime
correctness input.

## Decision

Keep the Stage 16 auxiliary trim. It reduces cumulative multi-document memory
and tightens the observed tail without regressing exact-version latency or node
identity. Continue to treat the current text budget plus token-size trigger as
soft safeguards, not proof of bounded total memory. Worker-wide derived-state
accounting and concurrent-document admission remain required before accepting
the ADR or removing the legacy path.
