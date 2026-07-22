# Single Tree Worker Stage 30 Semantic Query Metadata Measurement

## Scope

Stage 30 reduces repeated work inside dense semantic-token query walks without
changing request admission, edit delivery, latest-wins cancellation, worker
ownership, or response currency. For source segments of at least 32 KiB whose
syntax tree contains at least 4,096 descendants, pattern priorities and capture
roles are resolved lazily into request-local indexed tables. Only indices that
actually match are populated. Smaller or syntactically sparse hosts and
injection regions retain direct resolution, avoiding table setup when few
captures can amortize it.

The tables live only for one `collect_host_tokens` invocation. They do not
retain cross-request state, require invalidation, or delay a newly admitted
document version. The final measured candidate deliberately retains Stage 29's
eager content-line table so the A/B difference isolates query metadata reuse;
an earlier combined measurement could not establish which change caused its
result.

## Correctness result

Focused tests cover explicit and default pattern priorities, built-in and
special capture roles, lazy population of only requested pattern/capture
indices, both dimensions of the 32 KiB / 4,096-node admission boundary, and an
exactly 32 KiB sparse Rust tree that must remain on direct resolution. A
collector-level parity test compares direct and cached paths over the same query
and mappings, including suppression, unknown mappings, modifiers, and
explicit/default priorities. The semantic suite and warning-denying
all-target/all-feature Clippy passed.

The benchmark reconstructed every full/delta response client-side and accepted
a sample only when the tracked token represented the latest edit. All 2,160
timed responses passed. All 1,152 obsolete cancellation-burst requests,
including requests issued during warmup iterations, returned JSON-RPC
`RequestCancelled (-32800)`.

## Review-driven gate correction

Review found that the initial 32 KiB source-size gate had only been measured on
dense Rust. Two reversed exploratory pairs added exact-size sparse controls at
32 KiB minus 128 bytes, 32 KiB, 32 KiB plus 128 bytes, and 64 KiB. With the
source-only gate, the just-above control regressed in both orders (+3.7% and
+6.0%), and the 64 KiB control regressed +19.6% in one order. Source size alone
was therefore rejected as an admission signal.

The final gate additionally requires 4,096 syntax descendants, matching the
established large-semantic-host density boundary. All four sparse controls then
stay on direct resolution. Their final paired medians ranged from -4.7% to
-1.2%, with order-sensitive ranges; they establish no sparse-input regression,
not a performance gain attributable to metadata reuse. The exploratory raw
evidence is retained in:

- [`single_worker_stage30_sparse_gate_pair1_2026-07-22.json`](../../benches/profile/results/single_worker_stage30_sparse_gate_pair1_2026-07-22.json)
  (`7e706554006cbb948bc2f95d0a4be0fe64b3bf750ce1e1da3c1aba91d9b7b4ec`)
- [`single_worker_stage30_sparse_gate_pair2_2026-07-22.json`](../../benches/profile/results/single_worker_stage30_sparse_gate_pair2_2026-07-22.json)
  (`ae54d6ee6d9526450c1c3e442ec091ffba2bfa53148cb56f5e842d9c93755740`)

## Performance result

Reviewed Stage 29 and metadata-only Stage 30 release binaries were measured end
to end over LSP with the authoritative worker and eight compute threads on an
Apple M4. Four pairs alternated binary order; each binary received six warmups
and 30 timed iterations per scenario against the same content-addressed
parser/query fixture.

The median paired Stage 30 delta relative to Stage 29 was:

- Rust single-edit typing: **-12.4%**. All four pairs improved; the range was
  -17.7% to -5.7%.
- Rust eight-edit burst: **-0.8%**. Results were order-sensitive, ranging from
  -7.0% to +13.3%; no burst-median claim is made.
- Rust four-cancellation burst: **-9.8%**. All four pairs improved; the range
  was -16.6% to -6.9%.
- Cancellation-burst p95: **-6.8%** paired median, with a range of -21.4% to
  +1.3%.

Markdown controls do not meet the dense metadata gate. Their paired medians
were -12.6% for single edits and -7.4% for bursts, but each changed sign across
pairs. These results are treated only as evidence that the isolated candidate
does not introduce a Markdown regression; they are not attributed to metadata
reuse.

The measured server source was commit `73a766ee0` (the following commit adds
only a gate test), on top of reviewed Stage 29 source `510b8157d`. Release
binary SHA-256 identities were
`421ecba956065305f382d2663cdfff2ac22729e60134fa47646cba7758aa9e75`
for Stage 29 and
`f06f3989ad3c138248a190a79ec0da2fd0aaf5e9f6afde1db5dfac016066e435`
for Stage 30. The harness binary SHA-256 was
`42f234d184d513d325d3961154bd8d0745c1ad53191845abff8251d7e80da84a`.
Authoritative raw evidence is retained in:

- [`single_worker_stage30_metadata_only_final_pair1_2026-07-22.json`](../../benches/profile/results/single_worker_stage30_metadata_only_final_pair1_2026-07-22.json)
  (`a4f4df0ed9f71c19a4d6117aa8da0abc1ba8397d7fb1161d7b6adb4cc7465a14`)
- [`single_worker_stage30_metadata_only_final_pair2_2026-07-22.json`](../../benches/profile/results/single_worker_stage30_metadata_only_final_pair2_2026-07-22.json)
  (`82da006d38c577020bab1282156f4cbe812444abe3db067466cb3f0279a7bd45`)
- [`single_worker_stage30_metadata_only_final_pair3_2026-07-22.json`](../../benches/profile/results/single_worker_stage30_metadata_only_final_pair3_2026-07-22.json)
  (`8f5690374b591268fc836f681b1889bd76da869ac35174cab618f21a20810627`)
- [`single_worker_stage30_metadata_only_final_pair4_2026-07-22.json`](../../benches/profile/results/single_worker_stage30_metadata_only_final_pair4_2026-07-22.json)
  (`1d4b858d8d9158bec23dec50fd803f56fa361bf8527766130e504a9b615aabb2`)

## Decision

Accept request-local query metadata reuse only for large, syntactically dense
sources. It improves dense Rust single-edit and cancellation follow latency in
all four pairs without weakening freshness, and it adds no retained semantic
state. Keep sparse sources on direct resolution and keep lazy line indexing out
of this stage so the accepted performance claim remains isolated and auditable.
