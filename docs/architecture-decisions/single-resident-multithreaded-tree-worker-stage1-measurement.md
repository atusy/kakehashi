# Single Tree Worker Stage 1 Measurement

## Scope

This is the first real Rust worker-protocol probe for the
[single resident multithreaded tree worker](single-resident-multithreaded-tree-worker.md).
It compares the same dynamic Rust grammar and the same owned host-parse summary
through two paths:

```text
direct: benchmark -> process-local grammar/parser cache -> host parse summary
worker: benchmark -> framed JSON -> one resident Rust worker ->
        four-thread grammar/parser caches -> host parse summary -> framed JSON
```

The response is guarded by request ID, worker generation, URI, document
incarnation, content version, and configuration generation. No `Tree`, `Node`,
`Language`, parser, or other pointer-bearing value crosses the process boundary.
The worker accepts concurrent requests and routes out-of-order responses by
request ID.

This prototype does not yet maintain a worker-side document replica or
incremental `Tree`. Every `DeriveSnapshot` request carries the complete source
text and performs a full parse. It does not run injection discovery, queries,
semantic tokens, captures, or node tracking, so it is not a production
non-regression gate.

## Environment and method

The committed result is
`benches/profile/results/single_worker_stage1_2026-07-19.json`.
It records source, binary, benchmark, and parser SHA-256 digests. The measured
release build ran on an Apple M4 with macOS 26.5.1 and Rust 1.95.0.

Each of two consecutive page-cache-warm batches parsed a generated 200-line
Rust document 1,000 times. The sequential path measured one request at a time.
The parallel path used four caller threads and a four-thread worker budget.
Both paths retained one parser cache per compute thread; the final runs recorded
996 direct and 997 worker cache hits, avoiding the earlier unfair benchmark
variant that recreated direct parsers at Rayon split boundaries.

The request frame was 8,133 bytes, dominated by repeated full text. The worker
reported queue wait and parse/summary compute separately; parent-observed time
also includes JSON encoding/decoding, pipe I/O, wakeup, and response routing.

## Results

### Sequential request latency

| Batch | Direct p50 / p95 / p99 | Worker p50 / p95 / p99 | Worker queue p50 |
|---|---:|---:|---:|
| A | 808.8 / 823.4 / 830.5 µs | 1138.9 / 1372.1 / 1392.0 µs | 5.0 µs |
| B | 730.4 / 825.7 / 838.1 µs | 978.9 / 1163.4 / 1188.4 µs | 4.0 µs |

The parent-observed worker p50 was 41% slower in batch A and 34% slower in
batch B. Queue wait was small. Worker-reported compute was also slower than the
earlier direct interval, so the remainder cannot be labeled pure transport
overhead without an alternating paired collector.

### Four-thread throughput

| Batch | Direct | Worker | Worker delta |
|---|---:|---:|---:|
| A | 2399.8 req/s | 2027.2 req/s | -15.5% |
| B | 2830.0 req/s | 2234.2 req/s | -21.1% |

The worker did use multiple compute threads and preserved almost identical
parser-cache hit rates, but did not improve throughput in this full-text
protocol shape. Warm worker spawn plus handshake was 3.4--3.7 ms. A first run
after relinking observed a 698-ms cold value and is intentionally excluded from
the two warm batches; cold-start acceptance needs a dedicated alternating
collector rather than this microbenchmark.

## Decision and next stage

Stage 1 validates the process, framing, build/protocol handshake, generation
guards, concurrent request router, bounded request deadline, worker-local
grammar/parser caches, and a real high-level host-parse response. It does not
pass a performance gate and makes no performance-win claim.

The result narrows Stage 2. Repeating full document text in every derive request
is the wrong steady-state boundary. Stage 2 must add versioned `SyncDocument`
and `ApplyEdits` messages, keep the latest text and incremental `Tree` in the
worker, and make `DeriveSnapshot` refer only to guarded worker-owned state. Its
measurement must:

* alternate direct and worker order across independent process pairs;
* separate edit application, queue wait, incremental parse, derivation,
  serialization, and parent resume;
* compare stable parser- and tree-cache hit rates;
* include injected Markdown and concurrent documents; and
* reject any apparent throughput win caused by asymmetric cache warmup.

The ADR remains `proposed`.
