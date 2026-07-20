# Single Tree Worker Stage 15 Memory Measurement

## Scope

Stage 15 adds a reproducible parent-plus-worker memory collector and measures
the fixed and document-scaled memory cost of the authoritative worker against
the legacy in-process path. It does not change runtime admission yet.

The existing worker limits a single document to 32 MiB, retained document text
to 512 MiB, and document identities to 4,096. Those counters cover worker-owned
text, but not the parent copy, Trees, injection facts, semantic caches, parser
and query state, response buffers, or allocator retention. This measurement
tests whether the text-only counter can stand in for total memory pressure.

## Method

The release Stage 15 binary was run in legacy and authoritative modes on macOS
with one worker process and four compute threads. Six batches alternated the
variant order. Every run used a fresh XDG directory and the same staged binary,
queries, parsers, driver, and collector hashes.

The collector samples the complete process tree rooted below the benchmark
driver:

* aggregate RSS every 10 ms; and
* macOS shared-aware physical footprint every 250 ms.

After semantic-token warmup and the measured requests, the driver holds the
initialized server open for one second. The stable value is the median of the
second half of samples; the reported result is the median across six runs.
Physical footprint is the primary metric because summing per-process RSS counts
shared pages more than once.

Two generated Markdown documents were measured:

| Workload | Source bytes | Requests |
| --- | ---: | ---: |
| Small | 32,550 | 500 |
| Large | 1,121,182 | 10 |

The retained raw results are
`benches/profile/results/single_worker_stage15_memory_small_2026-07-20.json`
and
`benches/profile/results/single_worker_stage15_memory_large_2026-07-20.json`.

## Result

Stable physical-footprint medians were:

| Workload | Legacy | Authoritative | Absolute change | Relative change |
| --- | ---: | ---: | ---: | ---: |
| Small | 16.77 MiB | 23.42 MiB | +6.65 MiB | +39.6% |
| Large | 190.67 MiB | 224.06 MiB | +33.38 MiB | +17.5% |

Peak physical-footprint medians were:

| Workload | Legacy | Authoritative | Absolute change | Relative change |
| --- | ---: | ---: | ---: | ---: |
| Small | 16.77 MiB | 23.42 MiB | +6.65 MiB | +39.6% |
| Large | 190.67 MiB | 225.06 MiB | +34.38 MiB | +18.0% |

Aggregate RSS showed the expected larger shared-page overcount:

| Workload | Legacy | Authoritative | Absolute change | Relative change |
| --- | ---: | ---: | ---: | ---: |
| Small stable | 32.80 MiB | 47.27 MiB | +14.47 MiB | +44.1% |
| Large stable | 232.99 MiB | 270.24 MiB | +37.25 MiB | +16.0% |

The collector also records request latency to validate successful work, but
`footprint` temporarily inspects the target processes and materially perturbs
timing. Those latency fields are not performance evidence; Stages 9, 13, and 14
remain the relevant latency measurements.

## Findings

### One resident worker has a measurable fixed memory cost

For the 32 KiB document, the shared-aware increase was 6.65 MiB. This is the
cost of the second process, its bounded compute threads, worker-side language
state, and duplicated live document-derived state. It is substantially smaller
than the 14.47 MiB suggested by summing RSS, which confirms that RSS alone is an
overly pessimistic acceptance gate on this platform.

### Text duplication is not the dominant scaling term

Increasing source text by about 1.04 MiB increased the authoritative-minus-
legacy stable footprint gap by about 26.73 MiB. A second copy of source text
cannot explain that slope. The worker also retains a Tree, injection discovery
and resolved-region facts, injection token caches, and semantic tokens, while
the parent retains reconstructable text and publication caches. IPC and
allocator buffers retain additional peak capacity.

### The current retained-text budget is not a total-memory budget

A 1.07 MiB source remained far below the 512 MiB retained-text ceiling while
the combined authoritative physical footprint reached 224 MiB. The text counter
is still useful for transactional sync validation, but it cannot protect the
single worker's queueing and memory-failure domain by itself.

### Memory admission must account or evict derived state

Blindly lowering the text limit would reject ordinary source based on a weak
proxy and would not distinguish cheap text from injection-heavy derived state.
A useful next gate needs worker-reported accounting for at least cached semantic
tokens, resolved injection content, and other document-scoped derived caches,
plus deterministic eviction or admission behavior. The ADR forbids routinely
recycling healthy workers merely for memory pressure because restart invalidates
node identities.

## Decision

Keep the fixed one-worker design, but do not pass the ADR memory gate yet. The
measured cost is bounded for these fixtures and does not motivate worker
sharding, but the current text-only budget is insufficient evidence against
memory exhaustion. Add derived-state accounting and pressure-driven cache
eviction/admission, then repeat this collector across concurrent documents
before legacy removal.
