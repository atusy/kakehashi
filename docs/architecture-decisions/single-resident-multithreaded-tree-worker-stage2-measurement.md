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
semantic tokens, concurrent documents, or worker restart.

## Environment and method

The committed aggregate is
`benches/profile/results/single_worker_stage2_2026-07-19.json`; the two raw
collector outputs are retained beside it. It records source, executable,
benchmark, and parser SHA-256 identities. The release build ran on an Apple M4
with macOS 26.5.1 and Rust 1.95.0. The Rust grammar source revision is recorded
in the aggregate.

Two consecutive page-cache-warm batches ran 1,000 sequential rounds each. To
avoid the fixed-order reversal seen in Stage 1, even rounds measured direct
then worker and odd rounds measured worker then direct. The worker had four
compute threads, although this single-document sequential workload uses one
document lock at a time.

The standalone edit and derive request frames are 268 and 189 bytes. The fused
request is 279 bytes, 96.5% smaller than Stage 1's 8,069-byte full-text request.

## Results

### Incremental edit and derivation

| Batch | Direct p50 / p95 / p99 | Worker p50 / p95 / p99 | Worker p50 delta |
|---|---:|---:|---:|
| A | 254.2 / 301.2 / 304.4 us | 294.0 / 351.1 / 365.3 us | +15.7% |
| B | 256.2 / 305.9 / 316.7 us | 299.4 / 357.4 / 373.5 us | +16.9% |

| Batch | Direct | Worker | Worker throughput delta |
|---|---:|---:|---:|
| A | 3739.9 req/s | 3212.9 req/s | -14.1% |
| B | 3665.6 req/s | 3140.1 req/s | -14.3% |

The direction and magnitude are stable across the alternating batches. For a
single sequential document, the worker remains about 40--43 us slower at p50
and about 14% lower in throughput than the same incremental work in process.
That residual includes JSON serialization, two pipe crossings, scheduling,
response routing, and parent wakeup.

An exploratory two-request version performed edit and derive separately. It
measured 330--334 us p50 and 2854--2886 req/s. Fusing the operations reduced
worker p50 by about 10--12% and increased throughput by about 10--13%. Those
intermediate outputs are not retained as acceptance evidence, but they exposed
an architectural rule: worker messages should express host operations, not
mirror every internal method call.

Initial full synchronization took 1.58--1.85 ms directly and 2.54--2.73 ms
through the worker. Spawn plus executable-digest handshake took 0.99--1.17 s;
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

# Repeat the preceding cargo bench command with batch_b.json as the output.
```

The aggregate embeds both raw files unchanged. Verify that property with the
same `jq --slurpfile` check used by the Stage 1 measurement before comparing
results.

## Consequence for Stage 2

The stateful protocol is justified by the large reduction in steady-state work,
but this single-document result does not establish a raw performance win over
in-process parsing. Stage 2 must treat the remaining IPC cost as a budget and
seek benefits that the local baseline cannot provide: parallelism across
documents, isolation from native parser failure, reusable worker-local query
state, and coalescing or cancellation of obsolete work.

Before shadow integration, the protocol still needs:

* one ordered mutation stream per document;
* explicit close/removal and incarnation fences;
* stale full-sync rejection;
* injected Markdown and concurrent-document measurements; and
* parent-side comparison against the current authoritative result.

The ADR remains `proposed`.
