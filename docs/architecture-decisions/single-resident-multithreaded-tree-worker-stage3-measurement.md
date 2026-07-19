# Single Tree Worker Stage 3 Shadow Measurement

## Scope

Stage 3 connects the resident worker to the real LSP document lifecycle while
the existing in-process path remains authoritative. With
`KAKEHASHI_TREE_WORKER_SHADOW=true`, `didOpen`, ordered incremental
`didChange`, and `didClose` events are mirrored through a bounded actor into one
resident worker. The parent compares the worker's latest owned tree summary
with the authoritative parse but never publishes the worker result.

This measurement asks two separate questions:

1. Does the worker reproduce the authoritative incremental tree over a real LSP
   session?
2. What is the foreground cost of continuously parsing every edit twice, even
   though the worker transport itself is asynchronous?

The summary comparison covers language, root kind and byte range,
`has_error`, and named-node count. It does not yet compare injections, query
captures, semantic tokens, or node identities.

## Environment and method

The committed result is
`benches/profile/results/single_worker_stage3_shadow_2026-07-19.json`. The
release binary was built from commit
`00d99301c1751d3ca17ece6f7012de999ade5617` and has SHA-256
`90902453b256fcbab027fa84fbf87f2483aad1e077df3ae37ecdebc4590eb1ca`.
The run used an Apple M4, macOS 26.5.1, Rust 1.95.0, and four worker compute
threads.

`benches/profile/drive.py` generated a 200-line Rust document. After one warm
semantic-token request, each of 200 measured cycles sent one ranged edit and
synchronously requested `textDocument/semanticTokens/full`. A 500 ms settle
followed `didOpen`. Both variants used the same release binary and installed
grammar data. Batch A ran shadow disabled then enabled; batch B reversed that
order to expose drift and thermal/order effects.

The driver measures request-to-response latency, so the semantic-token values
include waiting for the authoritative incremental parse. The shadow actor does
not await worker IPC on the LSP ingress path. Any consistent foreground delta
therefore measures shared CPU and scheduler contention from duplicate work,
not a direct pipe round trip.

## Results

All 400 measured semantic-token requests in each variant completed
successfully. Both shadow runs matched all 201 tree versions, including the
open snapshot, with zero mismatches, superseded comparisons, or pending
comparisons at shutdown.

| Batch and order | Disabled ms/cycle | Shadow ms/cycle | Shadow wall delta |
|---|---:|---:|---:|
| A, disabled first | 68.88 | 74.34 | +7.92% |
| B, shadow first | 69.37 | 73.71 | +6.27% |

| Batch | Disabled p50 / p90 / p95 / p99 | Shadow p50 / p90 / p95 / p99 | Shadow p50 delta |
|---|---:|---:|---:|
| A | 50.8 / 53.1 / 54.3 / 57.2 ms | 56.4 / 60.3 / 62.2 / 66.7 ms | +11.02% |
| B | 50.1 / 56.6 / 59.3 / 64.8 ms | 55.4 / 59.8 / 62.3 / 65.2 ms | +10.58% |

Reversing execution order did not reverse the result. Continuous shadow parsing
cost 6.3--7.9% wall time and 10.6--11.0% at p50 in this single-document
edit-and-token workload. The p90--p99 deltas ranged from 0.6% to 16.6%.
The two disabled wall baselines differed by less than 1%, while percentile
variation still shows why the reversed order is retained rather than treating
one batch as a fixed shadow cost.

This final run includes review-driven allocation removal, exact parser/query
generation fencing, per-URI open-incarnation admission, atomic comparison
lifecycle state, and bounded actor drain before the final summary. Those
correctness checks are part of the measured hot path.
The authoritative summary traversal also runs inside the existing compute-pool
injection work-unit rather than blocking a Tokio worker or adding another pool
submission.
Earlier exploratory runs had lower overhead after allocation removal, but they
predated the final lifecycle fences and are not retained as acceptance evidence.

This does not imply that the eventual worker-authoritative design is slower by
the same amount. Shadow mode deliberately performs both the legacy parse and
the future parse. Cutover removes the legacy parse, while later fusion can move
tree-derived work behind the same ownership boundary. The result instead shows
that asynchronous shadow IPC alone does not isolate foreground performance:
the duplicate native parse competes for the same CPU while the client waits for
the authoritative result.

## Reproduction

```sh
python3 benches/profile/attest_worker_binary.py \
  --checkout . \
  --output benches/profile/results/single_worker_stage3_binary_attestation_2026-07-19.json

# Prepare DATA_DIR from one fixed nvim-treesitter checkout as in the Phase 0
# procedure, including byte-identical parsers.lua and runtime/queries files.
python3 benches/profile/collect_worker_shadow.py \
  --bin ./target/release/kakehashi \
  --binary-attestation benches/profile/results/single_worker_stage3_binary_attestation_2026-07-19.json \
  --data-dir "$DATA_DIR" \
  --nvim-treesitter-checkout "$NVIM_TREESITTER_CHECKOUT" \
  --batches 2 --worker-threads 4 \
  --output benches/profile/results/single_worker_stage3_shadow_2026-07-19.json
```

The collector alternates order, explicitly sets shadow to `false` for control,
creates a fresh empty `XDG_CONFIG_HOME` and `XDG_STATE_HOME` for every variant,
requires a complete drained comparison summary, and writes the batches,
deltas, binary attestation, runtime provenance, and harness hashes directly to
the committed JSON.

## Consequence for Stage 3

The real-process lifecycle and latest-version comparison are viable: the
bounded actor preserved ordered incremental coordinates, the worker matched
every observed version, and worker failure remains isolated from public LSP
results. The measurement also adds a performance gate for later stages.

Full-rate shadowing should remain an explicit validation mode rather than the
normal user path. Sampling or deferring shadow work could reduce validation
overhead, but either weakens coverage or adds scheduling state and is not
evidence about worker-authoritative performance. The more relevant next
measurement is cutover mode, where the parent no longer performs the duplicate
parse. That stage must compare one authoritative worker parse against one
authoritative in-process parse under identical LSP workloads.

Before cutover, supervision must also convert transport loss, worker-required
restart, and process death into bounded restart plus full resynchronization of
open documents. A failed or repeatedly crashing parser must disable only its
session path and emit an actionable error; no restart-time parser blacklist is
required.

The ADR remains `proposed`.
