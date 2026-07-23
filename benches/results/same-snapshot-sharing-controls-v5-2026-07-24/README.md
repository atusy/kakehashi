# Same-snapshot semantic artifact sharing controls

This run checks existing production semantic-token workloads against the final
Stage 3 candidate.

## Contract

- A: `benchmark/semantic-sharing-baseline-2026-07-24` at
  `16b857a74f637e7ac20c4b503b670cd3caca1738`
- B and harness:
  `benchmark/semantic-sharing-candidate-final-v2-2026-07-24` at
  `f77cb4a774fd5b2769cf4daa11c0db784361815c`
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
| Rust cache hit | +0.22% | -1.04% to +2.66% |
| Rust typing delta | -10.82% | -23.49% to +23.24% |
| Rust typing burst | +0.27% | -0.62% to +1.20% |
| Rust cancellation burst | -4.23% | -6.47% to +48.30% |
| Sparse Rust 32 KiB minus | -0.54% | -3.79% to +8.82% |
| Sparse Rust 32 KiB exact | -4.37% | -5.36% to +7.79% |
| Sparse Rust 32 KiB plus | +1.90% | -2.89% to +22.14% |
| Sparse Rust 64 KiB | -2.07% | -14.81% to +1.01% |
| Markdown typing delta | +4.52% | -16.90% to +27.06% |
| Markdown typing burst | +1.83% | -2.66% to +7.35% |
| Markdown large cache hit | -3.27% | -35.97% to +40.73% |
| Unicode Rust cache hit | -17.05% | -30.81% to +8.68% |

The stable controls are effectively neutral: Rust cache hit and typing burst
stay within about one to three percent. Several longer scenarios change sign
between pairs as machine state shifts; no broad control speedup is claimed.
Cancellation burst has one unusually fast first baseline pair, while the other
three pairs favor or closely match the candidate. Sub-millisecond Markdown and
Unicode cache hits remain too variable for a speedup or regression claim.

The performance claim is limited to the separately measured 29.37%
same-snapshot fan-out improvement.

`manifest.json` attests the exact commits, production server features, harness
sources, binaries, fixture manifest, raw pair files, stdout captures, and
aggregate summary.
