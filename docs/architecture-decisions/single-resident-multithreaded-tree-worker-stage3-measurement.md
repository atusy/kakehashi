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
`946de588c3928136138afb112200d6cf34af0da3` and has SHA-256
`8584e64bfb38d8548221d5f5fc3c4d3a086530697a153b4a8b193fb51a885ac1`.
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
| A, disabled first | 71.04 | 77.01 | +8.40% |
| B, shadow first | 67.80 | 76.66 | +13.08% |

| Batch | Disabled p50 / p90 / p95 / p99 | Shadow p50 / p90 / p95 / p99 | Shadow p50 delta |
|---|---:|---:|---:|
| A | 52.0 / 56.6 / 57.6 / 61.2 ms | 59.1 / 64.5 / 67.0 / 68.6 ms | +13.65% |
| B | 50.3 / 53.0 / 54.2 / 57.0 ms | 57.7 / 65.4 / 67.5 / 73.6 ms | +14.71% |

Reversing execution order did not reverse the result. Continuous shadow parsing
cost 8.4--13.1% wall time and 13.7--14.7% at p50 in this single-document
edit-and-token workload. The p90--p99 deltas ranged from 12.1% to 29.1%.
Batch B's disabled baseline was faster than batch A's, so the range includes
temporal/system variation rather than isolating one fixed shadow cost.

This final run includes review-driven allocation removal, exact parser/query
generation fencing, per-URI open-incarnation admission, and atomic comparison
lifecycle state. Those correctness checks are part of the measured hot path.
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
cargo build --release --locked --bin kakehashi
shasum -a 256 target/release/kakehashi

KAKEHASHI_TREE_WORKER_SHADOW=true \
KAKEHASHI_TREE_WORKER_THREADS=4 \
RUST_LOG=kakehashi::tree_worker_shadow=info,kakehashi::tree_worker_shadow_metrics=info \
python3 benches/profile/drive.py \
  --bin ./target/release/kakehashi \
  --lang rust --size 200 --requests 200 --edits 1 \
  --warm-semantic-cache --settle 0.5 \
  --data-dir deps/test/kakehashi

# Repeat without KAKEHASHI_TREE_WORKER_SHADOW, then repeat the pair in the
# opposite order. Use isolated but identical XDG_CONFIG_HOME and XDG_STATE_HOME
# directories for every run.
```

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
