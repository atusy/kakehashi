# Single Tree Worker Stage 31 Semantic Finalize Allocation Measurement

## Scope

Stage 31 removes two unconditional token-vector reallocations from semantic
finalization. If no `@none` capture exists, `apply_none_preprocessing` returns
the original token vector instead of partitioning every token into fresh
vectors. If every token already fits on one line, multiline preprocessing also
returns the original vector instead of allocating and moving the full set.

Both checks are request-local linear scans and poll cancellation every 64
tokens. Neither path changes request admission, latest-wins behavior, token
ordering, overlap resolution, or retained worker state.

## Correctness result

Allocation-focused tests retain each input vector pointer across the none-free
and single-line fast paths. Existing tests continue to cover actual `@none`
splitting, Unicode and multiline projection, cancellation during finalize,
transparent breakpoints, overlap priorities, injection exclusion, and LSP delta
encoding. The semantic suite and warning-denying all-target/all-feature Clippy
pass.

An additional line-level shortcut scanned for visible non-overlapping captures
and bypassed breakpoint/heap processing. Although a short debug series reduced
the reported finalize phase from roughly 3.4–3.6 ms to 2.7–2.8 ms, four
authoritative E2E pairs showed flat/slower Rust central latency and worse
single-edit p95 in every pair. That shortcut was removed. The accepted measured
binary contains only the two allocation-preserving paths. Four separately
hashed raw-sample files for the rejected candidate are retained alongside the
accepted evidence so this rejection remains independently auditable.

All 1,200 timed benchmark results represented the latest edit. All 960 obsolete
requests in cancellation scenarios returned JSON-RPC
`RequestCancelled (-32800)`.

## Performance result

Stage 30 and the allocation-only Stage 31 release binaries were measured end to
end over LSP with the authoritative worker and eight compute threads on an Apple
M4. Four pairs alternated binary order; each binary received six warmups and 30
timed iterations per scenario against the same content-addressed fixture.

The median paired Stage-31 delta relative to Stage 30 was:

- Rust single-edit typing: **-4.0%**. Three of four pairs improved; the range
  was -13.4% to +1.9%.
- Rust eight-edit burst: **-4.2%**. Three of four pairs improved; the range was
  -9.6% to +19.2%.
- Rust four-cancellation burst: **-9.0%**. All four pairs improved; the range
  was -13.8% to -1.6%.
- Markdown typing: **-6.2%** for single edits and **-3.5%** for bursts, both with
  sign-changing pair results.

Cancellation-burst p95 improved by 17.5% at the median and in three of four
pairs. Single-edit, eight-edit, and Markdown p95 values were unstable and do not
support a general tail claim. In particular, the eight-edit candidate regressed
materially in one pair; Stage 31 is accepted as a central/cancellation-latency
improvement, not as a universal burst-tail improvement.

The authoritative evidence is
[`single_worker_stage31_finalize_allocation_2026-07-22.json`](../../benches/profile/results/single_worker_stage31_finalize_allocation_2026-07-22.json),
with four separately hashed raw-sample files alongside it.
