# Single Tree Worker Stage 13 Dynamic Fairness Measurement

## Scope

Stage 13 turns Stage 12's admission-time semantic policy into a dynamically
rechecked policy. Injection cache misses are divided into chunks no larger than
the worker compute-thread count. Before every chunk, the semantic request asks
whether another document is runnable:

* no competitor: process the next chunk in parallel and borrow idle capacity;
* competitor present: process the next chunk sequentially, leaving the other
  worker threads available to outer document jobs; and
* competitor gone: resume parallel chunks without restarting the request.

The legacy parent semantic path passes no gate and retains its previous nested
parallel behavior.

## Method

### Late-arrival behavior

The E2E test uses one worker with four compute threads and a Markdown document
with eight Rust injection regions, which creates exactly two four-region
chunks. E2E-only barriers establish this causal order:

1. A begins its first chunk while no other document is runnable.
2. The first chunk pauses after proving idle capacity was observed.
3. B is opened and its worker request is registered in the runnable-document
   set, even though A's nested work currently occupies the pool.
4. A's first chunk is released.
5. Before the second chunk, A observes B and records a yield.

The B worker job remains behind a test-only barrier until that yield is
recorded. This makes the assertion independent of process startup, CPU
contention, and Rayon scheduling speed.

### No-competitor regression

The Stage 12 and Stage 13 debug server binaries were driven by the same release
benchmark harness in authoritative-worker mode. Each scenario used 40 measured
requests after 10 warmups, with no artificial delay.

## Result

The late-arrival E2E test observed the required allowed-to-denied transition.
The focused run completed B's node request in 919 ms and A in 2.405 seconds.
Under the full parallel E2E suite it still observed the yield, with B at 912 ms
and A at 11.332 seconds; all 36 tests passed.

Single-document medians were:

| Scenario | Stage 12 | Stage 13 | Change |
| --- | ---: | ---: | ---: |
| Markdown, 60 injections, full | 2.545 ms | 2.537 ms | -0.3% |
| Markdown, 150 injections, full | 5.650 ms | 5.640 ms | -0.2% |

The chunk-boundary overhead was below the noise floor in both measured
no-competitor workloads.

## Findings

### Runnable registration must precede compute admission

The deterministic test exposed a useful scheduler property: nested work can
temporarily occupy every Rayon thread, so a competing outer job may not begin.
Its document must therefore enter the runnable set when the worker reader
accepts it, before pool admission. Waiting for the competing job body to start
would make the fairness signal itself vulnerable to starvation.

### Rayon work stealing handles short competitors well

An early late-arrival fixture used a small node request that completed in about
42 ms, before A reached its next 50 ms chunk boundary. No yield was necessary.
The final fixture holds B runnable until the boundary to test the policy rather
than a timing accident. Dynamic chunking adds an upper bound; it does not claim
that every short competitor needs explicit intervention.

### This is bounded yielding, not full max-min scheduling

Stage 13 rechecks after at most one compute-width injection chunk and restores
idle borrowing dynamically. It still gives a competing set the aggregate
remaining threads rather than round-robin chunk admission per document. It
does not define user-blocking versus speculative priority classes, split other
long tree walks, or prove useful cooperative progress with a one-thread worker.

## Decision

Keep bounded dynamic injection chunks. They close the late-arrival hole without
a measurable single-document regression. Continue with explicit priority and
multi-document scheduler admission before claiming the ADR's full max-min
fairness gate.
