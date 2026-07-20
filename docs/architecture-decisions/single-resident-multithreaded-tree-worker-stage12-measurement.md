# Single Tree Worker Stage 12 Cross-Document Fairness Measurement

## Scope

Stage 12 protects an already-runnable user-blocking document from semantic-token
injection fan-out in another document. The worker counts distinct runnable
document URIs. A semantic request keeps nested Rayon parallelism while it is the
only runnable document, but processes injection misses sequentially when another
document is already runnable.

This is admission-time protection, not the ADR's complete dynamic max-min
scheduler. Work that becomes runnable after fan-out starts cannot yet reduce the
running document's share until a cooperative boundary is reached.

## Method

The debug-build E2E fixture uses one worker with four compute threads and opens:

```text
A = Markdown with 60 Rust injection regions
B = a small Rust document
injected per-region test delay = 20 ms
A admission delay = 100 ms
foreground operation = kakehashi/node on B
```

The admission delay deterministically registers both document requests before A
chooses its nested-parallelism policy. An E2E-only switch then forces the old
unrestricted nested behavior in the same Stage 12 binary, providing a paired
control without changing production behavior. Each mode ran in a fresh process
10 times after warm compilation.

## Result

Lower is better:

| Mode | B foreground median | B foreground mean | A completion median | A completion mean |
| --- | ---: | ---: | ---: | ---: |
| Competing document suppresses nested fan-out | 385.4 ms | 396.9 ms | 1.535 s | 1.532 s |
| Nested fan-out forced | 424.9 ms | 467.7 ms | 1.594 s | 1.641 s |
| Change | -9.3% | -15.1% | -3.7% | -6.6% |

The foreground ranges were 346.0–563.9 ms with protection and 375.0–691.4 ms
with nested fan-out forced. All protected runs completed B before A. The full
parallel E2E suite also preserved this ordering under host contention: B
completed in 6.37 seconds and A in 9.49 seconds, and all 35 tests passed.

## Findings

### Measure the user-blocking operation, not a second bulk derivation

An initial experiment used semantic-token requests for both documents. Its
five-run medians were 984 ms with protection and 944 ms with nested fan-out
forced, so it showed no benefit. That workload measured equal-priority bulk
throughput rather than the ADR's user-blocking fairness claim. Replacing B with
`kakehashi/node` exposed the intended latency effect.

### Rayon work stealing helps, but does not make admission free

Even the forced control completed B before A in these isolated runs, showing
that Rayon can steal outer work from nested fan-out. Explicitly retaining
capacity for the competing document still reduced both foreground latency and
total heavy-request completion in this fixture. The result supports one shared
pool; it does not justify a second process or a permanently partitioned pool.

### The dynamic fairness gate remains open

The implementation detects competitors only before semantic derivation starts.
It does not split fan-out into bounded scheduler-visible chunks, dynamically
reduce a running document's share when a competitor arrives late, implement
priority classes, or prove the `P == 1` cooperative degradation. The artificial
delays also make this a deterministic contention experiment, not a claim about
ordinary production latency.

## Decision

Keep admission-time nested-fan-out suppression as an incremental fairness
improvement because it preserves the single-document parallel fast path and
improves the measured user-blocking workload. Continue to bounded cooperative
chunks and priority-aware admission before claiming the ADR's max-min fairness
gate or accepting the ADR.
