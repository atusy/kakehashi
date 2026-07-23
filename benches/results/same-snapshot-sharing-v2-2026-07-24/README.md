# Same-snapshot semantic artifact sharing benchmark

This run measures the intended Stage 3 workload: concurrent semantic-token
delta and range consumers for one freshly parsed snapshot.

## Contract

- A: `benchmark/semantic-sharing-baseline-2026-07-24` at
  `16b857a74f637e7ac20c4b503b670cd3caca1738`
- B and harness:
  `benchmark/semantic-sharing-candidate-consumer-cancel-2026-07-24` at
  `08e7deb06e5f269c8e6a16217fec7b5bbd4b87bc`
- Four alternating AB/BA pairs
- Six warm-up and 30 retained iterations per pair
- Server binaries built with the isolated
  `semantic-bench-instrumentation` feature
- Lower is better; percentages are paired B-versus-A deltas

The fixture manifest records the installed language assets used by the run.
Parser libraries and query files are not archived in this evidence directory.

## Result

All four pairs improved:

| Pair | Order | A median | B median | Delta |
| ---: | :---: | ---: | ---: | ---: |
| 1 | AB | 90.642 ms | 69.456 ms | -23.37% |
| 2 | BA | 145.238 ms | 104.145 ms | -28.29% |
| 3 | AB | 147.003 ms | 102.156 ms | -30.51% |
| 4 | BA | 147.895 ms | 102.895 ms | -30.43% |

The paired median improvement is **29.36%**, with a -30.51% to -23.37%
range. This supports a speedup claim for concurrent same-snapshot consumers.
The separate control run records the effect on existing workloads.

`manifest.json` attests the exact commits, harness sources, binaries, feature
isolation, fixture manifest, raw pair files, stdout captures, and aggregate
summary.
