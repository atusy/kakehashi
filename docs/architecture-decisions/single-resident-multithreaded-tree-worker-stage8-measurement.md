# Single Tree Worker Stage 8 Ownership Measurement

## Scope

Stage 8 moves authoritative `didOpen` parsing and parser-install recovery into
the resident worker, preserves worker replicas incrementally across edits, and
adds current-version semantic-fact reuse. It also avoids deriving injection
facts for languages without an injection query and avoids producing a full
snapshot merely to acknowledge an authoritative edit.

This remains an intermediate measurement. Authoritative startup still asks the
parent language coordinator to load grammar and query metadata, and the legacy
implementation remains available for rollback. The ADR therefore remains
`proposed`.

## Method

The release Stage 8 binary was measured on macOS with the existing LSP
semantic-token harness. Both columns use the same binary; only the rollout mode
changes:

```text
iterations = 40
warmup = 10
legacy = KAKEHASHI_TREE_WORKER_MODE unset
worker = authoritative, KAKEHASHI_TREE_WORKER_THREADS=4
```

The edit scenarios include real `didChange`, incremental parse, injection and
diagnostic lifecycle, semantic-token calculation, delta calculation, and IPC.
The cold-open scenarios use a fresh URI for each iteration.

## Results

Lower is better. Values are medians from the final Stage 8 commit.

| Scenario | Legacy | Authoritative worker | Change |
|---|---:|---:|---:|
| Markdown, 60 injections, full | 0.270 ms | 0.303 ms | +12% |
| Markdown, 60 injections, edit + delta | 6.853 ms | 7.542 ms | +10% |
| Large Rust, edit + delta | 25.326 ms | 39.016 ms | +54% |
| Markdown, 150 injections, cold open + first token | 21.832 ms | 27.590 ms | +26% |
| Large Rust, cold open + first token | 527.047 ms | 554.410 ms | +5% |

The large Rust edit distribution remains bimodal:

| Percentile | Legacy | Authoritative worker |
|---|---:|---:|
| p25 | 18.025 ms | 0.635 ms |
| p75 | 31.258 ms | 41.548 ms |

The approximately 0.6 ms worker p25 confirms that an exact current-version
resident cache hit can outperform the in-process reader substantially. Cache
misses are frequent enough that the median and p75 still regress.

## Findings

### Stage 7 did not measure incremental worker edits

Stage 8 telemetry exposed an identity check that compared the replica's current
content version with the target version. Every advancing `didChange` therefore
selected full document synchronization instead of `ApplyDocumentEdits`.
Checking the replica against the edit's base version and advancing its identity
after a successful apply restores actual worker-side incremental parsing.

This means the Stage 7 edit numbers are valid end-to-end observations of that
implementation, but they are not evidence about worker incremental-parse
performance. After the fix and the Stage 8 ownership changes, the four-thread
large Rust median fell from 64.686 ms to 39.016 ms and Markdown edit + delta
fell from 11.746 ms to 7.542 ms. Neither result yet beats legacy.

### The internal worker queue is not the primary bottleneck

A four-iteration telemetry run measured large Rust semantic-token queue waits
of 6--12 microseconds without contention and 6.9 ms in one contended sample.
Worker semantic computation took approximately 19--25 ms. Adding worker threads
or processes would not remove this per-document miss work and would add routing
and replica state.

The worker count therefore remains fixed at one. The next performance work
should reduce cache-miss computation and transferred results rather than tune
the pool size.

### Acknowledgement-only edits do not close the gap

Authoritative edits no longer derive a full worker snapshot solely for shadow
comparison. The worker applies the edit and returns a compact document
acknowledgement; shadow mode retains snapshot derivation for comparison. The
final result still regresses against legacy, showing that redundant snapshot
derivation was not the dominant cost.

### Cold ownership removes duplication but not process-boundary cost

The authoritative parent no longer creates its own `Tree` during `didOpen`, yet
cold open remains 5% slower for large Rust and 26% slower for injection-heavy
Markdown. Removing the duplicate parent parse is structurally necessary for
single ownership, but it does not by itself establish a performance win.

## Decision

Stage 8 improves the worker path materially and validates very fast
current-version cache hits, but it does not pass the performance non-regression
gate. Keep one resident worker with an internal thread pool, keep the rollout
guarded, and do not claim that the worker architecture is faster overall.

Before acceptance, the implementation must move grammar/query loading out of
the authoritative parent, reduce semantic cache-miss computation or result
transfer, and repeat the same paired workload together with failure-injection,
protocol, compatibility, and memory gates.
