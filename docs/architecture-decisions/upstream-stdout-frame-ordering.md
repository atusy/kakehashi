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
- first and last byte accepted by stdout, plus successful flush completion;
- response ID and exact header + body byte count; and
- censored frames whose final flush is not observed.

The release workload used a 5,088-line, 37,940-byte Markdown document matching
the captured editor session's scale. Each scenario ran 20 edit/recompute cycles,
repeated three times in interleaved A/A runs. The profile driver continuously
drained stdout so the client did not manufacture pipe backpressure.

| Scenario | Semantic request p90, three runs | Semantic ready → last byte p90 | Semantic ready → flush p90 |
| --- | --- | --- | --- |
| semantic only | 7.5 / 7.3 / 7.3 ms | 0.3 ms | 0.5–0.6 ms |
| valid captures delta | 7.1 / 7.3 / 7.2 ms | 0.3 ms | 0.5–0.6 ms |
| captures full fallback | 7.1 / 7.2 / 7.5 ms | 0.3 ms | 0.6 ms |
| diagnostics burst | 18.2 / 18.1 / 18.2 ms | 0.3 ms | 0.5–0.6 ms |

The full captures fallback produced a 4,408.4 KiB frame. Its own
ready-to-last-byte p90 was 22.3–23.2 ms and ready-to-flush p90 was 23.2–24.1 ms,
but semantic responses were ready before that frame started in 0 of 60 cycles.
They completed first, so a pre-write scheduler had no opportunity to improve
semantic latency. The diagnostic responses in this configuration were small;
the transport instrumentation remains available for configurations with large
downstream diagnostic payloads.

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

1. semantic responses repeatedly become ready before a competing large frame
   starts writing; and
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
`semantic-large-frame-overlap` classification. Open a new transport optimization
only when the reconsideration thresholds above are met on a representative
configuration.
