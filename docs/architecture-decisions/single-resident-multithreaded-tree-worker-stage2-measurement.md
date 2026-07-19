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

Both paths first synchronize the full 7,611-byte, 200-line document. Each
measured round then replaces the same six-byte numeric literal, advances the
guarded content version, incrementally reparses against the edited old tree,
and derives an owned summary. The worker request combines edit and derivation
into one high-level operation so the document mutex, parser cache, and IPC
boundary are crossed once.

This is still a protocol/data-plane probe. It is not wired into LSP document
lifecycle or shadow comparison yet, and it does not cover injections, queries,
semantic tokens, or worker restart.

## Environment and method

The committed aggregate is
`benches/profile/results/single_worker_stage2_2026-07-19.json`; the two raw
collector outputs are retained beside it. It records source, executable,
benchmark, and parser SHA-256 identities. The release build ran on an Apple M4
with macOS 26.5.1 and Rust 1.95.0. The Rust grammar source revision is recorded
in the aggregate.

Two consecutive page-cache-warm batches first ran 1,000 sequential rounds each.
To avoid the fixed-order reversal seen in Stage 1, even rounds measured direct
then worker and odd rounds measured worker then direct. Each batch then ran
1,000 edits divided evenly across four documents and four caller threads. Batch
A measured the concurrent direct path first; batch B measured the concurrent
worker path first. The worker had four compute threads and a FIFO lane per URI.

The standalone edit and derive request frames are 268 and 189 bytes. The fused
request is 279 bytes, 96.5% smaller than Stage 1's 8,069-byte full-text request.

## Results

### Incremental edit and derivation

| Batch | Direct p50 / p95 / p99 | Worker p50 / p95 / p99 | Worker p50 delta |
|---|---:|---:|---:|
| A | 254.4 / 260.6 / 271.5 us | 294.3 / 308.7 / 333.5 us | +15.7% |
| B | 252.2 / 259.7 / 271.4 us | 293.3 / 304.7 / 317.0 us | +16.3% |

| Batch | Direct | Worker | Worker throughput delta |
|---|---:|---:|---:|
| A | 3919.2 req/s | 3378.1 req/s | -13.8% |
| B | 3953.9 req/s | 3396.1 req/s | -14.1% |

The direction and magnitude are stable across the alternating batches. For a
single sequential document, the worker remains about 40--41 us slower at p50
and about 14% lower in throughput than the same incremental work in process.
That residual includes JSON serialization, two pipe crossings, scheduling,
response routing, and parent wakeup.

An exploratory two-request version performed edit and derive separately. It
measured 330--334 us p50 and 2854--2886 req/s. Fusing the operations reduced
worker p50 by about 10--12% and increased throughput by about 10--13%. Those
intermediate outputs are not retained as acceptance evidence, but they exposed
an architectural rule: worker messages should express host operations, not
mirror every internal method call.

### Four concurrent document lanes

| Batch and order | Direct p50 / p95 / p99 | Worker p50 / p95 / p99 |
|---|---:|---:|
| A, direct first | 293.0 / 392.7 / 415.7 us | 434.9 / 499.5 / 531.7 us |
| B, worker first | 315.3 / 392.3 / 431.8 us | 367.8 / 463.0 / 495.9 us |

| Batch | Direct | Worker | Worker throughput delta |
|---|---:|---:|---:|
| A | 12504.4 req/s | 9082.5 req/s | -27.4% |
| B | 12382.8 req/s | 10454.5 req/s | -15.6% |

The fixed worker scales from about 3.39k sequential requests/s to 9.08--10.45k
requests/s, or 2.7--3.1x, while retaining FIFO order within each document. Thus
one worker does use its internal threads materially; it is not a single-threaded
bottleneck. The process-local baseline remains faster at 12.38--12.50k
requests/s. Reversing the concurrent path order materially changes worker p50
and throughput, so the interval is reported rather than selecting the favorable
batch. A larger repeated randomized collector is required for an acceptance
threshold.

Initial full synchronization took 1.95--2.20 ms directly and 2.55--2.60 ms
through the worker. Spawn plus executable-digest handshake took 1.00--1.07 s;
as in Stage 1, the current implementation hashes the full executable on both
sides and is not representative of a retained immutable session image.

Stage 1 and Stage 2 measure different work: full parsing versus one incremental
edit. Their absolute numbers are therefore not a speedup ratio for equivalent
operations. They do show that retaining text and Tree state removes the dominant
steady-state payload and full-parse cost: worker p50 falls from roughly 1.15 ms
to roughly 0.30 ms while the request frame falls from 8,069 to 279 bytes.

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
```

The aggregate embeds both raw files unchanged. Verify that property with the
same `jq --slurpfile` check used by the Stage 1 measurement before comparing
results.

## Consequence for Stage 2

The stateful protocol is justified by the large reduction in steady-state work,
but neither workload establishes a raw performance win over in-process parsing.
Stage 2 must treat the remaining IPC cost as a budget and
seek benefits that the local baseline cannot provide: parallelism across
documents, isolation from native parser failure, reusable worker-local query
state, and coalescing or cancellation of obsolete work.

The protocol now has ordered per-URI mutation lanes, explicit close/removal,
incarnation fences, and stale full-sync rejection. Before shadow integration it
still needs:

* injected Markdown measurement;
* repeated randomized concurrent batches and fairness measurement; and
* parent-side comparison against the current authoritative result.

The ADR remains `proposed`.
