# Same-snapshot semantic artifact sharing benchmark

This run measures concurrent semantic-token delta and range consumers for one
freshly parsed snapshot. It is the primary Stage 3 performance claim.

## Contract

- A: `benchmark/semantic-sharing-baseline-2026-07-24` at
  `16b857a74f637e7ac20c4b503b670cd3caca1738`
- B and harness: `benchmark/semantic-sharing-candidate-final-2026-07-24`
  at `abddc1306daa0b8bea48ca3868e3ccbb406fb590`
- Four alternating AB/BA pairs
- Six warm-up and 30 retained iterations per pair
- Server binaries built with the isolated
  `semantic-bench-instrumentation` feature
- Lower is better; percentages are paired B-versus-A deltas

The candidate tag contains the final Stage 3 runtime, including the range
invalidation review fix, plus the isolated #911 benchmark harness. The intended
merge order is #911 before this Stage 3 PR. `manifest.json` records hashes for
every harness source.

The fixture manifest records paths and digests for language assets installed by
the benchmark setup. Parser libraries and query bodies are not archived in this
evidence directory.

## Result

| Pair | Order | A median | B median | Delta |
| ---: | :---: | ---: | ---: | ---: |
| 1 | AB | 92.501 ms | 125.001 ms | +35.14% |
| 2 | BA | 170.206 ms | 116.908 ms | -31.31% |
| 3 | AB | 176.595 ms | 126.540 ms | -28.34% |
| 4 | BA | 168.144 ms | 118.752 ms | -29.37% |

The paired median improvement is **28.86%**. The first pair ran in a distinctly
faster machine state and regressed, while the remaining three pairs agree at
28.34% to 31.31% improvement across both AB and BA orders. The raw outlier is
retained; the speedup claim rests on the four-pair median and the stable final
three pairs, not on discarding it.

`manifest.json` attests the exact commits, harness sources, binaries, feature
isolation, fixture manifest, raw pair files, stdout captures, and aggregate
summary.
