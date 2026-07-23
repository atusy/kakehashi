# Same-snapshot semantic artifact sharing controls

This run checks existing production semantic-token workloads against the final
Stage 3 candidate.

## Contract

- A: `benchmark/semantic-sharing-baseline-2026-07-24` at
  `16b857a74f637e7ac20c4b503b670cd3caca1738`
- B and harness: `benchmark/semantic-sharing-candidate-final-2026-07-24`
  at `abddc1306daa0b8bea48ca3868e3ccbb406fb590`
- Four alternating AB/BA pairs
- Six warm-up and 30 retained iterations per scenario and pair
- Production server features only
- Lower is better; percentages are paired B-versus-A deltas

The candidate contains the final Stage 3 runtime plus the #911 harness, with
#911 intended to merge first. The fixture manifest contains only installed
asset paths and digests; parser libraries and query bodies are not archived.

## Result

| Scenario | Paired median | Pair range |
| --- | ---: | ---: |
| Rust cache hit | +4.41% | +3.02% to +6.11% |
| Rust typing delta | -10.16% | -23.40% to +27.52% |
| Rust typing burst | +1.49% | -12.20% to +24.67% |
| Rust cancellation burst | +0.63% | -12.43% to +4.66% |
| Sparse Rust 32 KiB minus | -1.28% | -8.89% to +1.85% |
| Sparse Rust 32 KiB exact | +0.29% | -2.48% to +3.67% |
| Sparse Rust 32 KiB plus | +1.36% | -1.16% to +15.45% |
| Sparse Rust 64 KiB | +2.71% | -19.35% to +4.36% |
| Markdown typing delta | +1.74% | -18.84% to +18.66% |
| Markdown typing burst | -1.26% | -5.82% to +6.91% |
| Markdown large cache hit | +13.74% | -41.17% to +40.43% |
| Unicode Rust cache hit | +2.97% | -5.47% to +47.75% |

Typing, cancellation, and sparse controls are neutral within the observed
sign-changing variance. The sub-2ms Rust cache hit is consistently 3.02% to
6.11% slower, so the result does not claim universal non-regression: Stage 3
trades a small cache-hit overhead for the separately measured 28.86%
same-snapshot fan-out improvement. The other sub-millisecond cache-hit controls
remain too variable for a speedup or regression claim.

`manifest.json` attests the exact commits, production server features, harness
sources, binaries, fixture manifest, raw pair files, stdout captures, and
aggregate summary.
