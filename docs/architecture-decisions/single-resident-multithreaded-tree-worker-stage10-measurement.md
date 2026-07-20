# Single Tree Worker Stage 10 Cancellation Measurement

## Scope

Stage 10 adds an explicit, idempotent cancellation frame for tree-dependent
reader work. Dropping an LSP semantic-token future, `$/cancelRequest`, request
supersession, and `didClose` now cancel the matching worker request. Queued work
is skipped before tree access; running semantic computation polls the shared
token and publishes no result or cache entry after cancellation.

The worker keeps reading one bounded lookahead frame when its ordinary pending
window is full, so cancellation cannot be trapped behind compute admission.
Permits are acquired when a document-lane job actually starts rather than when
it is queued. Thus a single document retains at most one active outer job and
cannot reserve every permit with its FIFO backlog.

This is still an intermediate measurement. Nested injection fan-out is not yet
split into scheduler-visible fair chunks, and reader priority classes are not
implemented.

## Method

All end-to-end measurements used release binaries on macOS, 40 measured
iterations, and 10 warmups. The ordinary workload comparison ran the same Stage
10 binary in both modes:

```text
legacy = KAKEHASHI_TREE_WORKER_MODE unset
worker = authoritative, KAKEHASHI_TREE_WORKER_THREADS=4
```

The cancellation comparison ran the Stage 9 and Stage 10 authoritative
binaries back-to-back. Each iteration used a 600-function Rust document, issued
four semantic-token requests, waited 20 ms after each so computation could
start, explicitly cancelled it, then measured latency from the final edit and
request to the final usable response. This deliberately measures running-work
reclamation rather than ingress-only request coalescing.

## Results

Lower is better. Ordinary workloads remain mixed:

| Scenario | Legacy | Stage 10 authoritative | Change |
|---|---:|---:|---:|
| Markdown, 60 injections, full | 0.403 ms | 0.570 ms | +41.4% |
| Markdown, 60 injections, edit + delta | 7.786 ms | 7.027 ms | -9.7% |
| Large Rust, edit + delta | 31.220 ms | 38.440 ms | +23.1% |
| Markdown, 150 injections, cold open + first token | 27.090 ms | 28.343 ms | +4.6% |
| Large Rust, cold open + first token | 615.623 ms | 589.512 ms | -4.2% |

The sub-millisecond Markdown full result is sensitive to process scheduling: an
isolated repeat measured 0.344 ms legacy and 0.330 ms authoritative. It should
therefore be treated as transport-floor noise, not as evidence for either a 41%
regression or a 4% win. The edit and cold measurements remain the useful gates.

Running cancellation produced a stable material improvement:

| Scenario | Stage 9 authoritative | Stage 10 authoritative | Change |
|---|---:|---:|---:|
| Xlarge Rust, four cancelled requests, then latest | 302.323 ms | 199.860 ms | -33.9% |

## Findings

### Cross-process cancellation reclaims user-visible latency

Before Stage 10, the parent could stop awaiting an obsolete worker future, but
the worker continued its semantic computation. Explicit worker cancellation
reduced latest-result latency by about one third once the benchmark guaranteed
that obsolete work had actually entered compute. A smaller workload mostly
measured ingress coalescing and showed only a low-single-digit change; it was
not a valid stress test of running cancellation.

### Control responsiveness does not require an unconditional dispatcher hop

An intermediate implementation put every request through a dispatcher thread.
It preserved cancellation delivery but raised the small Markdown full median
from the Stage 9 range near 0.24 ms to about 0.57 ms. The final design decodes a
single bounded lookahead frame at saturation and schedules normal work directly.
This retained backpressure and early-cancel tests while removing the extra hot
path hop. The isolated Markdown full repeat returned to roughly 0.33 ms.

### Cancellation may overtake request execution

Dropping the async future immediately after creating its blocking worker task
can send cancellation before that task writes the request frame. The worker
therefore retains an idempotent same-generation cancellation token for an
unknown request ID and consumes it when the matching reader request arrives.
Lifecycle requests are not cancellable and discard such a token. This closes a
real scheduling race; assuming source-level creation order implies wire order
was incorrect.

### Outer fairness improved, but the ADR fairness gate remains open

Moving permit acquisition into the active lane job prevents queued work for one
document from reserving all compute capacity. This is a useful outer
per-document cap, not the ADR's complete max-min fairness contract. Nested
semantic/injection parallelism can still borrow Rayon threads without
scheduler-visible document tags, and user-blocking priority is still absent.

## Decision

Keep the one-worker design and Stage 10 cancellation protocol. It provides a
measured 33.9% latest-result win under running cancellation and fixes a real
resource-reclamation gap without increasing worker count.

Do not accept the ADR or remove legacy mode yet. Large Rust edit remains 23.1%
slower in this run, ordinary measurements are mixed, and nested fairness,
priority, native-call handshakes, failure injection, memory, compatibility, and
legacy-removal gates remain incomplete.
