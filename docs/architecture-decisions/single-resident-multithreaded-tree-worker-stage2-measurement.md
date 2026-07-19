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
source commit `40a8fc5a7e65b8b96b49a461bec4e77de9cb67d0` and the executable,
benchmark, and parser SHA-256 identities. The release build ran on an Apple M4
with macOS 26.5.1 and Rust 1.95.0. The Rust grammar source revision is recorded
in the aggregate. Commit `a3455ffef` subsequently changed only poisoned-lock
handling, a restart-only branch that none of these benchmark operations enter;
the measured hot path is unchanged.

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
| A | 309.4 / 357.4 / 390.9 us | 372.6 / 441.3 / 489.0 us | +20.4% |
| B | 304.9 / 318.5 / 362.3 us | 352.5 / 417.7 / 482.9 us | +15.6% |

| Batch | Direct | Worker | Worker throughput delta |
|---|---:|---:|---:|
| A | 3325.9 req/s | 2741.3 req/s | -17.6% |
| B | 3448.8 req/s | 2904.4 req/s | -15.8% |

For a single sequential small document, the worker remains 48--63 us slower at
p50 and 16--18% lower in throughput than the same incremental work in process.
That residual includes JSON serialization, two pipe crossings, scheduling,
response routing, parent wakeup, and one atomic retained-byte reservation.

| Batch and order | Direct p50 / p95 / p99 | Worker p50 / p95 / p99 |
|---|---:|---:|
| A, direct first | 351.7 / 388.8 / 424.2 us | 398.2 / 514.9 / 623.8 us |
| B, worker first | 346.9 / 378.0 / 409.5 us | 432.5 / 498.8 / 582.8 us |

| Batch | Direct | Worker | Worker throughput delta |
|---|---:|---:|---:|
| A | 11298.5 req/s | 9517.4 req/s | -15.8% |
| B | 11428.7 req/s | 9124.0 req/s | -20.2% |

The fixed worker scales from 2.74--2.90k sequential requests/s to 9.12--9.52k
requests/s, or 3.1--3.5x, while retaining FIFO order within each document. One
worker therefore uses its internal threads materially; it is not a
single-threaded bottleneck. The process-local baseline remains faster for this
small operation.

### Large document: 79,851 bytes

| Batch | Direct p50 / p95 / p99 | Worker p50 / p95 / p99 | Worker p50 delta |
|---|---:|---:|---:|
| A | 2687.5 / 3372.2 / 4242.1 us | 2749.8 / 3633.3 / 4520.9 us | +2.3% |
| B | 2669.0 / 3231.7 / 3580.8 us | 2738.8 / 3320.9 / 3833.9 us | +2.6% |

| Batch | Direct | Worker | Worker throughput delta |
|---|---:|---:|---:|
| A | 342.7 req/s | 333.0 req/s | -2.9% |
| B | 356.2 req/s | 347.1 req/s | -2.6% |

| Batch and order | Direct p50 / p95 / p99 | Worker p50 / p95 / p99 |
|---|---:|---:|
| A, direct first | 3842.7 / 4537.4 / 5085.5 us | 3968.2 / 5030.0 / 10045.9 us |
| B, worker first | 3994.5 / 4785.8 / 6206.3 us | 3976.4 / 4747.2 / 6533.5 us |

| Batch | Direct | Worker | Worker throughput delta |
|---|---:|---:|---:|
| A | 1016.5 req/s | 943.6 req/s | -7.2% |
| B | 971.2 req/s | 965.7 req/s | -0.6% |

For the large near-EOF edit, the sequential gap narrows to about 2.6--2.9%.
The worker is 7.2% slower in concurrent batch A and 0.6% slower in batch B;
batch B worker p50 and p95 are slightly faster but its p99 and total throughput are worse.
This is not evidence of a worker win. It shows that fixed IPC overhead becomes small relative to the
O(n) text clone and point calculation shared by both implementations. The
worker scales 2.8x from sequential to four document lanes.

Initial full synchronization was 1.18 ms directly versus 1.38 ms through the
worker in small batch A and 2.18 versus 2.60 ms in small batch B. For the large
document it was 8.65 versus 9.07 ms and 12.55 versus 12.84 ms. These cold,
single-sample, order-sensitive values are provenance rather than acceptance
evidence. Spawn plus executable-digest handshake took 0.22--0.35 s; as in Stage
1, the current implementation hashes the full executable on both sides and is
not representative of a retained immutable session image.

Stage 1 and Stage 2 measure different work: full parsing versus one incremental
edit. Their absolute numbers are therefore not a speedup ratio for equivalent
operations. They do show that retaining text and Tree state removes the
dominant steady-state payload and full-parse cost: small-worker p50 falls from
roughly 1.15 ms to roughly 0.35 ms while the request frame falls from 8,069 to
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
