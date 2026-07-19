# Single Tree Worker Stage 2 Measurement

## Scope

This measurement extends the
[single resident multithreaded tree worker](single-resident-multithreaded-tree-worker.md)
with a worker-owned document replica and incremental tree. It compares two
stateful paths over the same dynamic Rust grammar:

```text
direct: benchmark -> process-local document text + Tree -> edit + incremental parse -> summary
worker: benchmark -> framed byte edit -> resident document text + Tree ->
        edit + incremental parse + summary -> framed response
```

Both paths first synchronize the full document. Each measured round then
replaces the same fixed-width numeric literal near EOF, advances the guarded
content version, incrementally reparses against the edited old tree, and derives
an owned summary. The worker request combines edit and derivation into one
high-level operation so the document lane, parser cache, and IPC boundary are
crossed once.

Two document sizes distinguish fixed IPC cost from size-dependent shared work:

| Workload | Lines | Bytes | Edit location |
|---|---:|---:|---|
| Small | 200 | 7,651 | trailing literal near EOF |
| Large | 2,000 | 79,851 | trailing literal near EOF |

The near-EOF edit deliberately exposes current O(n) text cloning and point
calculation in both paths. This is still a protocol/data-plane probe. It is not
wired into LSP document lifecycle or shadow comparison yet, and it does not
cover injections, queries, semantic tokens, or automatic worker restart.

## Environment and method

The committed aggregate is
`benches/profile/results/single_worker_stage2_2026-07-19.json`; four raw
collector outputs, two per size, are retained beside it. The aggregate records
source commit `8adde114cfafe6f8cb1894e72c90410a37e04a49` and the executable,
benchmark, and parser SHA-256 identities. The release build ran on an Apple M4
with macOS 26.5.1 and Rust 1.95.0. The Rust grammar source revision is recorded
in the aggregate.

For each size, two consecutive page-cache-warm batches first ran 5,000
sequential rounds. To avoid the fixed-order reversal seen in Stage 1, even
rounds measured direct then worker and odd rounds measured worker then direct.
Each batch then ran 5,000 edits divided evenly across four documents and four
caller threads. Batch A measured the concurrent direct path first; batch B
measured the concurrent worker path first. The worker had four compute threads
and a FIFO lane per URI.

The standalone apply, derive, and fused request frames are respectively 274,
191, and 285 bytes for the small workload, and 276, 191, and 287 bytes for the
large workload. The 285-byte small fused request is 96.5% smaller than Stage
1's 8,069-byte full-text request.

## Results

### Small document: 7,651 bytes

| Batch | Direct p50 / p95 / p99 | Worker p50 / p95 / p99 | Worker p50 delta |
|---|---:|---:|---:|
| A | 310.4 / 365.0 / 396.8 us | 376.1 / 450.1 / 501.4 us | +21.2% |
| B | 261.8 / 317.0 / 356.6 us | 303.3 / 417.0 / 463.8 us | +15.9% |

| Batch | Direct | Worker | Worker throughput delta |
|---|---:|---:|---:|
| A | 3238.0 req/s | 2661.1 req/s | -17.8% |
| B | 3587.6 req/s | 3040.5 req/s | -15.2% |

For a single sequential small document, the worker remains 42--66 us slower at
p50 and 15--18% lower in throughput than the same incremental work in process.
That residual includes JSON serialization, two pipe crossings, scheduling,
response routing, parent wakeup, and one atomic retained-byte reservation.

| Batch and order | Direct p50 / p95 / p99 | Worker p50 / p95 / p99 |
|---|---:|---:|
| A, direct first | 350.4 / 416.3 / 464.6 us | 385.6 / 487.1 / 552.1 us |
| B, worker first | 339.8 / 398.7 / 460.7 us | 434.7 / 522.2 / 574.5 us |

| Batch | Direct | Worker | Worker throughput delta |
|---|---:|---:|---:|
| A | 10964.8 req/s | 9880.6 req/s | -9.9% |
| B | 11380.4 req/s | 9008.0 req/s | -20.8% |

The fixed worker scales from 2.66--3.04k sequential requests/s to 9.01--9.88k
requests/s, or 3.0--3.7x, while retaining FIFO order within each document. One
worker therefore uses its internal threads materially; it is not a
single-threaded bottleneck. The process-local baseline remains faster for this
small operation.

### Large document: 79,851 bytes

| Batch | Direct p50 / p95 / p99 | Worker p50 / p95 / p99 | Worker p50 delta |
|---|---:|---:|---:|
| A | 3157.8 / 3405.8 / 4278.8 us | 3206.2 / 3484.7 / 4433.5 us | +1.5% |
| B | 2927.9 / 3262.0 / 3880.3 us | 2968.3 / 3369.9 / 4215.0 us | +1.4% |

| Batch | Direct | Worker | Worker throughput delta |
|---|---:|---:|---:|
| A | 326.0 req/s | 320.9 req/s | -1.6% |
| B | 340.6 req/s | 330.5 req/s | -3.0% |

| Batch and order | Direct p50 / p95 / p99 | Worker p50 / p95 / p99 |
|---|---:|---:|
| A, direct first | 3763.3 / 4469.4 / 5173.4 us | 4091.1 / 4735.8 / 5107.6 us |
| B, worker first | 3998.0 / 4737.4 / 5867.6 us | 3987.2 / 5143.5 / 7213.1 us |

| Batch | Direct | Worker | Worker throughput delta |
|---|---:|---:|---:|
| A | 1029.0 req/s | 963.9 req/s | -6.3% |
| B | 967.2 req/s | 955.8 req/s | -1.2% |

For the large near-EOF edit, the sequential gap narrows to about 1.6--3.0%.
The worker is 6.3% slower in concurrent batch A and 1.2% slower in batch B;
batch B worker p50 is 0.3% faster but its tail and total throughput are worse.
This is not evidence of a worker win. It shows that fixed IPC overhead becomes small relative to the
O(n) text clone and point calculation shared by both implementations. The
worker scales 2.9--3.0x from sequential to four document lanes.

Initial full synchronization was 3.84 ms directly versus 2.59 ms through the
worker in small batch A and 2.36 versus 2.68 ms in small batch B. For the large
document it was 12.95 versus 12.76 ms and 12.01 versus 13.22 ms. These cold,
single-sample, order-sensitive values are provenance rather than acceptance
evidence. Spawn plus executable-digest handshake took 0.35--0.99 s; as in Stage
1, the current implementation hashes the full executable on both sides and is
not representative of a retained immutable session image.

Stage 1 and Stage 2 measure different work: full parsing versus one incremental
edit. Their absolute numbers are therefore not a speedup ratio for equivalent
operations. They do show that retaining text and Tree state removes the
dominant steady-state payload and full-parse cost: small-worker p50 falls from
roughly 1.15 ms to roughly 0.30 ms while the request frame falls from 8,069 to
285 bytes.

An earlier exploratory two-request version performed edit and derive
separately. Fusing the operations improved worker p50 and throughput by roughly
10%. Those intermediate outputs are not retained as acceptance evidence, but
they exposed an architectural rule: worker messages should express host
operations, not mirror every internal method call.

## Reproduction

```sh
cargo build --release --locked --bin kakehashi
stage2_bench_bin="$(cargo bench --locked --bench tree_worker_stage2 \
  --no-run --message-format=json \
  | jq -sr 'map(select(.reason == "compiler-artifact" and
      .target.name == "tree_worker_stage2")) | last | .executable')"
shasum -a 256 target/release/kakehashi "$stage2_bench_bin" \
  deps/test/kakehashi/parser/rust.dylib

cargo bench --locked --bench tree_worker_stage2 -- \
  --bin target/release/kakehashi \
  --parser deps/test/kakehashi/parser/rust.dylib \
  --requests 5000 --threads 4 --lines 200 \
  > benches/profile/results/single_worker_stage2_2026-07-19_batch_a.json

# Repeat with --worker-first and batch_b.json as the output.
# Repeat both orders with --lines 2000 and the large_batch_a/b outputs.
```

The aggregate embeds all four raw files unchanged under `batches.small` and
`batches.large`. Verify that property with `jq --slurpfile` before comparing
results.

## Consequence for Stage 2

The stateful protocol is justified by the large reduction in steady-state work,
but neither size establishes a raw performance win over in-process parsing.
The size sweep instead identifies two optimization boundaries:

* IPC and scheduling dominate tiny incremental operations, so operation fusion,
  batching, coalescing, and cancellation remain important.
* Current O(n) text cloning and point calculation dominate large near-EOF edits,
  so changing the text representation could benefit both paths and reveal the
  worker's fixed overhead more accurately.

The fixed worker nevertheless provides parallelism across documents and the
future isolation boundary that an in-process baseline cannot. The protocol now
has persistent live per-URI FIFO lanes, transactional edit-and-parse, explicit
close/removal, incarnation and generation fences, bounded closed-document
tombstones, and a typed restart-required response when its identity budget is
exhausted. Retained text is also capped at 32 MiB per document and 512 MiB per
worker through an atomic reservation, avoiding a global lane mutex.

The next stage should integrate this data plane in shadow mode: retain the
current in-process result as authoritative, mirror document lifecycle into the
worker, compare owned summaries, and measure real document/injection workloads
before cutover. Supervision, bounded restart/resync, and session quarantine
remain later stages.

The ADR remains `proposed`.
