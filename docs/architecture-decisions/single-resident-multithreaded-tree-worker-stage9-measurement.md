# Single Tree Worker Stage 9 Native-Loading Measurement

## Scope

Stage 9 moves dynamic grammar loading and tolerant query compilation out of the
authoritative parent. The parent resolves immutable parser artifacts and raw
query text as data; the worker alone loads `tree_sitter::Language`, compiles
queries, owns parser registries, and serves tree-dependent readers.

Search-path parser filenames populate the worker catalog without `dlopen` in
the parent. This preserves injected-language discovery when the parent registry
is intentionally empty.

## Method

The same release Stage 9 binary was run in both modes on macOS:

```text
iterations = 40
warmup = 10
legacy = KAKEHASHI_TREE_WORKER_MODE unset
worker = authoritative, KAKEHASHI_TREE_WORKER_THREADS=4
```

The semantic-token E2E contract was strengthened to require non-empty full and
range results in authoritative mode. This matters because an intermediate
build returned empty tokens in approximately 0.7 ms after mistaking the absent
parent query for an unsupported language. Those invalid measurements are not
included below.

## Results

Lower is better. These are paired medians from the final correctness-preserving
Stage 9 binary.

| Scenario | Legacy | Authoritative worker | Change |
|---|---:|---:|---:|
| Markdown, 60 injections, full | 0.266 ms | 0.237 ms | -11% |
| Markdown, 60 injections, edit + delta | 7.604 ms | 7.150 ms | -6% |
| Large Rust, edit + delta | 30.779 ms | 39.265 ms | +28% |
| Markdown, 150 injections, cold open + first token | 22.675 ms | 33.679 ms | +49% |
| Large Rust, cold open + first token | 548.209 ms | 597.798 ms | +9% |

The large Rust authoritative edit distribution remains bimodal: p25 was
1.302 ms and p75 was 41.547 ms. Resident exact-version reuse is faster than
legacy, but cache misses still determine the median.

## Findings

### One fixed worker can improve real workloads

The authoritative worker beats the paired legacy mode for both measured
injection-heavy steady-state workloads. This is the first end-to-end evidence
in the stack that one fixed worker with internal threads can improve user-facing
latency rather than merely isolate crashes or win a synthetic cache-hit case.

The result does not justify more workers. Stage 8 telemetry showed that queue
wait was normally measured in microseconds while per-document semantic miss
computation took tens of milliseconds.

### Ownership and availability are different states

Removing parent parser registration initially broke host-language detection and
injected parser discovery. A language remains available when the worker has a
resolvable artifact even though no parent `Language` exists. Separating that
capability check from parent-loaded state lets every authoritative reader keep
the existing detection contract without reintroducing native ownership.

### Raw and compiled query identities must both be cached

The worker receives raw query text so tolerant compilation can happen inside
the crash boundary. Invalid patterns make the compiled source differ from the
raw input. Comparing the next raw request only with compiled source caused a
tolerant recompile on every semantic request: large Rust edit rose to
101.229 ms and Markdown edit to 17.398 ms.

The worker now records raw input identity separately from the filtered compiled
source. Reusing that compilation restored large Rust edit to 39.265 ms and
Markdown edit to 7.150 ms.

### Cold and serial miss paths remain the acceptance blocker

Worker-authoritative cold open is still slower, especially for
injection-heavy Markdown. Large Rust edit misses also remain slower than
legacy despite very fast cache-hit samples. The overall performance
non-regression gate therefore remains open.

## Decision

Keep one worker and its internal thread pool. Stage 9 completes the guarded
native loading ownership boundary and demonstrates measured steady-state wins,
but it does not establish a universal performance win or change the ADR from
`proposed`.

Next work should target lazy catalog/parser/query preload, large semantic miss
computation and transfer, then repeat cold, miss, memory, failure-injection,
protocol, and compatibility gates before removing the legacy implementation.
