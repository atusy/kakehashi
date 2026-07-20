# Single Tree Worker Stage 19 Systemic-Breaker Measurement

## Scope

Stage 19 proves that repeated systemic worker failures converge to one degraded
tree tier instead of creating an unbounded restart loop. It also proves both
outcomes of the configuration-fenced half-open state: one failed probe returns
immediately to the open state, while a healthy replacement full-resyncs every
open document and closes the breaker only after its probation interval.

The production policy remains three fast systemic generation failures (the
initial failed generation plus at most two replacement restarts), eight fast
native-evidenced restarts, sixteen long-horizon restart tokens, a 60-second fast
healthy interval, a 10-minute long interval, and exponential backoff from 250
milliseconds to five minutes. Stage 19 groups those immutable values into a
supervisor policy. Only E2E builds can shorten the fast interval, and each test
sets that override on its isolated LSP child process.

This stage deliberately injects `WorkerRestartRequired`, which is unambiguously
systemic. It does not treat the existing generic request timeout or `exit(86)`
fixture as proof of an ADR-native failure. Native attribution requires the
future parent-acknowledged grammar-hazard and `NativeSegmentEntered` control
protocol.

## Method

### Failure convergence

Three process E2E scenarios used the real LSP parent and resident worker:

1. repeated systemic failure exhausted the three-failure fast budget, emitted
   the terminal unavailable state once, ignored later document edits as respawn
   pressure, avoided spawning a fourth generation, and kept parser-independent
   `workspace/executeCommand` service and LSP shutdown available;
2. after the open state and cooldown, one configuration-generation change
   admitted one half-open worker, whose injected resync failure immediately
   reopened the breaker; and
3. a file-gated failure was removed after opening the breaker, allowing one
   replacement to full-resync the open document, enter probation, close the
   breaker, and serve semantic tokens again.

Markers expose only completed state transitions, so the tests do not infer
breaker state from fixed sleeps. Each scenario was run three times. The elapsed
measure covers the complete debug E2E test, including LSP initialization,
parser work, transitions, assertions, and shutdown; it is not a fine-grained
recovery-latency benchmark.

### Default release A/B

The Stage 18 release binary and the Stage 19 default-feature release binary ran
in authoritative mode with four worker compute threads. The source commit was
`f03392f45eb74c0530cb6298c99657e144cc9115`. Baseline SHA-256 was
`ce06bbfd7bce95d7b0867142008e5a381a16d3ef26b6d0064c230d288b75dfc4`;
candidate SHA-256 was
`72dda444029ebdb9122e71401e36b960dca67b3165ebf5c3649480d5c2e35468`.

Twelve order-reversed cache-hit runs used one 5,000-injection Markdown document
and twenty exact-version semantic requests after warmup. Twelve independent
cold-start runs per binary used fresh config and state directories, one small
Rust document, no settle delay, and one semantic request. Raw values are in
`benches/profile/results/single_worker_stage19_breaker_2026-07-20.json`.

## Result

Every failure scenario passed in every repetition:

| Scenario | Passed | Median full-test elapsed | Range |
| --- | ---: | ---: | ---: |
| Systemic exhaustion and host-tier survival | 3/3 | 21.564 s | 21.321–21.703 s |
| Failed one-shot half-open | 3/3 | 23.249 s | 23.053–23.293 s |
| Successful full-resync and breaker close | 3/3 | 23.806 s | 23.593–23.839 s |

Default release cache-hit medians were:

| Metric | Stage 18 median | Stage 19 median | Median paired delta |
| --- | ---: | ---: | ---: |
| p50 | 72.45 ms | 72.90 ms | +0.3% |
| p95 | 76.65 ms | 77.30 ms | +1.0% |
| Wall time per cycle | 113.01 ms | 114.59 ms | +1.4% |

The paired wall-time delta ranged from -4.12 to +8.61 ms and favored Stage 19
in 4 of 12 runs. The median difference is much smaller than the sign-changing
run-to-run variation.

Fresh-process medians were:

| Metric | Stage 18 median | Stage 19 median | Median paired delta |
| --- | ---: | ---: | ---: |
| Driver elapsed | 2,242.24 ms | 2,276.04 ms | +0.8% |
| First request | 1,909.15 ms | 1,966.90 ms | +0.7% |
| Wall time per cycle | 1,909.62 ms | 1,967.40 ms | +0.7% |

The paired first-request delta ranged from -127.5 to +233.2 ms and favored
Stage 19 in 4 of 12 runs. The independently computed medians differ more than
the median of the paired deltas because the run-to-run ordering crosses; this
does not isolate a Stage 19 cold-start effect. Reading one optional E2E override
while constructing the supervisor policy shows no separable cold-start cost,
and the policy does not run on the exact-version request path.

Validation passed:

* `cargo fmt --all -- --check`;
* `cargo test --lib` (3,147 passed, 2 ignored);
* the 32 tree-worker supervisor unit tests;
* `cargo check --lib --tests --features e2e`;
* `cargo test --features e2e --test tree_worker_process` (9 passed);
* all three new breaker E2Es in every focused and measurement run; and
* `cargo clippy --all-targets --all-features -- -D warnings`.

The complete shadow E2E binary also passed 37 tests, but the existing
`late_competing_document_reduces_fanout_at_a_chunk_boundary` test timed out in
this checkout. The same test failed identically on the unmodified Stage 18
baseline, so it is recorded as a pre-existing local failure rather than hidden
or attributed to Stage 19.

## Findings

### Systemic failure now has a bounded process-level proof

Unit tests already described the restart counters, but they could not prove the
supervisor stops spawning real processes or that the LSP remains responsive.
The new convergence fixture crosses the LSP/worker boundary through three failed
generations, observes one terminal transition, and proves later edits cannot
turn the open breaker into a respawn loop.

### Half-open authority is configuration-fenced

Ordinary edits after breaker exhaustion are inert. A newer configuration
generation plus the completed cooldown admits one candidate. A candidate that
cannot full-resync returns directly to the open state; it does not recursively
consume the normal recovery loop. A successful candidate stays in probation
until the healthy interval elapses, then restores tree-backed service.

### Existing crash and hang fixtures do not prove native attribution

The current crash fixture exits normally with code 86, while the timeout starts
at generic request admission. The ADR classifies a native failure only from OS
crash evidence or an expired segment after the parent has received
`NativeSegmentEntered`. Therefore the older fixtures demonstrate containment
and recovery, but not the final grammar-attribution contract. Extending their
current heuristic would risk quarantining the command whose response happened
to fail rather than every parent-acknowledged active grammar.

### Immutable policy keeps the ordinary path unchanged

Restart durations are fixed once when the supervisor actor starts. Healthy
polls and failure recovery read plain fields; ordinary worker requests add no
state, allocation, environment lookup, or branch. The noise-scale A/B results
are consistent with that structure.

## Decision

Keep Stage 19. It closes the systemic convergence and configuration-fenced
half-open portions of the ADR without broadening the unsupported native-failure
claim. Keep the ADR `proposed`. Descendant-safe process ownership, the grammar
hazard/quarantine control plane, native-segment deadlines, protocol corruption,
compatibility gates, and legacy-path removal remain required.
