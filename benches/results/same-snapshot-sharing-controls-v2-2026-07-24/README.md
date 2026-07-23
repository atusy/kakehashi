# Same-snapshot semantic artifact sharing controls

This run checks whether same-snapshot artifact sharing regresses the existing
semantic-token workloads.

## Contract

- A: `benchmark/semantic-sharing-baseline-2026-07-24` at
  `16b857a74f637e7ac20c4b503b670cd3caca1738`
- B and harness:
  `benchmark/semantic-sharing-candidate-consumer-cancel-2026-07-24` at
  `08e7deb06e5f269c8e6a16217fec7b5bbd4b87bc`
- Four alternating AB/BA pairs
- Six warm-up and 30 retained iterations per scenario and pair
- Production server features only
- Lower is better; percentages are paired B-versus-A median deltas

The fixture manifest records the installed language assets used by the run.
Parser libraries and query files are not archived in this evidence directory.

## Result

| Scenario | Paired median | Pair range |
| --- | ---: | ---: |
| Rust typing delta | -0.85% | -2.25% to +6.95% |
| Rust typing burst | -2.46% | -7.04% to -0.69% |
| Rust cancellation burst | +0.13% | -1.38% to +6.02% |
| Markdown typing delta | +0.33% | -8.75% to +13.99% |
| Markdown typing burst | -0.48% | -1.29% to +9.95% |

The cancellation burst is back to effectively neutral after cancelling an
incomplete producer when its last consumer disappears. The primary scenarios
support non-regression; the speedup claim is limited to the separately measured
same-snapshot fan-out workload.

Sub-millisecond cache-hit controls were bimodal and changed sign across AB/BA
orders. For example, Markdown large cache hit ranged from -27.63% to +110.64%,
and Unicode cache hit ranged from -39.84% to +9.15%. These unstable controls are
retained as raw evidence but are not interpreted as either a regression or a
speedup.

`manifest.json` attests the exact commits, harness sources, production server
features, binaries, fixture manifest, raw pair files, stdout captures, and
aggregate summary.
