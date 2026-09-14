# Semantic refresh client latency

Ordinary edits clear refresh interest from the preceding edit. A client request waiting for the current parse can finish without a refresh-induced cancellation and replacement request. Timeout and tree-less recovery, settings invalidation, and delta baselines remain supported. Already queued recovery or non-edit workspace refreshes can still overlap later edits; workspace refresh dispatch is unchanged.

These measurements use the same two local Neovim fixes in both variants: obsolete null/error replies may only clear their own active request, and delta reconstruction retains the array matching its requested previousResultId. Unmodified Neovim can lose a large response after yielding or reconstruct a delta against a viewport-only array. This is conditional client performance evidence; it does not establish readiness with the unmodified editor.

| Workload | Baseline ms | Candidate ms | Paired median change (range) |
| --- | ---: | ---: | ---: |
| rust-large / single | 481.7 | 225.2 | -53.2% (-56.1% to -45.6%) |
| markdown-large / single | 368.5 | 136.7 | -62.8% (-65.1% to -57.2%) |
| rust-common / single | 231.5 | 150.5 | -34.9% (-35.3% to -34.1%) |
| rust-small / single | 209.8 | 149.9 | -28.5% (-29.1% to -28.3%) |
| rust-large / burst | 557.1 | 310.2 | -48.7% (-53.8% to -37.7%) |
| markdown-large / burst | 487.9 | 251.6 | -49.3% (-63.0% to -31.0%) |

Each workload uses four alternating AB/BA pairs, six warmup edits/bursts and 30 measured samples per run. A burst inserts eight leading spaces at 20 ms intervals. The metric ends when Neovim completes conversion of the current full/delta result, measured from the last edit. It does not measure a painted screen frame. Table latencies are medians of four run medians; percent changes pair those medians before aggregation. Descriptive p95 ranges and refresh/cancellation counts are in [summary.json](summary.json). Repeated samples within a run are not independent experiments.

All 48 runs validate every unique marker position and compare the complete final token-array hash across each A/B pair. No retries or discarded runs are included. Earlier v2 raw results were rejected because full-array validation exposed the viewport/delta baseline bug; they remain outside Git and are excluded here. The v3 cohort used the preceding server revision and is also excluded from this final table.

Baseline source: `17ab82bd300bf14e3b8f2c574d179d5e214a8987`. Candidate source: `d7da7e22b07bb4f1c890b1850724a3104d0fff52`. Both use `cargo build --profile profiling`; exact binary and probe hashes are in [provenance.json](provenance.json). The environment is Apple M4 / 32 GiB, macOS 26.5.1, Neovim 0.13-nightly+050fa30. Builds and tests were stopped during collection; ordinary desktop applications remained active. No CPU, allocation, multi-document fairness, or one-worker improvement is claimed.

Fixtures and parser/query assets are retained from [the discovery comparison](../discovery-2026-09-09/README.md): approximately 1 MiB Rust/Markdown, 32 KiB Rust, and 2 KiB Rust. Native configuration lists rust, markdown, markdown_inline, lua, comment, and regex with autoInstall=false and the retained runtime in searchPaths. No downstream analyzer is configured. Rust edits use zero-based line 1; Markdown uses line 8.

Reproduce using the exact binaries and retained fixtures/runtime, following [the Neovim probe instructions](../../README.md). Prepend `package.loaded["vim.lsp.semantic_tokens"] = dofile(client_module)` to the probe, where `client_module` is the retained module whose hash is recorded in provenance. Set PROBE_SAMPLES=36, PROBE_BURST=1 or 8, PROBE_INTERVAL_MS=20, and the language/line/fixture/config/binary/output variables. Run each workload in AB, BA, AB, BA order; drop six warmups, retain all remaining samples, and reject any marker failure or final-array mismatch. The complete collection scripts, client patch, raw traces, binaries, and runtime assets remain in the external artifact directory recorded in provenance; no archive is committed or uploaded.

Validation of candidate source: 3,773 unit tests passed (3 ignored), 445 E2E tests passed (1 ignored), 79 integration tests passed, and make check passed. The raw LSP harness also passed every scenario with two measured iterations and one warmup against the exact optimized candidate; those smoke timings are not comparison evidence. Neovim has 30 passing semantic-token functional tests, including regressions for both response races.

Post-measurement follow-up `d9b4dd4c5` additionally rejects obsolete-generation refresh interest after a timeout; the same generation-race test fails before that fix and all 3,773 unit tests pass afterwards. This does not change ordinary edit scheduling. The table remains tied to the exact measured source above, not to later documentation or recovery-path commits.

Follow-up `82202d3bc` ties timeout interest to its starting incarnation/content version. It also adds one snapshot-identity read before a normal wait; that additional read is not included in the table. Its edit/reopen/current-success regressions and all 3,776 unit tests pass (3 ignored), with make check passing. No exact-latest-head latency claim is made.
