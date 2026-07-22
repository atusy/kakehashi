# Single Tree Worker Stage 30 Semantic Query Metadata Measurement

## Scope

Stage 30 reduces repeated work inside dense semantic-token query walks without
changing request admission, edit delivery, latest-wins cancellation, worker
ownership, or response currency. Before a query walk starts, it may allocate
request-local indexed tables for pattern priorities and capture roles. Table
entries remain lazy, so only indices that actually match are populated.

Admission requires both:

- at least 4,096 syntax-tree descendants; and
- at least 16 syntax-tree descendants per possible query metadata slot
  (pattern count plus capture-name count).

The second condition bounds setup relative to the query's own table size,
including unusually large custom queries. Sparse trees remain on direct
resolution regardless of source byte size. The decision is made once before the
walk, so match and capture loops pay no admission counter or branch.

The tables live only for one `collect_host_tokens` invocation. They add no
cross-request state or invalidation, and they cannot delay admission of a newer
document version. Stage 29's eager content-line table is retained so the A/B
difference isolates query metadata reuse.

## Alternatives tested

Review exposed that source byte size and raw AST density alone were incomplete
cost signals. Stage 30 therefore tested, in incremental commits:

- a 32 KiB source-only gate;
- a 32 KiB plus 4,096-node gate;
- admission after 64 and 256 observed matches/captures; and
- memoizing pattern priorities without capture roles.

Observed-item admission added a branch to every hot-loop item. It improved dense
Rust but regressed Markdown burst follow latency. Priority-only memoization did
not preserve the broad improvement. Those candidates were rejected. The final
cost ratio keeps admission outside the hot loops while bounding table setup by
both tree work and query metadata size.

## Correctness result

Focused tests cover explicit and default priorities, built-in and special
capture roles, lazy population of only requested indices, both dimensions of
the amortization boundary, and direct-versus-cached collector parity. The parity
case includes suppression, unknown mappings, modifiers, and explicit/default
priorities.

Every benchmark full/delta response was reconstructed client-side and accepted
only when its tracked semantic token represented the latest edit. All 2,160
timed responses passed. All 1,152 obsolete cancellation-burst requests,
including warmups, returned JSON-RPC `RequestCancelled (-32800)`. The semantic
suite, full library suite, benchmark build, and warning-denying all-target,
all-feature Clippy passed.

## Performance result

Reviewed Stage 29 and final Stage 30 release binaries were measured end to end
over LSP with the authoritative worker and eight compute threads on an Apple M4.
Four pairs alternated binary order; each binary received six warmups and 30
timed iterations per scenario against the same content-addressed parser/query
fixture. Lower is better.

The median of four paired Stage 30 deltas relative to Stage 29 was:

- Rust single-edit typing: **-12.7%** (range -29.7% to +14.2%).
- Rust eight-edit burst: **-14.2%** (range -29.7% to +3.0%).
- Rust four-cancellation burst: **-9.3%** (range -13.7% to +0.4%).
- Sparse Rust controls: **-1.6% to +0.8%** across 32/64 KiB inputs.
- Markdown injection burst: **+0.5%** (range -1.1% to +1.5%).
- Markdown single-edit typing: **+4.0%**, with sign-changing results and one
  +32.8% outlier; no regression or improvement claim is made for this control.

The accepted claim is therefore narrower than “all semantic-token workloads
improve”: dense Rust median and follow latency improve, while sparse inputs and
Markdown burst remain neutral. Cancellation-burst p95 also changed sign across
pairs, so no tail-latency claim is made.

The Stage 29 source was `510b8157d68a5c51d7b95883fe2a42a8a148afde`.
The measured Stage 30 server source was
`88303574f14a2f80e12e36a87fc6ff00aff0bbc4`. Release binary SHA-256
identities were
`421ecba956065305f382d2663cdfff2ac22729e60134fa47646cba7758aa9e75`
for Stage 29 and
`dcd5c1a9e7c52e827840fff2c7b689f925dad92f33deb01f1e73ab232b7ac6ee`
for Stage 30. The harness binary SHA-256 was
`0aca43a36ca660f5a8ed67d014cc5d7323cafa81541e9059884804116eed21fd`.

Authoritative raw evidence:

- [pair 1](../../benches/profile/results/single_worker_stage30_amortized_metadata_final_pair1_2026-07-22.json)
  (`b5b38ce952711f13cf6186e40ccfd8cc8872dec1b763ec2bc347ef44cf699509`)
- [pair 2](../../benches/profile/results/single_worker_stage30_amortized_metadata_final_pair2_2026-07-22.json)
  (`1b555f0ec88e8efcf9ec93440f77ce129bd7275902089b99c886c6eba1bc31c3`)
- [pair 3](../../benches/profile/results/single_worker_stage30_amortized_metadata_final_pair3_2026-07-22.json)
  (`96fbe3a8ce80de0187046fb8b0c69ea86c81790fce9601e17cbfff22fedac40a`)
- [pair 4](../../benches/profile/results/single_worker_stage30_amortized_metadata_final_pair4_2026-07-22.json)
  (`cfba8fb40d9c2be93e0bded26d0094e26ecb9f4f2baa9bed98f5dfd5a57984b7`)

The machine-readable aggregate is
[`single_worker_stage30_query_metadata_2026-07-22.json`](../../benches/profile/results/single_worker_stage30_query_metadata_2026-07-22.json).

## Decision

Accept request-local query metadata reuse only when syntax-walk work
conservatively amortizes the query-wide table slots. This improves the measured
dense Rust typing paths without weakening semantic-token freshness or retaining
new semantic state. Keep sparse and high-table-cost queries on direct
resolution. Do not add hot-loop observed-item admission or edit coalescing.
