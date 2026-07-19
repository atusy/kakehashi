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

The prototype authenticates the exact bytes at the caller-supplied spawn path
against the image observed by the child. It does not yet bind that path to the
parent's running image or retain the immutable session-private executable
snapshot required by the final ADR. That parent-image binding remains a
cutover prerequisite, not a security property claimed by Stage 1.

This prototype does not yet maintain a worker-side document replica or
incremental `Tree`. Every `DeriveSnapshot` request carries the complete source
text and performs a full parse. It does not run injection discovery, queries,
semantic tokens, captures, or node tracking, so it is not a production
non-regression gate.

## Environment and method

The committed result is
`benches/profile/results/single_worker_stage1_2026-07-19.json`.
It records source, binary, benchmark, and parser SHA-256 digests, and retains
the two raw collector outputs beside the aggregate. The measured release build
ran on an Apple M4 with macOS 26.5.1 and Rust 1.95.0. The Rust grammar came
from `tree-sitter/tree-sitter-rust` revision
`77a3747266f4d621d0757825e6b11edcbf991ca5`; its recorded digest is the final
identity check.

Each of two consecutive page-cache-warm batches parsed a generated 200-line
Rust document 1,000 times. The sequential path measured one request at a time.
The parallel path used four caller threads and a four-thread worker budget.
Both paths retained one parser cache per compute thread; the final runs recorded
996 direct and 997 worker cache hits in each batch, avoiding the earlier unfair benchmark
variant that recreated direct parsers at Rayon split boundaries.

The request frame was 8,069 bytes, dominated by repeated full text. The worker
reported queue wait and parse/summary compute separately; parent-observed time
also includes JSON encoding/decoding, pipe I/O, wakeup, and response routing.

## Results

### Sequential request latency

| Batch | Direct p50 / p95 / p99 | Worker p50 / p95 / p99 | Worker queue p50 |
|---|---:|---:|---:|
| A | 957.3 / 1084.5 / 1092.5 µs | 1147.5 / 1170.3 / 1189.5 µs | 4.7 µs |
| B | 1274.5 / 1304.0 / 1328.0 µs | 1175.4 / 1396.2 / 1419.6 µs | 5.2 µs |

The parent-observed worker p50 was 20% slower in batch A and 8% faster in batch
B. This reversal under a fixed direct-then-worker order is evidence that the
sequential difference is smaller than run-order noise, not evidence of a worker
win. Queue wait remained small. The remainder cannot be labeled pure transport
overhead without an alternating paired collector.

### Four-thread throughput

| Batch | Direct | Worker | Worker delta |
|---|---:|---:|---:|
| A | 2440.2 req/s | 2381.1 req/s | -2.4% |
| B | 2133.8 req/s | 2527.4 req/s | +18.4% |

The worker did use all four compute threads and preserved the expected cache
hit rates. The two batches disagree on the throughput direction, so this
fixed-order collector cannot establish either a win or a regression. Exact
executable SHA-256 authentication added after review made spawn plus handshake
1.09--1.14 s in these runs, dominated by hashing the 34 MiB parent and child
images with the current implementation. The final retained
executable snapshot should hash once and reuse its identity across worker
generations; cold-start acceptance needs a dedicated alternating collector.

An intermediate measurement exposed a sender-mutex guard accidentally retained
across response wait: throughput fell to 693--818 req/s and p99 reached
117--140 ms. Releasing the guard immediately after bounded queue admission
restored 2230--2237 req/s. Those defective runs are not part of the committed
results.

A second intermediate implementation used `Rayon::scope` to remove lifetime
completion messages. The control scope consumed one compute slot, shifted the
worker cache-hit count from 997 to 998, and reduced throughput to 1597--1815
req/s. Waiting for all bounded admission permits at EOF instead preserved four
compute threads without retaining per-request completion history. Those runs
are also excluded.

## Reproduction

The measured artifacts were prepared and collected with:

```sh
cargo build --release --locked --bin kakehashi
target/release/kakehashi language install rust \
  --data-dir deps/test/kakehashi --force
stage1_bench_bin="$(cargo bench --locked --bench tree_worker_stage1 \
  --no-run --message-format=json \
  | jq -sr 'map(select(.reason == "compiler-artifact" and \
      .target.name == "tree_worker_stage1")) | last | .executable')"
shasum -a 256 target/release/kakehashi "$stage1_bench_bin" \
  deps/test/kakehashi/parser/rust.dylib

cargo bench --locked --bench tree_worker_stage1 -- \
  --bin target/release/kakehashi \
  --parser deps/test/kakehashi/parser/rust.dylib \
  --requests 1000 --threads 4 --lines 200 \
  > benches/profile/results/single_worker_stage1_2026-07-19_batch_a.json

# Repeat the preceding cargo bench command with batch_b.json as the output.
```

Before comparing results, the parser revision and all three recorded SHA-256
values must match the aggregate JSON. The raw files are the collector output;
the aggregate embeds them unchanged under `batches` plus environment and
artifact provenance.

The embedding can be checked without trusting the prose:

```sh
jq -e --slurpfile a \
  benches/profile/results/single_worker_stage1_2026-07-19_batch_a.json \
  --slurpfile b \
  benches/profile/results/single_worker_stage1_2026-07-19_batch_b.json \
  '.batches == [$a[0], $b[0]]' \
  benches/profile/results/single_worker_stage1_2026-07-19.json
```

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
