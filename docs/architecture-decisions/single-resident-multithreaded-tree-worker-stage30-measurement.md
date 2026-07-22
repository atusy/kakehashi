# Single Tree Worker Stage 30 Semantic Query Metadata Measurement

## Scope

Stage 30 reduces repeated work inside the semantic-token query walk without
changing request admission, edit delivery, latest-wins cancellation, worker
ownership, or response currency. For source segments of at least 32 KiB, pattern
priorities and capture roles are resolved lazily into request-local indexed
tables. Only indices that actually match are populated. Smaller hosts and
injection regions retain direct resolution, avoiding whole-query setup costs on
short inputs. The content-local line table is likewise built only when a
multiline capture actually needs random line access.

The tables are local to one `collect_host_tokens` invocation. They do not retain
cross-request state, require invalidation, or delay a newly admitted document
version. This is a compute-path optimization, not edit coalescing or debounce.

## Correctness result

Focused tests cover explicit and default pattern priorities, built-in and special
capture roles, lazy population of only requested pattern/capture indices, and the
32 KiB admission boundary. The existing semantic suite covers capture mappings,
multiline projection, overlap resolution, injection exclusion, cancellation, and
UTF-16 positions. The semantic suite and warning-denying all-target/all-feature
Clippy pass.

An attempted allocation removal in `has-ancestor?` was rejected. Initial control
measurements still showed a Markdown delta regression, and a direct comparison
of the two ancestor implementations was dominated by execution-order effects.
Stage 30 therefore retains the established `HashSet` implementation rather than
claiming an unproved win.

The benchmark reconstructs every full/delta response client-side and accepts a
sample only when the tracked token represents the latest edit. All 1,200 timed
responses passed. All 960 obsolete requests in the cancellation scenarios
returned JSON-RPC `RequestCancelled (-32800)`.

## Performance result

Reviewed Stage 29 and Stage 30 release binaries were measured end to end over
LSP with the authoritative worker and eight compute threads on an Apple M4.
Four pairs alternated binary order; each binary received six warmups and 30 timed
iterations per scenario against the same content-addressed parser/query fixture.

The median paired Stage-30 delta relative to Stage 29 was:

- Rust single-edit typing: **-11.6%**. All four pairs improved; the range was
  -25.5% to -1.1%.
- Rust eight-edit burst: **-5.6%**. Two pairs improved materially and two were
  near-flat/slightly slower; the range was -15.9% to +2.2%.
- Rust four-cancellation burst: **-12.8%**. All four pairs improved; the range
  was -16.9% to -10.8%.
- Markdown typing controls: **-0.3%** for single edits and **-0.4%** for bursts,
  with sign-changing pair results.

Cancellation-burst p95 improved in all four pairs, with a median paired delta of
-17.3%. Single-edit and eight-edit p95 results were order-sensitive and do not
support a general tail-latency claim. Markdown p95 contained isolated scheduler
outliers in both directions and is treated only as a central-latency control.

The accepted result shows that request-local query metadata reuse can improve
latest-edit follow latency without weakening semantic-token freshness. The
authoritative evidence is
[`single_worker_stage30_query_metadata_2026-07-22.json`](../../benches/profile/results/single_worker_stage30_query_metadata_2026-07-22.json),
with four separately hashed raw-sample files alongside it.
