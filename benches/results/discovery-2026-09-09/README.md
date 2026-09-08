# Parallel large-document discovery

Across the 12 large-file scenarios, paired latency median changes ranged from -11.8% to -1.0%; 8/12 improved in all three pairs. Consistent three-pair regressions: none. Whole-session median CPU changes ranged from -2.8% to +6.3%, so shorter latency does not imply lower CPU work. Small/common-file pooled median differences ranged from -0.11 to +0.12 ms; those sizes retain serial discovery. Detailed controls and paired ranges are below. All-three-pair improvements occurred in 6/6 single-document scenarios and 2/6 four-document scenarios. Pooled large-file cycle medians were slower for rust/narrow/4 docs (+1.9%). Pooled observations and separately paired run medians can have different signs; this variability rules out a uniform speedup claim.

During guard-free snapshot population, large local/rooted injection queries can use the existing compute pool without introducing an incremental cache. Ordinary reader and cached-layer discovery remains synchronous: entering a Rayon wait while initializing a OnceLock could execute a queued reader that waits for the same initializer. Each query cursor starts at the original document root and searches one of at most two adjacent byte windows aligned to top-level children. This preserves hidden supertype ancestors. Unsupported patterns, fewer than two usable windows, and cross-window duplicate region keys use the serial full-query collector. Files below 64 KiB and callers outside a multi-worker Rayon pool also use the serial path.

There is no cross-invocation discovery state to invalidate: each populate call uses its supplied tree, text and query, and existing cancellation/generation checks govern publication. The full-query oracle exercises changed text, incremental and fresh parses, query replacement, enclosing/combined regions, text predicates and shifted boundaries. Fresh parses and query replacement exercise the collector inputs used by reopen/reload; this adds no separate end-to-end reopen/reload test.

This completes the bounded experiment in [#1068](https://github.com/atusy/kakehashi/issues/1068). It does not make discovery proportional to edit size. Whole-tree root patterns may overlap windows and pay for parallel work before falling back; trees dominated by one child may gain little. Host-token query performance remains separate work under [#895](https://github.com/atusy/kakehashi/issues/895).

Baseline: `d1b1d03010e98e58d973f8a0c253029ac668cd0f` (merged #1076). Candidate: `d6e47d066955ca3b1f0a167fbffbbbf73e058995`. Both were built with `cargo build --release --bin kakehashi`, rustc 1.97.1, on an Apple M4 with 10 logical CPUs and 32 GiB RAM. The server compute pool has 8 workers on this machine. Exact binary, harness, fixture and runtime hashes are in [provenance.json](provenance.json).

The unchanged [matrix harness](../../profile/measure_discovery.py) uses retained real Rust/Markdown parser and query assets, approximately 2 KiB / 32 KiB / 1 MiB fixtures, and a first-comment/prose character edit. Native, Lua-only (`narrow`) and wildcard configurations each run with one or four documents. Each scenario uses three alternating A/B pairs, five warmup cycles and 30 measured cycles per run. All documents are edited before their concurrent token requests; a cycle ends after every response. Controlled bridge peers measure routing/synchronization, not real analyzer or diagnostic work.

A/B logging is disabled. Each scenario has one separate logged candidate attribution run, excluded from the latency/CPU/RSS comparisons. No builds or tests ran during the matrix; ordinary desktop applications remained active. These workstation measurements are descriptive, not confidence intervals or guarantees for other grammars, hardware or workloads.

Edit-to-response cycle medians below pool 90 measured cycles per variant. The last column gives the median and range of percent changes of the three separately paired run medians. The 90 cycles are repeated observations within three sessions, not 90 independent experiments. Negative changes are improvements. All 36 scenarios and response/resource distributions are retained in [summary.json](summary.json).

| Language | Size | Mode | Docs | Baseline ms | Candidate ms | Change | Paired median (range) |
| --- | --- | --- | ---: | ---: | ---: | ---: | ---: |
| rust | large | narrow | 1 | 186.37 | 171.73 | -7.9% | -8.0% (-9.0% to -6.9%) |
| rust | large | narrow | 4 | 446.91 | 455.31 | +1.9% | -4.2% (-5.4% to +1.4%) |
| rust | large | native | 1 | 220.84 | 211.99 | -4.0% | -4.4% (-4.5% to -3.6%) |
| rust | large | native | 4 | 565.67 | 552.84 | -2.3% | -3.1% (-3.6% to -0.9%) |
| rust | large | wildcard | 1 | 204.36 | 198.59 | -2.8% | -2.7% (-3.6% to -1.8%) |
| rust | large | wildcard | 4 | 528.58 | 515.10 | -2.5% | -2.2% (-3.0% to +4.0%) |
| rust | small | narrow | 1 | 0.83 | 0.93 | +12.8% | +9.1% (+6.9% to +28.6%) |
| rust | small | narrow | 4 | 1.20 | 1.30 | +8.1% | +4.7% (-3.4% to +18.4%) |
| rust | small | native | 1 | 0.90 | 0.89 | -1.0% | +9.0% (-29.6% to +13.4%) |
| rust | small | native | 4 | 1.34 | 1.30 | -2.4% | +0.3% (-10.1% to +8.2%) |
| rust | small | wildcard | 1 | 0.93 | 0.89 | -3.7% | -6.7% (-13.3% to +2.3%) |
| rust | small | wildcard | 4 | 1.32 | 1.29 | -2.2% | -2.9% (-5.6% to -0.7%) |
| rust | common | narrow | 1 | 5.67 | 5.70 | +0.6% | +0.7% (+0.5% to +0.8%) |
| rust | common | narrow | 4 | 11.02 | 10.95 | -0.7% | -1.7% (-1.8% to +2.2%) |
| rust | common | native | 1 | 5.71 | 5.84 | +2.2% | +0.5% (+0.2% to +6.7%) |
| rust | common | native | 4 | 12.18 | 12.16 | -0.2% | -1.0% (-3.3% to +2.7%) |
| rust | common | wildcard | 1 | 5.70 | 5.72 | +0.3% | +0.4% (+0.3% to +0.6%) |
| rust | common | wildcard | 4 | 10.95 | 10.84 | -1.0% | -0.2% (-3.2% to +0.6%) |
| markdown | large | narrow | 1 | 91.78 | 81.07 | -11.7% | -11.8% (-12.0% to -10.5%) |
| markdown | large | narrow | 4 | 317.60 | 310.61 | -2.2% | -5.8% (-9.3% to +0.3%) |
| markdown | large | native | 1 | 153.05 | 147.76 | -3.5% | -3.1% (-3.9% to -2.7%) |
| markdown | large | native | 4 | 366.17 | 349.68 | -4.5% | -4.4% (-5.3% to -3.3%) |
| markdown | large | wildcard | 1 | 119.06 | 113.85 | -4.4% | -4.1% (-5.5% to -2.5%) |
| markdown | large | wildcard | 4 | 327.27 | 317.26 | -3.1% | -1.0% (-19.1% to +0.4%) |
| markdown | small | narrow | 1 | 0.56 | 0.62 | +10.6% | +4.7% (+1.2% to +33.1%) |
| markdown | small | narrow | 4 | 0.88 | 0.92 | +4.3% | +7.8% (-2.4% to +9.5%) |
| markdown | small | native | 1 | 0.65 | 0.67 | +2.6% | -0.3% (-20.0% to +24.8%) |
| markdown | small | native | 4 | 0.95 | 0.95 | -0.2% | -6.6% (-12.9% to +14.2%) |
| markdown | small | wildcard | 1 | 0.62 | 0.59 | -3.8% | +1.0% (-43.6% to +18.5%) |
| markdown | small | wildcard | 4 | 1.08 | 1.06 | -1.6% | +0.9% (-8.5% to +3.2%) |
| markdown | common | narrow | 1 | 2.74 | 2.74 | -0.1% | +0.3% (-0.6% to +0.3%) |
| markdown | common | narrow | 4 | 6.08 | 6.12 | +0.5% | +0.2% (-1.1% to +0.6%) |
| markdown | common | native | 1 | 3.47 | 3.52 | +1.5% | +1.7% (+0.0% to +2.1%) |
| markdown | common | native | 4 | 5.68 | 5.68 | +0.0% | +0.5% (-2.4% to +1.2%) |
| markdown | common | wildcard | 1 | 2.73 | 2.78 | +1.9% | +1.4% (+0.6% to +2.9%) |
| markdown | common | wildcard | 4 | 7.06 | 7.09 | +0.4% | +1.7% (-3.6% to +2.1%) |

Large-file resources: median child user+system CPU seconds for the whole session (startup, warmup, measurement and shutdown), and maximum OS-reported child RSS across the three runs. RSS is not total simultaneous process memory or per-edit allocation. CPU seconds are not phase wall time.

| Language | Mode | Docs | CPU baseline → candidate (s) | RSS baseline → candidate (MiB) |
| --- | --- | ---: | ---: | ---: |
| rust | narrow | 1 | 11.99 → 12.07 | 1161.8 → 1162.2 |
| rust | narrow | 4 | 70.83 → 72.10 | 1341.0 → 1336.9 |
| rust | native | 1 | 16.30 → 17.01 | 1161.6 → 1160.3 |
| rust | native | 4 | 106.64 → 107.54 | 1335.2 → 998.3 |
| rust | wildcard | 1 | 13.63 → 14.23 | 1182.5 → 1182.4 |
| rust | wildcard | 4 | 90.84 → 88.29 | 1376.4 → 1205.5 |
| markdown | narrow | 1 | 6.34 → 6.42 | 560.7 → 555.1 |
| markdown | narrow | 4 | 47.93 → 50.96 | 1253.3 → 1297.4 |
| markdown | native | 1 | 8.71 → 9.10 | 267.7 → 245.0 |
| markdown | native | 4 | 73.85 → 76.08 | 1141.1 → 1122.6 |
| markdown | wildcard | 1 | 8.40 → 8.91 | 561.9 → 551.7 |
| markdown | wildcard | 4 | 56.95 → 58.05 | 1096.4 → 1101.2 |

The separate collector probe used a four-worker pool with at most two windows per query and compared the original collector against the final implementation on the same parsed 1 MiB trees: Rust 32.63–34.55 ms → 21.97–22.22 ms; Markdown 19.29–19.40 ms → 11.78–11.92 ms. Complete region descriptors matched. This isolates collector latency and is not a measurement of CPU savings or end-to-end latency. Candidate phase distributions in `summary.json` are logged invocation distributions; never sum overlapping phase medians or compare them as additive request shares.

Validation: 252 runs and 18,900 successful, nonempty measured responses; minimum explicit token count 120. Both existing report consumers verified inventory, invocation/fixture/binary identity, per-document workload and bridge opens. The LSP matrix proves nonempty response coverage, not latest-edit token equivalence. The separate full-query oracle compares node identity/range, language, pattern, identity slot, include-children, combined and runtime offsets, including large public dispatch, collision fallback, incremental edits, fresh trees/query changes, hidden supertypes and cancellation tests.

An earlier, wider fan-out prototype (`5f11391d3`) was rejected after Rust large/narrow/four-document paired cycle medians regressed by 6.19%, 6.86% and 7.02%. That separate experiment was intentionally stopped after 125 complete runs; it is not a complete matrix and is not mixed into the final measurements here. Its individually validated records remain outside Git in the sibling `discovery-2026-09-09` directory. The final candidate caps each query at two windows.

Raw logs, binaries and inputs remain outside Git at the location recorded in `provenance.json`; no archive is committed or publicly uploaded. An external `artifact-manifest.json` hashes the retained files; its own digest is recorded in provenance. Input runtime assets must remain available to rerun the measurement. The existing runtime query warnings are recorded in provenance and the raw results. As with the previous report, phase JSON is derived from logs rather than independently cryptographically bound to a run.

Reproduce the matrix with the exact commits/binaries and the recorded runtime assets, then use the unchanged consumers:

```sh
python3 benches/profile/prepare_discovery.py --runtime "$runtime" --output "$inputs"
python3 benches/profile/measure_discovery.py --baseline "$baseline" --candidate "$candidate" --inputs "$inputs" --output "$results" --requests 30 --warmup 5 --repeats 3 --documents 1 4 --sizes large small common --modes narrow native wildcard
python3 benches/results/discovery-2026-09-08/summarize.py "$results" summary.json
python3 benches/results/discovery-2026-09-08/validate_retained.py "$results" validation.json
DISCOVERY_PROBE_INPUTS="$inputs" cargo test --release --lib discovery_attribution_probe -- --ignored --nocapture
```
