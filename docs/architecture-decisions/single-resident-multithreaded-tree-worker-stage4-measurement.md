# Single Tree Worker Stage 4 Supervision Measurement

## Scope

Stage 4 keeps the Stage 3 shadow path non-authoritative and adds a supervisor
around its single resident worker. The supervisor detects request-time
transport loss, explicit `WorkerRestartRequired` responses, and process exit
while idle. It starts a new worker generation, replays the latest full text of
all open documents, and fences queued commands to the replacement generation.

An implicated grammar is conservatively quarantined for the current session.
The worker still restarts so other grammars can continue. The quarantine is not
persisted and therefore does not create a next-start blacklist.

## Method

The committed raw result is
`benches/profile/results/single_worker_stage4_recovery_2026-07-19.json`. The
release binary was built with the `e2e` feature from commit
`0d6763b36f1aa6bb32576f9b5882514678e30805` and has SHA-256
`d75ff167c973a17e3d1f842284c376eee794f0672f9f9aeac8e8cca4012aaad6`.
The run used an Apple M4, macOS 26.5.1, Rust 1.95.0, and four worker compute
threads.

Deterministic one-shot hooks injected four failure shapes. Each scenario ran
five fresh LSP server sessions. `recovery_ms` starts when the actor begins
handling the failure and ends after worker cleanup, the configured 250 ms
backoff, replacement spawn and handshake, and full-text resynchronization.
For idle exit, the separate detection delay is not included; the supervisor
polls at 250 ms, so up to another 250 ms precedes the logged recovery interval.

## Results

| Scenario | Replayed documents / bytes | Recovery median | Mean | Range |
|---|---:|---:|---:|---:|
| Explicit systemic restart | 1 / 34 B | 515 ms | 543.4 ms | 508--625 ms |
| Explicit systemic restart, larger session | 17 / 248,037 B | 522 ms | 526.2 ms | 517--543 ms |
| Request-time process crash, implicated grammar quarantined | 0 / 0 B | 504 ms | 528.0 ms | 486--600 ms |
| Idle process exit | 0 / 0 B | 493 ms | 498.4 ms | 470--550 ms |

The 17-document replay did not measure slower than the one-document replay;
their ranges overlap and the larger replay's median was only 7 ms higher. At this
scale, replaying roughly 248 KB is below run-to-run worker cleanup and spawn
jitter. The fixed 250 ms backoff plus cleanup, process creation, binary digest,
handshake, and parser loading dominates the observed roughly 0.5 second
recovery interval.

This does not prove replay cost is constant. Larger retained text, more
languages, expensive parsers, and query-derived state can make replay visible.
The supervisor therefore logs both document and byte counts beside recovery
time so later production observations can expose that scaling.

## Architectural consequences

One worker is compatible with bounded recovery, but it is also one shared
failure domain. A systemic worker loss temporarily interrupts every grammar,
even when only one document was active. Session quarantine prevents a parser
with failure-correlated evidence from repeatedly taking down healthy grammars;
the replacement process then recovers the non-quarantined documents.

The result supports the ADR's decision not to persist a parser blacklist.
Restarting Kakehashi can retry the same parser safely because it remains behind
the process boundary and will be quarantined again if it fails. Persistence
would add stale cross-session state without reducing the approximately
half-second bounded recovery measured here.

The current grammar identity is parser path, grammar symbol, and configuration
generation rather than a content-addressed artifact identity. Restart budgets
are bounded but do not yet restore tokens after a healthy period or implement
circuit half-open transitions. Those are follow-up supervision stages before
authoritative cutover; this measurement must not be read as completion of the
entire ADR supervision model.

## Reproduction

```sh
for test_name in \
  systemic_worker_restart_full_resyncs_the_open_document \
  systemic_worker_restart_measures_many_document_full_resync \
  crashed_grammar_is_quarantined_only_in_session_and_other_grammar_recovers \
  idle_worker_exit_is_detected_and_restarted_before_the_next_document
do
  for trial in 1 2 3 4 5
  do
    cargo test --release --locked --features e2e \
      --test e2e_tree_worker_shadow "$test_name" -- --nocapture
  done
done
```

The Stage 3 normal-workload result remains the steady-state measurement:
full-rate duplicate shadow parsing costs 6.3--7.9% wall time in that workload.
Stage 4 changes failure handling rather than the successful request path, so
its new acceptance evidence is recovery latency and replay volume. The later
authoritative cutover still requires a fresh steady-state comparison with the
parent's duplicate parse removed.
