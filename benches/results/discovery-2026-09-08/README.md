# Discovery attribution after the correctness stack

Full-tree discovery remains a material cost for large documents, but these measurements do not justify treating it as the universal first optimization. In the single-document native cases, Rust near 1 MiB spent a median 31.8 ms in discovery and 96.3 ms in host tokenization; Markdown spent 21.1 ms and 25.7 ms respectively. These are separate logged invocation distributions, not additive shares of a request or predicted optimization wins.

For the common ~32 KiB fixtures, native discovery was 0.94 ms for Rust and 0.56 ms for Markdown. Baseline edit-to-response medians were 5.46 ms and 3.49 ms. Large-file work is therefore the useful target for a bounded discovery experiment; small/common-file controls must remain in its gate.

Recommended next work: profile the host-token query cost under [#895](https://github.com/atusy/kakehashi/issues/895), and retain [#1068](https://github.com/atusy/kakehashi/issues/1068) for a separately scoped discovery experiment against a full-query oracle. Require a defined invalidation contract, unknown-query fallback, and zero mismatches before activation. This PR adds measurement only. It introduces no incremental cache, scheduler policy, or language-precedence change. Cold range requests (#935) and immutable query metadata (#936) remain separate hypotheses; this run does not measure either.

## Method and provenance

- Baseline: `73df422f9c2e16f93a411f5ae5b7c1213a19f2b7`, including the unmerged #1072/#1073 correctness fixes above integrated main. Candidate: `5fbaf20b79a272996c500fa9cf9264102c3f2f3e`, the same implementation with opt-in phase timers. This is an instrumentation comparison, not a before/after optimization experiment.
- Measurement harness: `e80b668ff49a42dd07473d014b3be2d8bd8eca19`. Every host URI carries a distinct `?kakehashi-profile-document=N` query tag, preserved by virtual injection URIs, so the controlled peer can identify the originating host without changing directory roots. Both variants use the same tagged workload.
- Both binaries: `cargo build --release --bin kakehashi`, rustc 1.97.1, Apple M4 (10 CPUs), 32 GiB RAM. Exact binary hashes, build commands, hardware and runtime provenance are in [provenance.json](provenance.json) and the archived matrix.
- Real parser/query assets were copied from the existing Neovim runtime. The clean query repository revision was `nvim-treesitter/nvim-treesitter@8b98b4470eb326f1c7b50dae79f8c963568e5720`; every copied asset is hashed. Parser libraries and query source are not committed.
- Rust sizes: 2,152 / 33,001 / 1,048,613 bytes. Markdown: 2,170 / 33,043 / 1,048,682 bytes. Rust includes ordinary/doc comments and macros; Markdown includes ordinary prose, inline markup and Lua fences. A character is toggled in the first ordinary comment/prose line, with no artificial edit delay.
- Both languages × 3 sizes × native/Lua-only/wildcard configuration × 1/4 documents = 36 scenarios. Each uses 3 alternating A/B pairs, 5 warmup cycles and 30 measured cycles per run. A separate logged candidate run follows each scenario: 252 runs total. All copies are edited before their concurrent token requests; a four-document cycle ends after all four responses.
- A/B logs are disabled. Phase timing runs are separate and excluded from A/B latency/resource comparisons. The matrix ran without our builds/tests, but ordinary desktop applications remained active. Three pairs on a workstation are descriptive evidence, not confidence intervals or a proof of zero instrumentation overhead.
- Bridge peers are controlled sync-only servers. Narrow accepts Lua injections; wildcard accepts the host and all injections. These results measure kakehashi routing/synchronization, not real analyzer CPU or diagnostic latency.

## Validation and limits

All 18,900 measured token responses were `ok`, with an explicit minimum of 120 tokens per response: no cancellation, null, error or empty responses. Both consumers verify the declared cycle/response counts and exact per-document coverage in every run, including the logged attribution runs. This proves nonempty responses, not correspondence to the latest edit or byte-for-byte token correctness.

Expected injected languages were observed for every tagged host URI in every applicable bridge run. Wildcard runs separately require the exact host URI and host language to open downstream. This is session-level coverage, including warmup; it does not establish fresh downstream work on every measured cycle. See [validation.json](validation.json).

The runner retains mixed outcomes as completed observations when each document eventually produces nonempty work. That is separate from report eligibility: both report consumers require explicit positive token counts and successful responses throughout, plus per-host bridge evidence. There are no legacy byte-size or manifest exceptions in these consumers.

Three existing query warnings occurred in every run: unsupported capture-valued `#set!` patterns in `markdown_inline/highlights.scm`, lines 48–50, 52–54 and 99–101. They concern links/images/autolinks; these fixtures contain none. The unmodified warnings and full stderr are retained, rather than deleting query patterns to obtain a silent run. No error-level server messages occurred.

This attribution does not satisfy the broader behavioral optimization gate in #895: it does not validate every token against a unique-edit oracle, exercise cancellation/bursts/reloads, or cover sparse/Unicode/mixed-priority workloads. Parser/query revisions and dense repeated fixtures limit generalization. Cross-configuration timing differences are observations, not evidence that adding a bridge speeds up parsing.

## Edit-to-response latency

Milliseconds; medians/p90 pool 90 cycles per variant. Paired deltas compare each repetition's candidate cycle median to its baseline median, then report the median and full range of those three deltas. Negative means candidate faster. Four-document values are batch latency, not per-document latency. Raw per-request distributions and status counts remain in [summary.json](summary.json).

| Language | Size | Mode | Docs | Baseline median / p90 | Candidate median / p90 | Paired delta median [min, max] |
| --- | --- | --- | ---: | ---: | ---: | ---: |
| rust | small | native | 1 | 1.07 / 1.49 | 1.02 / 1.47 | +0.6% [-26.6, +5.2] |
| rust | small | native | 4 | 1.44 / 2.19 | 1.45 / 2.23 | +0.6% [+0.5, +1.8] |
| rust | small | narrow | 1 | 1.02 / 1.43 | 1.11 / 1.48 | +8.0% [-3.5, +37.2] |
| rust | small | narrow | 4 | 1.47 / 2.20 | 1.44 / 2.23 | -3.2% [-4.9, -0.9] |
| rust | small | wildcard | 1 | 1.12 / 1.49 | 1.12 / 1.53 | +0.7% [-0.9, +3.8] |
| rust | small | wildcard | 4 | 1.43 / 2.21 | 1.49 / 2.38 | +4.0% [+0.1, +7.1] |
| rust | common | native | 1 | 5.46 / 5.87 | 5.51 / 6.15 | +1.4% [+0.9, +4.0] |
| rust | common | native | 4 | 12.01 / 13.05 | 12.15 / 12.94 | +0.6% [-0.5, +2.4] |
| rust | common | narrow | 1 | 5.52 / 5.92 | 5.53 / 5.99 | -0.9% [-1.6, +1.1] |
| rust | common | narrow | 4 | 11.01 / 11.65 | 11.10 / 11.83 | +1.0% [+0.2, +1.4] |
| rust | common | wildcard | 1 | 5.58 / 6.00 | 5.59 / 5.92 | +0.6% [-0.5, +0.9] |
| rust | common | wildcard | 4 | 10.90 / 11.70 | 10.79 / 11.58 | -1.2% [-2.8, +0.8] |
| rust | large | native | 1 | 189.17 / 191.46 | 189.59 / 191.93 | +0.3% [-0.1, +0.5] |
| rust | large | native | 4 | 418.57 / 432.85 | 416.22 / 434.09 | -1.1% [-2.9, +2.6] |
| rust | large | narrow | 1 | 188.52 / 191.32 | 187.32 / 189.47 | -0.7% [-1.0, -0.2] |
| rust | large | narrow | 4 | 383.56 / 393.95 | 382.13 / 392.11 | -0.5% [-0.9, +0.4] |
| rust | large | wildcard | 1 | 191.90 / 196.52 | 188.31 / 192.98 | -2.1% [-2.8, -1.0] |
| rust | large | wildcard | 4 | 392.67 / 432.17 | 396.86 / 418.30 | -1.2% [-3.7, +0.7] |
| markdown | small | native | 1 | 0.80 / 0.98 | 0.80 / 1.01 | +0.2% [-5.3, +0.6] |
| markdown | small | native | 4 | 1.05 / 1.34 | 1.08 / 1.44 | +2.6% [-3.1, +9.4] |
| markdown | small | narrow | 1 | 0.72 / 0.88 | 0.71 / 0.89 | -5.2% [-7.9, +9.4] |
| markdown | small | narrow | 4 | 1.10 / 1.47 | 1.03 / 1.31 | -1.6% [-25.6, +8.2] |
| markdown | small | wildcard | 1 | 0.74 / 0.95 | 0.72 / 0.92 | -2.1% [-4.6, -0.4] |
| markdown | small | wildcard | 4 | 1.31 / 1.69 | 1.23 / 1.72 | -3.9% [-8.3, -1.1] |
| markdown | common | native | 1 | 3.49 / 4.35 | 3.50 / 4.39 | -0.2% [-0.2, +0.4] |
| markdown | common | native | 4 | 5.77 / 6.39 | 5.68 / 6.22 | -3.4% [-4.5, +4.0] |
| markdown | common | narrow | 1 | 2.69 / 3.68 | 2.72 / 3.62 | +0.7% [-0.7, +3.5] |
| markdown | common | narrow | 4 | 6.11 / 6.60 | 6.20 / 6.62 | +1.2% [-1.3, +3.4] |
| markdown | common | wildcard | 1 | 2.75 / 3.80 | 2.74 / 3.86 | -0.1% [-1.2, +1.9] |
| markdown | common | wildcard | 4 | 7.24 / 8.00 | 7.18 / 8.02 | -0.9% [-1.6, +1.4] |
| markdown | large | native | 1 | 125.39 / 133.74 | 127.84 / 136.42 | +2.0% [+1.3, +3.7] |
| markdown | large | native | 4 | 233.24 / 253.91 | 228.52 / 249.98 | -2.0% [-2.3, -0.9] |
| markdown | large | narrow | 1 | 95.35 / 103.16 | 96.16 / 103.77 | +0.6% [+0.4, +1.1] |
| markdown | large | narrow | 4 | 232.11 / 245.82 | 228.73 / 239.45 | -1.5% [-1.7, -0.5] |
| markdown | large | wildcard | 1 | 98.08 / 109.07 | 97.98 / 106.54 | -1.1% [-1.2, +2.9] |
| markdown | large | wildcard | 4 | 282.95 / 679.18 | 263.71 / 593.78 | -2.1% [-3.6, +6.8] |

Small absolute differences can have large percentage changes. The candidate is faster in some pairs/scenarios and slower in others; no general speedup or exact-zero-overhead claim follows from this comparison. The full paired range remains visible for each scenario rather than being averaged across workloads.

## Phase attribution

Candidate logged invocation medians in milliseconds. Representative native cases plus large wildcard cases are shown; all 36 phase distributions, counts and incomplete outcomes are in `summary.json` and raw `.phases.json` files. A dash means the phase was not observed, not zero cost.

| Language | Size | Mode | Docs | Parse | Discovery | Resolution | Snapshot wait | Host tokens | Injection tokens | Finalize |
| --- | --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| rust | small | native | 1 | 0.01 | 0.16 | — | 0.24 | 0.45 | 0.19 | 0.09 |
| rust | common | native | 1 | 0.04 | 0.94 | — | 1.06 | 2.76 | 0.92 | 0.60 |
| rust | large | native | 1 | 2.25 | 31.84 | — | 35.90 | 96.27 | 22.24 | 24.64 |
| rust | large | native | 4 | 3.36 | 37.63 | — | 43.87 | 185.41 | 150.56 | 33.64 |
| rust | large | wildcard | 1 | 2.24 | 31.84 | 0.80 | 36.20 | 94.16 | 22.24 | 24.52 |
| rust | large | wildcard | 4 | 3.67 | 40.75 | 1.55 | 48.63 | 189.74 | 140.93 | 41.00 |
| markdown | small | native | 1 | 0.02 | 0.10 | — | 0.23 | 0.13 | 0.07 | 0.05 |
| markdown | common | native | 1 | 0.21 | 0.56 | — | 1.43 | 0.72 | 0.18 | 0.29 |
| markdown | large | native | 1 | 9.05 | 21.06 | — | 56.67 | 25.74 | 4.83 | 12.18 |
| markdown | large | native | 4 | 16.76 | 29.84 | — | 62.93 | 46.94 | 22.69 | 18.97 |
| markdown | large | wildcard | 1 | 7.86 | 21.97 | 7.03 | 41.34 | 25.49 | 4.76 | 11.67 |
| markdown | large | wildcard | 4 | 19.60 | 39.30 | 16.36 | 83.78 | 52.96 | 19.85 | 22.72 |

`parse` includes incremental-seed replay inside the parser closure; `parse_await` additionally includes pool/checkout/resumption. Discovery covers the root injection query traversal, and resolution covers the measured resolver call when bridge regions are built. Snapshot waits overlap parse/population; host and injection tokenization can overlap. Region bookkeeping outside the timed calls, serialization, queues and protocol work also cost time. Never sum these medians to reconstruct end-to-end latency or subtract them to invent an unmeasured phase.

The four-document runs demonstrate larger batch latency and phase durations under contention. They do not isolate fairness or identify a particular lock/queue as the cause; that needs a dedicated workload and queue instrumentation.

## CPU and memory

Baseline large-file runs below: median child user+system CPU seconds per complete session, and maximum OS-reported child `ru_maxrss` across the three runs, converted to MiB. These include initialization, warmup, measured work and shutdown; CPU is not the phase wall time above. RSS is an OS child-resource statistic, not allocations per edit or the sum of simultaneously live processes. Candidate and small/common results are retained in `summary.json`.

| Language | Mode | Docs | Session CPU seconds | Maximum child RSS MiB |
| --- | --- | ---: | ---: | ---: |
| rust | native | 1 | 14.59 | 1161.0 |
| rust | native | 4 | 79.63 | 1052.8 |
| rust | narrow | 1 | 12.09 | 1161.2 |
| rust | narrow | 4 | 62.28 | 1349.2 |
| rust | wildcard | 1 | 12.99 | 1182.7 |
| rust | wildcard | 4 | 69.95 | 1326.3 |
| markdown | native | 1 | 7.47 | 256.2 |
| markdown | native | 4 | 48.89 | 1133.7 |
| markdown | narrow | 1 | 6.48 | 561.9 |
| markdown | narrow | 4 | 36.10 | 1286.7 |
| markdown | wildcard | 1 | 7.18 | 567.2 |
| markdown | wildcard | 4 | 43.88 | 1083.2 |

## External evidence and reproduction

The raw measurement archive is retained outside the repository and is not distributed with this checkout. `provenance.json` records its filename, SHA-256, size and member count. It contains all per-run JSON/stderr/phase samples, `matrix.json`, generated fixtures/configs and the exact measured harness. The invocation exited successfully and persisted overall completion after final input and harness verification; both consumers require that status and the complete unique scenario inventory.

This authoritative rerun supersedes the initial measurement identified in `provenance.json`. That initial run retained aggregate bridge-language counts, which could not prove coverage for every host. The new run uses the same exact binaries and parser/query assets, with explicit token counts and tagged peer URI evidence. Its measurements replace all tables here; differences between the two runs are not an optimization comparison.

The summary, validation audit and provenance remain in the repository. Regenerating them requires a separately supplied raw archive matching the SHA-256 in `provenance.json`; there is no public download URL recorded here. Once that archive is available:

```sh
discovery_archive=/path/to/raw-results.tar.gz
mkdir /tmp/discovery-retained
tar -xzf "$discovery_archive" -C /tmp/discovery-retained
python3 benches/results/discovery-2026-09-08/summarize.py /tmp/discovery-retained/results /tmp/summary.json
python3 benches/results/discovery-2026-09-08/validate_retained.py /tmp/discovery-retained/results /tmp/validation.json
```

Regression tests use small self-contained fixtures and do not require the external archive. To collect a new matrix, follow [the harness instructions](../../profile/README.md#discovery-attribution-fixtures) with rebuilt exact binaries and a runtime whose asset hashes match the manifest. Regenerate configuration paths and retain the resulting new hashes. Keep raw outputs outside the repository. A new run is new evidence; it does not reproduce the historical timings exactly.
