# Same-snapshot semantic artifact sharing benchmark

This run measures concurrent semantic-token delta and range consumers for one
freshly parsed snapshot. It is the primary performance claim for Stage 3.

## Contract

- A: `benchmark/semantic-sharing-baseline-2026-07-24` at
  `16b857a74f637e7ac20c4b503b670cd3caca1738`
- B and harness:
  `benchmark/semantic-sharing-candidate-converged-2026-07-24` at
  `d2ea0c73eddf3b59f452e94e5183af0ec3e00b97`
- Four alternating AB/BA pairs
- Six warm-up and 30 retained iterations per pair
- Server binaries built with the isolated
  `semantic-bench-instrumentation` feature
- Lower is better; percentages are paired B-versus-A deltas

The candidate tag is a synthetic benchmark ref containing the converged Stage
3 runtime plus the isolated #911 benchmark harness. The intended merge order is
#911 before this Stage 3 PR. `manifest.json` records hashes for every harness
source so the measurement remains attributable to the exact source used.

The fixture manifest records paths and digests for language assets installed by
the benchmark setup. Parser libraries and query bodies are not archived in this
evidence directory.

## Result

| Pair | Order | A median | B median | Delta |
| ---: | :---: | ---: | ---: | ---: |
| 1 | AB | 91.736 ms | 88.687 ms | -3.32% |
| 2 | BA | 158.512 ms | 103.551 ms | -34.67% |
| 3 | AB | 152.147 ms | 103.070 ms | -32.26% |
| 4 | BA | 153.047 ms | 104.623 ms | -31.64% |

The paired median improvement is **31.95%**, with a -34.67% to -3.32% range.
The first pair was much faster for both revisions than the remaining pairs, but
all four pairs improved and the final three agree closely. This supports a
speedup claim for concurrent same-snapshot consumers.

`manifest.json` attests the exact commits, harness sources, binaries, feature
isolation, fixture manifest, raw pair files, stdout captures, and aggregate
summary.
