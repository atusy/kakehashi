# Same-snapshot semantic artifact sharing controls

This run checks existing production semantic-token workloads alongside the
same-snapshot fan-out result.

## Contract

- A: `benchmark/semantic-sharing-baseline-2026-07-24` at
  `16b857a74f637e7ac20c4b503b670cd3caca1738`
- B and harness:
  `benchmark/semantic-sharing-candidate-converged-2026-07-24` at
  `d2ea0c73eddf3b59f452e94e5183af0ec3e00b97`
- Four alternating AB/BA pairs
- Six warm-up and 30 retained iterations per scenario and pair
- Production server features only
- Lower is better; percentages are paired B-versus-A deltas

The candidate is the converged Stage 3 runtime plus the #911 harness, with #911
intended to merge first. The fixture manifest contains only installed asset
paths and digests; parser libraries and query bodies are not archived.

## Result

| Scenario | Paired median | Pair range |
| --- | ---: | ---: |
| Rust cache hit | +1.26% | -1.91% to +3.04% |
| Rust typing delta | -4.31% | -10.34% to +27.51% |
| Rust typing burst | +14.30% | -6.44% to +31.13% |
| Rust cancellation burst | +2.07% | -0.96% to +58.92% |
| Sparse Rust 32 KiB minus | -3.16% | -6.41% to +1.40% |
| Sparse Rust 32 KiB exact | -2.54% | -7.20% to +5.28% |
| Sparse Rust 32 KiB plus | +3.36% | -3.50% to +10.05% |
| Sparse Rust 64 KiB | +0.42% | -4.88% to +28.52% |
| Markdown typing delta | +0.95% | -16.71% to +12.87% |
| Markdown typing burst | -1.09% | -6.04% to +3.71% |
| Markdown large cache hit | +14.00% | -5.87% to +55.18% |
| Unicode Rust cache hit | +3.82% | -0.34% to +50.20% |

Most production controls are effectively neutral, but several scenarios have
large sign-changing pair variance. In particular, Rust typing burst appeared
regressed in this collection, so the four suspicious controls were collected
again independently in the adjacent `controls-rerun` evidence. The rerun did
not reproduce a stable typing-burst regression.

Sub-millisecond cache-hit controls remain too sensitive to machine state for a
speedup or regression claim. The raw data is retained rather than discarded.

`manifest.json` attests the exact commits, production server features, harness
sources, binaries, fixture manifest, raw pair files, stdout captures, and
aggregate summary.
