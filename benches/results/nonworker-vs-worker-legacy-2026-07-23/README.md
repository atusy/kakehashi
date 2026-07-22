# Non-worker baseline versus worker legacy

This evidence set compares:

- A: `origin/main` at `a1278be5fdff24d109d9e03134c6bdb880577f64`
- B: worker stage 30 at `f3febbf4ae553f38d0e0569da7daa710fec9d0e8`
  with `KAKEHASHI_TREE_WORKER_MODE=legacy`
- harness: `b15e020f0ab2e283e2345533e6728dbacf066fff`

The collector ran four alternating pairs (`AB`, `BA`, `AB`, `BA`), with six
warmups and 30 retained samples per binary and scenario. Negative deltas mean B
was faster than A. The primary statistic is the median and range of the four
paired median deltas. Per-run p95 values in the raw files are descriptive only.

| Scenario | B vs A paired median | Pair range | A median | B median |
| --- | ---: | ---: | ---: | ---: |
| `rust_large/full_cache_hit` | -1.5% | -16.6% to +0.8% | 2.300 ms | 2.182 ms |
| `unicode_rust/full_cache_hit` | +6.7% | -11.9% to +21.2% | 0.494 ms | 0.489 ms |
| `markdown_injections_large/full_cache_hit` | -0.0% | -1.0% to +8.0% | 0.535 ms | 0.554 ms |
| `rust_large/typing_delta` | +0.2% | -10.8% to +16.3% | 29.005 ms | 27.339 ms |
| `rust_large/typing_burst` | -6.4% | -8.0% to +4.5% | 30.042 ms | 28.484 ms |
| `rust_sparse_32k_minus/typing_delta` | -41.5% | -47.1% to -35.1% | 10.860 ms | 5.959 ms |
| `rust_sparse_32k_exact/typing_delta` | -43.2% | -45.7% to -40.9% | 9.456 ms | 5.327 ms |
| `rust_sparse_32k_plus/typing_delta` | -44.7% | -46.6% to -32.3% | 9.102 ms | 4.991 ms |
| `rust_sparse_64k/typing_delta` | -38.1% | -49.3% to -30.1% | 19.900 ms | 11.556 ms |
| `markdown_injections/typing_delta` | -15.2% | -20.2% to -4.0% | 5.976 ms | 5.301 ms |
| `markdown_injections/typing_burst` | -7.7% | -18.0% to -1.5% | 5.599 ms | 5.180 ms |
| `rust_xlarge/cancel_burst` | +3.7% | +0.1% to +5.4% | 146.878 ms | 150.445 ms |

All cancellation runs retained 30 samples with zero discarded attempts, so
every obsolete request in the measured samples returned `RequestCancelled`.
Binary, harness, fixture, environment, raw-output, and summary hashes are
recorded in `manifest.json`; the read-only fixture remained unchanged across
every pair. The collector also verified the complete raw contract before
publishing the directory.

## Interpretation

The worker branch is not a uniform performance win. Its strongest repeatable
gains are the sparse query-walk controls and Markdown typing. Dense Rust typing
has strong order variance and does not support a broad win claim. Explicit
cancellation is consistently slightly slower in the worker branch. The three
`full_cache_hit` rows are sub-millisecond-to-low-millisecond cheap-path controls,
not evidence about tokenization, injection discovery, or UTF-16 conversion.

This supports extracting the stateless semantic core first, then porting and
measuring the query-walk and injection hot-path changes independently on the
non-worker architecture. Scheduler or cancellation redesign should not be
justified by this baseline alone.
