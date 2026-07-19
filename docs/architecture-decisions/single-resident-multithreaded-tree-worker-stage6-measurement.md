# Stage 6 node-operation measurement

Date: 2026-07-20

This measurement evaluates the first worker-owned reader operation from
`single-resident-multithreaded-tree-worker`: host-tree node resolution and
parent navigation. It is a microbenchmark of the internal worker protocol, not
an end-to-end LSP latency claim.

## Attestation

- source commit: `4b0884ef941a3b29e8c9692b3c2b0a89e7a4e131`
- release binary SHA-256:
  `02c0d7717dcac03e7f168a2b11758da33169a6bec918fb778aa2d56be7ebdb61`
- Rust parser SHA-256:
  `74a21f5ff92f409170d6fc605b44773911842c54757744af71a95fd4aeccd28a`
- four batches of 5,000 requests against an 8,270-byte, 200-line Rust source
- one resident worker with four compute threads

The raw batch summary is committed at
`benches/profile/results/single_worker_stage6_node_2026-07-20.json`.

## Results

Median of the four batch p50 values:

| operation | p50 |
| --- | ---: |
| direct local resolve + parent | 0.854 us |
| worker resolve, one round trip | 53.917 us |
| worker resolve + parent, two round trips | 108.834 us |
| worker resolve, four concurrent documents | 81.938 us |

The four-document run sustained a median 46,932 requests/second. This proves
the concurrent read router reaches the worker without the supervisor actor
serializing all reads. It does not make the fine-grained RPC shape acceptable:
one local traversal is about 63 times cheaper than one worker round trip at the
median, and the two-round-trip operation is almost exactly twice the one-round-
trip latency.

## Decision impact

Do not translate the public node accessor protocol into one internal RPC per
Tree-sitter primitive. The process boundary must fuse the complete public
request: resolve the opaque ID, perform its navigation or scalar operation,
mint any returned IDs, and serialize the final public result in one worker
operation. Bulk endpoints such as children must return the complete owned list
in one response.

The `ResolveNode` and `NavigateNode` prototype remains useful for identity and
generation-fence validation, but the authoritative cutover must route each
public handler through a fused high-level operation. The same rule applies to
captures, selection ranges, symbols, and tokens: worker-side tree locality can
offset repeated tree walks only when the boundary avoids N+1 IPC.
