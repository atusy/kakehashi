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
source commit `ea2bf541fdada37bfaea76fa69511b20b9a0471f` and the executable,
benchmark, and parser SHA-256 identities. The release build ran on an Apple M4
with macOS 26.5.1 and Rust 1.95.0. The Rust grammar source revision is recorded
in the aggregate.

For each size, two consecutive page-cache-warm batches first ran 1,000
sequential rounds. To avoid the fixed-order reversal seen in Stage 1, even
rounds measured direct then worker and odd rounds measured worker then direct.
Each batch then ran 1,000 edits divided evenly across four documents and four
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
| A | 260.7 / 269.3 / 278.8 us | 303.3 / 316.7 / 331.3 us | +16.3% |
| B | 266.0 / 317.3 / 326.0 us | 309.0 / 368.4 / 379.6 us | +16.2% |

| Batch | Direct | Worker | Worker throughput delta |
|---|---:|---:|---:|
| A | 3825.5 req/s | 3282.1 req/s | -14.2% |
| B | 3538.8 req/s | 3044.5 req/s | -14.0% |

For a single sequential small document, the worker remains about 43 us slower
at p50 and about 14% lower in throughput than the same incremental work in
process. That residual includes JSON serialization, two pipe crossings,
scheduling, response routing, and parent wakeup.

| Batch and order | Direct p50 / p95 / p99 | Worker p50 / p95 / p99 |
|---|---:|---:|
| A, direct first | 326.7 / 421.3 / 456.2 us | 456.6 / 515.9 / 540.2 us |
| B, worker first | 357.9 / 422.8 / 444.6 us | 450.5 / 522.2 / 547.8 us |

| Batch | Direct | Worker | Worker throughput delta |
|---|---:|---:|---:|
| A | 11503.8 req/s | 8671.9 req/s | -24.6% |
| B | 10746.7 req/s | 8797.8 req/s | -18.1% |

The fixed worker scales from 3.04--3.28k sequential requests/s to 8.67--8.80k
requests/s, or 2.6--2.9x, while retaining FIFO order within each document. One
worker therefore uses its internal threads materially; it is not a
single-threaded bottleneck. The process-local baseline remains faster for this
small operation.

### Large document: 79,851 bytes

| Batch | Direct p50 / p95 / p99 | Worker p50 / p95 / p99 | Worker p50 delta |
|---|---:|---:|---:|
| A | 3141.1 / 4010.0 / 4647.1 us | 3181.5 / 4217.2 / 4808.0 us | +1.3% |
| B | 3160.4 / 3614.5 / 4329.5 us | 3207.6 / 3827.6 / 4534.0 us | +1.5% |

| Batch | Direct | Worker | Worker throughput delta |
|---|---:|---:|---:|
| A | 323.9 req/s | 318.1 req/s | -1.8% |
| B | 328.4 req/s | 322.1 req/s | -1.9% |

| Batch and order | Direct p50 / p95 / p99 | Worker p50 / p95 / p99 |
|---|---:|---:|
| A, direct first | 3861.5 / 4696.0 / 5456.7 us | 3977.6 / 4879.7 / 5964.5 us |
| B, worker first | 3883.1 / 4575.6 / 4892.1 us | 3841.4 / 4311.4 / 4606.1 us |

| Batch | Direct | Worker | Worker throughput delta |
|---|---:|---:|---:|
| A | 998.2 req/s | 973.5 req/s | -2.5% |
| B | 1006.3 req/s | 1038.7 req/s | +3.2% |

For the large near-EOF edit, the sequential gap narrows to about 1.8--1.9%.
The concurrent direction reverses with measurement order: the worker is 2.5%
slower in batch A and 3.2% faster in batch B. This is not evidence of a stable
worker win. It shows that the fixed IPC overhead becomes small relative to the
O(n) text clone and point calculation shared by both implementations. The
worker scales 3.1--3.2x from sequential to four document lanes.

Initial full synchronization was 5.11 ms directly versus 2.80 ms through the
worker in small batch A and 1.83 versus 2.87 ms in small batch B. For the large
document it was 12.57 versus 11.07 ms and 12.18 versus 11.74 ms. These cold,
single-sample, order-sensitive values are provenance rather than acceptance
evidence. Spawn plus executable-digest handshake took 1.06--1.17 s; as in Stage
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
  --requests 1000 --threads 4 --lines 200 \
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
exhausted.

The next stage should integrate this data plane in shadow mode: retain the
current in-process result as authoritative, mirror document lifecycle into the
worker, compare owned summaries, and measure real document/injection workloads
before cutover. Supervision, bounded restart/resync, and session quarantine
remain later stages.

The ADR remains `proposed`.
