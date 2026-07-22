# Single Tree Worker Stage 31 Semantic Finalize Allocation Measurement (Rejected)

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
single-edit p95 in every pair. That shortcut was removed. The final reviewed
binary contains only the two allocation-preserving paths. Four separately
hashed raw-sample files for the rejected candidate are retained alongside the
allocation-candidate evidence so this rejection remains independently auditable.

All 1,200 timed benchmark results represented the latest edit. All 960 obsolete
requests in cancellation scenarios returned JSON-RPC
`RequestCancelled (-32800)`.

## Performance result

After local review added bounded cancellation polling to the `@none` admission
scan, Stage 30 and the reviewed Stage 31 release binaries were remeasured end to
end over LSP with the authoritative worker and eight compute threads on an Apple
M4. Four pairs alternated binary order; each binary received six warmups and 30
timed iterations per scenario against the same content-addressed fixture. This
final series supersedes the stronger pre-review numbers retained in branch
history.

The median paired Stage-31 delta relative to Stage 30 was:

- Rust single-edit typing: **+0.8%**. Two of four pairs improved; the range was
  -5.1% to +35.0%.
- Rust eight-edit burst: **+1.6%**. Two of four pairs improved; the range was
  -11.4% to +20.2%.
- Rust four-cancellation burst: **-9.0%**. All four pairs improved; the range
  was -18.1% to -3.7%.
- Markdown typing: **-1.0%** for single edits and **-0.9%** for bursts, both with
  sign-changing or near-flat pair results.

Cancellation-burst p95 improved by 8.9% at the median and in three of four
pairs. However, Rust single-edit and eight-edit p95 were worse overall, and
Markdown single-edit p95 regressed in all four pairs. The consistent
cancellation-follow gain does not compensate for the lack of normal Rust typing
improvement and the Markdown tail regression. Stage 31 is therefore rejected;
the allocation fast paths should not be merged in this form.

The authoritative evidence is
[`single_worker_stage31_finalize_allocation_2026-07-22.json`](../../benches/profile/results/single_worker_stage31_finalize_allocation_2026-07-22.json),
with four separately hashed raw-sample files alongside it.
