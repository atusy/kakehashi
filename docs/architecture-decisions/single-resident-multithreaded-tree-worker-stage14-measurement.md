# Single Tree Worker Stage 14 Priority Measurement

## Scope

Stage 14 distinguishes foreground worker requests from bulk derivations.
Semantic-token injection fan-out now chooses one of three policies at every
compute-width chunk boundary:

* no competing document: continue parallel fan-out;
* a background-only document competes: process the chunk sequentially; or
* a foreground document competes: yield once to the Rayon pool, then process
  the chunk according to the newly observed policy.

Document writes, node accessors, captures, selection ranges, native bindings,
configuration, and close are foreground. Snapshot, injection-region, and
semantic-token derivations are background. Runnable priority is registered by
the worker reader before pool scheduling, so saturated nested work cannot hide
a newly accepted foreground request.

## Method

### Priority contention

The deterministic E2E fixture uses one worker process, a Markdown document with
eight Rust injection regions, and a foreground node request for another
document. Each injection has an E2E-only 500 ms delay. A barrier holds the
foreground job after runnable registration and before pool scheduling until the
background request observes the priority transition. This avoids depending on
OS or Rayon scheduling timing.

The test ran five times with four compute threads and five times with one
compute thread. The same Stage 14 binary was also run five times with dynamic
nested policy disabled. These absolute durations include artificial delays and
test barriers; they compare scheduler policies rather than production latency.

### No-competitor regression

The Stage 13 and Stage 14 debug server binaries were driven by the same release
benchmark harness in authoritative-worker mode. Each ordering used 100 measured
requests after 20 warmups with no artificial delay. The binary order was then
reversed to expose order-dependent drift. The reported value is the mean of
the two medians for each stage.

## Result

Priority-contention medians were:

| Compute threads | Policy | Foreground response | Background completion |
| ---: | --- | ---: | ---: |
| 4 | Stage 14 priority | 1.343 s | 3.602 s |
| 4 | dynamic policy disabled | 1.839 s | 2.305 s |
| 1 | Stage 14 priority | 4.872 s | 5.332 s |
| 1 | dynamic policy disabled | 4.873 s | 5.125 s |

With four threads, explicit priority reduced foreground latency by 27.0% while
increasing background completion time by 56.3%. This is the intended tradeoff:
capacity is reserved for an interactive request instead of minimizing the
bulk request's makespan.

With one compute thread, foreground latency differed by less than 0.1% and the
priority policy made background completion 4.0% slower. A unit test proves
that `rayon::yield_now` can run an externally queued foreground job between
background chunks in a one-thread pool, but the process-level fixture shows no
performance benefit over Rayon's existing cooperative scheduling for this
workload.

No-competitor medians were:

| Scenario | Stage 13 | Stage 14 | Change |
| --- | ---: | ---: | ---: |
| Markdown, 60 injections, full | 1.223 ms | 1.224 ms | +0.1% |
| Markdown, 150 injections, full | 2.798 ms | 2.711 ms | -3.1% |

The small-workload direction reversed with binary order. The order-balanced
result is effectively unchanged, while the larger workload improved modestly.
No ordinary single-document regression was measured.

## Findings

### One worker process is not one compute thread

The ADR's fixed worker count still allows the resident worker to use a bounded
internal Rayon pool. The measured foreground benefit appears with four compute
threads, so it is compatible with the one-worker architecture. Reducing the
internal pool itself to one thread removes the measured benefit.

### Priority improves tail choice, not total work

The scheduler does not make the contended workload cheaper. It deliberately
moves completion time from the foreground request to the background request.
Throughput and user-blocking latency therefore need separate gates; a single
aggregate makespan would conceal the useful effect.

### A bounded yield must not spin

The first implementation repeatedly called `rayon::yield_now` while a
foreground guard remained registered. An idle pool can report a successful
idle yield, which would turn that loop into CPU spin. Stage 14 yields at most
once per chunk boundary and then falls back to sequential work.

### Full scheduler fairness remains open

This stage prioritizes a foreground document over semantic injection fan-out.
It does not provide weighted admission across more than two documents, split
other long tree walks, account memory by request priority, or define aging for
background work. The one-thread result also does not justify promising a
latency improvement when deployments explicitly reduce the compute pool to
one.

## Decision

Keep explicit foreground/background classification and bounded foreground
yielding. It materially improves the measured interactive response with the
intended multi-threaded worker and does not regress the no-competitor path.
Treat one-compute-thread progress as a correctness fallback, not a performance
claim. Continue with memory-pressure admission and compatibility gates before
accepting the ADR.
