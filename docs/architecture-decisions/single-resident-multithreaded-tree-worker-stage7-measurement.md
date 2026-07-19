# Single Tree Worker Stage 7 Authoritative-Reader Measurement

## Scope

Stage 7 adds a guarded `authoritative` rollout mode to the
[single resident multithreaded tree worker](single-resident-multithreaded-tree-worker.md).
In this mode, public tree readers consume worker-owned nodes, captures, native
bindings, selection ranges, semantic tokens, injection regions, bridge
contexts, formatting regions, diagnostic regions, and exact live region
geometry. After `didChange`, the parent does not schedule its local reparse;
virtual-document lifecycle and diagnostics continue from worker facts.

This is an intermediate ownership measurement, not acceptance of the ADR. The
parent still contains the legacy parser/snapshot implementation for rollback,
and Stage 8 must remove authoritative-mode didOpen/install dependencies on
parent parsing before the guarded cutover is structurally complete.

## Questions

1. Does one authoritative worker avoid the shadow mode's duplicate parent
   reparse cost?
2. Can worker-owned caches make current-version repeated reads faster?
3. Does increasing the one worker's internal thread budget hide IPC and
   cache-miss costs?
4. Which work must be fused before the rollout can meet a non-regression gate?

## Method

Measurements used the release `kakehashi` binary built from Stage 7 on macOS.
The existing LSP semantic-token harness drove the same binary for every mode:

```text
iterations = 40
warmup = 10
legacy = KAKEHASHI_TREE_WORKER_MODE unset
worker-4 = authoritative, KAKEHASHI_TREE_WORKER_THREADS=4
worker-8 = authoritative, KAKEHASHI_TREE_WORKER_THREADS=8
```

The harness sends real `didOpen`, `didChange`, semantic full, and semantic
full/delta messages. The edit cases therefore include incremental parse,
injection lifecycle, diagnostic scheduling, semantic discovery, token shaping,
and IPC rather than timing an isolated tree walk.

## Results

Lower is better. Values are medians unless a percentile is named.

| Scenario | Legacy | Authoritative, 4 threads | Authoritative, 8 threads |
|---|---:|---:|---:|
| Markdown, 60 injections, full | 0.305 ms | 0.339 ms | 0.395 ms |
| Markdown, 60 injections, edit + delta | 7.009 ms | 11.746 ms | 10.387 ms |
| Large Rust, edit + delta | 29.568 ms | 64.686 ms | 69.315 ms |

The large Rust distribution is bimodal:

| Percentile | Legacy | Authoritative, 4 threads | Authoritative, 8 threads |
|---|---:|---:|---:|
| p25, cache-hit side | 18.565 ms | 0.725 ms | 0.712 ms |
| p75, cache-miss side | 32.022 ms | 70.256 ms | 77.039 ms |

Earlier raw resident-worker throughput measurements explain why extra internal
threads help injection-heavy work but not this single-host miss:

| Payload | 1 thread | 2 threads | 4 threads | 8 threads |
|---|---:|---:|---:|---:|
| Small, approximately 7.7 KiB | 2,859 RPS | 5,102 RPS | 7,280 RPS | 14,222 RPS |
| Large, approximately 80 KiB | 351 RPS | 538 RPS | 852 RPS | 1,120 RPS |

## Findings

### Parent reparse removal is necessary but insufficient

Stopping the authoritative parent reparse removed about 1.6 ms from the
Markdown edit path during development. Removing an eager duplicate worker
region derivation removed a further approximately 3.1 ms. Those improvements
confirm that duplicate ownership and duplicate discovery are material, but the
complete post-edit workflow still remains slower than legacy.

### Worker cache hits can be substantially faster

The large Rust p25 fell from 18.565 ms to approximately 0.7 ms. A resident
worker can therefore turn an exact current-version repeated request into a
cheap cache read and process round trip. The median alone hides this win because
the alternating edit corpus also forces expensive misses.

### Worker cache misses are the current blocker

The large Rust p75 more than doubled. The miss path combines worker-side token
calculation with process serialization and a large response payload. Switching
the internal protocol from JSON to bincode improved the large-miss distribution
modestly, but encoding alone does not close the gap.

### Internal threads help parallel regions, not a serial host miss

Eight threads improved Markdown edit + delta from 11.746 ms to 10.387 ms, but
it remained 48% slower than the 7.009 ms legacy result. Eight threads did not
improve the large Rust miss. This supports the ADR's one-process internal pool,
but rejects thread-count tuning as the primary Stage 8 optimization.

### Shared facts must be lazy and generation-fenced

Bridge lifecycle, diagnostics, and semantic tokens can request injection facts
for the same document version concurrently. Stage 7 shares resolved injection
facts and semantic discovery inside the worker and invalidates them on edit or
configuration generation change. Forcing all facts eagerly before they are
needed did not improve edit latency and regressed idle full reads, so the final
Stage 7 path keeps semantic-only and region-consuming work lazy.

## Consequence for the next stage

Stage 7 validates the authoritative reader contracts and the potential of a
resident worker cache. It does not pass a general performance non-regression
gate. Stage 8 should prioritize:

1. removing the remaining authoritative parent parser/snapshot ownership;
2. single-flight derivation for identical document/version/generation facts;
3. avoiding large token payload transfers on cache hits and deltas;
4. measuring compute, serialization, bytes, queue wait, and parent resume
   separately for the large miss; and
5. preserving lazy derivation so bridge-only work does not penalize
   semantic-only documents, and vice versa.

The worker count remains fixed at one. These measurements do not justify
sharding: the limiting path is per-request miss work and payload transfer, not
aggregate resident-worker throughput.
