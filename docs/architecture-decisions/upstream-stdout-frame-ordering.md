# Preserve FIFO Ordering for Upstream Stdout Frames

**Related Decisions**: [ls-bridge-message-ordering](ls-bridge-message-ordering.md)

## Context

LSP uses one framed JSON-RPC byte stream on stdout. A response cannot be
interleaved with another frame, but a bounded scheduler could choose among
complete frames before any bytes of the selected frame are written. Issue #665
asked whether large captures or diagnostic responses create enough head-of-line
blocking to justify such a scheduler for semantic-token responses.

The transport observer records, on a shared monotonic clock:

- handler response-ready time and originating method;
- first write attempt, last byte accepted by stdout, and successful flush completion;
- response ID and exact header + body byte count; and
- censored frames whose final flush is not observed.

The writer observer is scoped to tower-lsp-server 0.23's tokio-util
`FramedWrite`: while a write is pending, its encoded buffer is immutable, and
only a successfully accepted prefix is advanced. The observer deliberately does
not hash the complete outstanding multi-megabyte range on every retry, because
that would introduce the very head-of-line cost being measured.

The release workload used `__ignored/example.md`, a 5,088-line, 37,940-byte
Markdown document matching the captured editor session's scale. Its SHA-256 is
`bad443ab7b8d4328e1b255f90db65ee9c83353394e1db9f84f57b371972777f6`.
Each scenario ran 20 edit/recompute cycles, repeated three times in interleaved
A/A runs. The profile driver continuously drained stdout so the client did not
manufacture pipe backpressure.

Measurement provenance:

- source: `420acaf5`;
- build: `cargo build --release --bin kakehashi`, rustc 1.95.0;
- host: arm64 macOS 26.5.1 (Darwin 25.5.0, T8132); and
- configuration: repository default discovery, `deps/test/kakehashi` data dir,
  no extra server arguments.

The exact interleaved matrix was:

```sh
for repeat in 1 2 3; do
  for scenario in semantic-only captures-delta captures-full diagnostics-burst; do
    benches/profile/drive.py \
      --bin ./target/release/kakehashi \
      --file __ignored/example.md --requests 20 --edits 1 --settle 0.3 \
      --stdout-metrics --scenario "$scenario"
  done
done
```

| Scenario | Semantic request p90, three runs | Semantic ready → last byte p90 | Semantic ready → flush p90 |
| --- | --- | --- | --- |
| semantic only | 7.7 / 7.4 / 7.4 ms | 0.3 ms | 0.4 ms |
| valid captures delta | 7.5 / 7.5 / 7.5 ms | 0.3 ms | 0.4 ms |
| captures full fallback | 7.6 / 7.4 / 7.7 ms | 0.3 ms | 0.4 ms |
| diagnostics burst | 9.3 / 9.4 / 9.3 ms | 0.3 ms | 0.4 ms |

The full captures fallback produced a 4,408.4 KiB frame. Its own
ready-to-last-byte and ready-to-flush p90 were both 11.8–11.9 ms,
but semantic responses were ready before that frame started in 0 of 60 cycles.
They completed first, so a pre-write scheduler had no opportunity to improve
semantic latency. The diagnostic responses in this configuration were valid
full reports but only about 0.1 KiB. They exercise burst ordering and metric
attribution, not the large diagnostic payload from the captured editor log. The
scheduling defer is therefore supported by the representative captures-full
path; it is not a claim that every large-diagnostics configuration has the same
opportunity rate.

To check observer effect, the captures-full matrix was also run three times
without `--stdout-metrics`. Instrumented captures p90 was 93.9 / 93.2 / 96.4 ms
versus 90.1 / 94.6 / 93.7 ms uninstrumented; semantic p90 was
7.6 / 7.4 / 7.7 ms versus 7.2 / 7.6 / 8.5 ms. The differences stayed within
run-to-run variation after replacing per-byte tail maintenance with bounded
slice copies.

## Decision Drivers

- JSON-RPC frames must never interleave.
- The common path should not pay instrumentation or scheduling overhead.
- Scheduling complexity requires measured opportunity, not only a large payload.
- An optimization must exceed A/A variation and materially improve semantic p90.
- Measurements must preserve response contents and continuously drain stdout.

## Decision

Keep the existing single physical FIFO stdout writer and do not add a frame
scheduler now. Retain opt-in instrumentation and the repeatable four-scenario
driver so future reports can be evaluated with the same metrics.

Reconsider scheduling only if a representative trace shows both:

1. semantic responses repeatedly become ready before a competing large response
   frame starts writing; and
2. ready-to-last-byte semantic p90 has at least 1 ms of avoidable delay beyond
   A/A variation.

Any future scheduler must remain bounded, select only complete frames before
their first byte is written, and keep one non-interleaving physical writer. It
must not hold service futures or consume ingress concurrency slots while queued.

## Considered Options

### Keep FIFO and retain opt-in measurement

Chosen. The measured semantic transport p90 is 0.3 ms and no schedulable
large-frame overlap occurred, giving a very low current improvement ceiling.

### Add a bounded pre-write priority scheduler

Deferred. It could help only when semantic work is ready before another frame's
first byte. The measured workload produced no such cases, while the scheduler
would add queueing state, starvation policy, cancellation behavior, and a local
replacement or upstream change for tower-lsp-server's private transport loop.

### Interrupt or interleave a large frame already being written

Rejected. Byte-level interleaving corrupts the JSON-RPC stream, and pausing a
partially written frame does not create a valid boundary for another response.

### Reduce or paginate large payloads

Not selected as a transport change. A valid captures delta already reduces the
repeat payload from 4,408.4 KiB to about 0.3 KiB. Full fallback remains a protocol
compatibility path; pagination would change the protocol result and clients.

## Consequences

### Positive

- Protocol ordering and the one-writer invariant remain simple and explicit.
- Normal sessions pay no observer or scheduler overhead.
- Future regressions can be separated into handler compute, queued transport,
  active write, flush, payload-size, and scheduling-opportunity components.

### Negative

- A semantic response that becomes ready just before a future large write will
  still follow FIFO order until measurements justify scheduling.
- Opt-in runs retain frame metrics in memory until server shutdown.

### Neutral

- A frame already in progress remains uninterruptible under every valid option.
- This decision does not change semantic-token, captures, or diagnostic payloads.

## Confirmation

Run the release driver three or more times for each scenario documented in
`benches/profile/README.md`. Confirm exact response shapes, zero censored samples,
per-method payload sizes, semantic ready-to-write/flush percentiles, and the
`semantic-large-response-overlap` classification. Open a new transport optimization
only when the reconsideration thresholds above are met on a representative
configuration.
Do not generalize this decision to large diagnostic frames until a downstream
fixture or real configuration produces a representative diagnostic payload.
