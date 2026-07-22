# Non-worker baseline versus worker legacy

This evidence set compares:

- A: `origin/main` at `a1278be5fdff24d109d9e03134c6bdb880577f64`
- B: worker stage 30 at `f3febbf4ae553f38d0e0569da7daa710fec9d0e8`
  with `KAKEHASHI_TREE_WORKER_MODE=legacy`
- harness: `1814e7ee614112f09c65d36a1779ef0279535bfe`

The collector ran four alternating pairs (`AB`, `BA`, `AB`, `BA`), with six
warmups and 30 retained samples per binary and scenario. Negative deltas mean B
was faster than A. The primary statistic is the median and range of the four
paired median deltas. Per-run p95 values in the raw files are descriptive only.

| Scenario | B vs A paired median | Pair range | A median | B median |
| --- | ---: | ---: | ---: | ---: |
| `rust_large/full` | -7.0% | -9.3% to +1.3% | 2.265 ms | 2.088 ms |
| `unicode_rust/full` | -6.1% | -12.0% to 0.0% | 0.473 ms | 0.444 ms |
| `markdown_injections_large/full` | +3.7% | -13.4% to +17.0% | 0.533 ms | 0.533 ms |
| `rust_large/typing_delta` | +7.6% | -5.5% to +17.4% | 27.651 ms | 28.202 ms |
| `rust_large/typing_burst` | +0.3% | -4.0% to +1.7% | 28.524 ms | 28.200 ms |
| `rust_sparse_32k_minus/typing_delta` | -39.6% | -45.4% to -32.9% | 10.155 ms | 6.271 ms |
| `rust_sparse_32k_exact/typing_delta` | -46.4% | -50.5% to -44.8% | 10.008 ms | 5.227 ms |
| `rust_sparse_32k_plus/typing_delta` | -44.2% | -47.2% to -38.6% | 9.744 ms | 5.553 ms |
| `rust_sparse_64k/typing_delta` | -33.6% | -43.5% to -21.8% | 18.857 ms | 13.571 ms |
| `markdown_injections/typing_delta` | -22.3% | -30.0% to -16.5% | 6.476 ms | 4.987 ms |
| `markdown_injections/typing_burst` | -7.3% | -10.0% to -2.6% | 5.615 ms | 5.193 ms |
| `rust_xlarge/cancel_burst` | +1.1% | -0.1% to +7.4% | 149.151 ms | 149.774 ms |

All cancellation runs retained 30 samples with zero discarded attempts, so
every obsolete request in the measured samples returned `RequestCancelled`.
Binary, harness, fixture, raw-output, and summary hashes are recorded in
`manifest.json`; the fixture remained unchanged across every pair.

## Interpretation

The worker branch is not a uniform performance win. Its strongest repeatable
gains are the sparse query-walk controls and Markdown typing. Dense Rust typing
does not improve, and explicit cancellation is effectively neutral. This
supports extracting the stateless semantic core first, then porting and
measuring the query-walk and injection hot-path changes independently on the
non-worker architecture. Scheduler or cancellation redesign should not be
justified by this baseline alone.
